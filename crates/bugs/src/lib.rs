mod envelope;
mod ui;
mod writer;

use std::{
    collections::HashMap,
    io::Read,
    path::Path as FsPath,
    sync::{Arc, RwLock},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use flate2::read::{GzDecoder, ZlibDecoder};
use rusqlite::Connection;
use serde_json::{Value, json};
use shell::{Db, Failure, Limiter, Shell, client_ip};
use tokio::sync::mpsc;

const MAX_BODY: usize = 1024 * 1024;
const MAX_INFLATED: u64 = 4 * 1024 * 1024;
const MAX_EVENTS_PER_REQUEST: usize = 4;
const QUEUE: usize = 1024;
const BACKOFF_SECONDS: &str = "60";
const ALERTS_PER_HOUR: f64 = 12.0;

struct Incoming {
    project_id: i64,
    event: Value,
}

#[derive(Clone)]
pub struct Bugs {
    db: Db,
    shell: Shell,
    keys: Arc<RwLock<HashMap<String, i64>>>,
    queue: mpsc::Sender<Incoming>,
    per_ip: Arc<Limiter>,
    per_project: Arc<Limiter>,
}

impl Bugs {
    pub fn open(data: &FsPath, shell: Shell) -> Result<Self, String> {
        let path = data.join("bugs.db");
        let db = Db::open(&path, include_str!("schema.sql")).map_err(|e| e.to_string())?;
        let mut writer = Connection::open(&path).map_err(|e| e.to_string())?;
        writer.pragma_update(None, "foreign_keys", "ON").map_err(|e| e.to_string())?;
        writer.pragma_update(None, "cache_size", -256).map_err(|e| e.to_string())?;
        writer.busy_timeout(std::time::Duration::from_secs(10)).map_err(|e| e.to_string())?;
        writer::add_issue_environment(&writer).map_err(|e| e.to_string())?;
        let keys = writer::keys(&writer).map_err(|e| e.to_string())?;

        let (queue, mut incoming) = mpsc::channel::<Incoming>(QUEUE);
        let (alerts, mut pending_alerts) = mpsc::channel::<writer::Alert>(64);
        std::thread::spawn(move || {
            let mut pruned_at = 0;
            let mut batch = Vec::new();
            while incoming.blocking_recv_many(&mut batch, 64) > 0 {
                let now = shell::now();
                match writer::write(&mut writer, &batch, now) {
                    Ok(found) => found.into_iter().for_each(|alert| drop(alerts.try_send(alert))),
                    Err(error) => eprintln!("bugs: write failed: {error}"),
                }
                batch.clear();
                if now - pruned_at > 3600 {
                    pruned_at = now;
                    if let Err(error) = writer::prune(&writer, now) {
                        eprintln!("bugs: prune failed: {error}");
                    }
                }
            }
        });

        let alerter = shell.clone();
        tokio::spawn(async move {
            let throttle = Limiter::new(ALERTS_PER_HOUR / 3600.0, ALERTS_PER_HOUR);
            while let Some(alert) = pending_alerts.recv().await {
                if !throttle.allow(&alert.project_id.to_string()) {
                    continue;
                }
                let Ok(Some((org, home))) = alerter.org_address(alert.org_id).await else { continue };
                alerter
                    .run_hooks(&json!({
                        "type": alert.kind,
                        "org": org,
                        "project": alert.project,
                        "title": alert.title,
                        "culprit": alert.culprit,
                        "level": alert.level,
                        "url": format!("{home}/bugs/{}", alert.issue_id),
                    }))
                    .await;
            }
        });

        Ok(Self {
            db,
            shell,
            keys: Arc::new(RwLock::new(keys)),
            queue,
            per_ip: Arc::new(Limiter::new(5.0, 100.0)),
            per_project: Arc::new(Limiter::new(10.0, 300.0)),
        })
    }

    pub fn ingest(&self) -> Router {
        Router::new()
            .route("/api/{project_id}/envelope/", post(ingest))
            .route("/api/{project_id}/envelope", post(ingest))
            .route("/api/{project_id}/store/", post(ingest))
            .with_state(self.clone())
    }

    pub fn ui(&self) -> Router {
        Router::new().nest("/bugs", ui::routes()).with_state(self.clone())
    }
}

fn backoff() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            (header::RETRY_AFTER, HeaderValue::from_static(BACKOFF_SECONDS)),
            (header::HeaderName::from_static("x-sentry-rate-limits"), HeaderValue::from_static("60::organization")),
        ],
    )
        .into_response()
}

fn sentry_key<'a>(headers: &'a HeaderMap, query: &'a HashMap<String, String>) -> Option<&'a str> {
    let from_header = || {
        headers
            .get("x-sentry-auth")?
            .to_str()
            .ok()?
            .split([',', ' '])
            .find_map(|part| part.strip_prefix("sentry_key="))
    };
    query.get("sentry_key").map(String::as_str).or_else(from_header)
}

fn inflate(headers: &HeaderMap, body: &[u8]) -> Option<Vec<u8>> {
    let encoding = headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("");
    let mut inflated = Vec::new();
    let read = if encoding == "gzip" || body.starts_with(&[0x1f, 0x8b]) {
        GzDecoder::new(body).take(MAX_INFLATED + 1).read_to_end(&mut inflated)
    } else if encoding == "deflate" {
        ZlibDecoder::new(body).take(MAX_INFLATED + 1).read_to_end(&mut inflated)
    } else {
        return Some(body.to_vec());
    };
    (read.is_ok() && inflated.len() as u64 <= MAX_INFLATED).then_some(inflated)
}

async fn ingest(
    State(bugs): State<Bugs>,
    Path(project_id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Body,
) -> Result<Response, Failure> {
    if !bugs.per_ip.allow(&client_ip(&headers)) {
        return Ok(backoff());
    }
    let known = sentry_key(&headers, &query).is_some_and(|key| bugs.keys.read().expect("no holder of the key cache panics").get(key) == Some(&project_id));
    if !known {
        return Err(Failure::new(StatusCode::UNAUTHORIZED, "unknown project or key"));
    }
    if !bugs.per_project.allow(&project_id.to_string()) {
        return Ok(backoff());
    }
    let body = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Failure::new(StatusCode::PAYLOAD_TOO_LARGE, "body too large"))?;
    let body = inflate(&headers, &body).ok_or_else(|| Failure::bad_request("bad or oversized compressed body"))?;

    let (id, events) = if uri.path().contains("/store") {
        let event: Value = serde_json::from_slice(&body).map_err(|_| Failure::bad_request("malformed event"))?;
        (event["event_id"].clone(), vec![event])
    } else {
        let envelope = envelope::parse(&body, MAX_EVENTS_PER_REQUEST).ok_or_else(|| Failure::bad_request("malformed envelope"))?;
        (envelope.header["event_id"].clone(), envelope.events)
    };
    for event in events {
        if bugs.queue.try_send(Incoming { project_id, event }).is_err() {
            return Ok(backoff());
        }
    }
    Ok(Json(json!({ "id": id })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    #[test]
    fn key_comes_from_query_or_auth_header_only() {
        let mut headers = HeaderMap::new();
        let mut query = HashMap::new();
        assert_eq!(sentry_key(&headers, &query), None);
        headers.insert("x-sentry-auth", "Sentry sentry_version=7, sentry_key=abc, sentry_client=x".parse().unwrap());
        assert_eq!(sentry_key(&headers, &query), Some("abc"));
        query.insert("sentry_key".to_owned(), "def".to_owned());
        assert_eq!(sentry_key(&headers, &query), Some("def"));
    }

    #[test]
    fn inflate_refuses_bombs_and_passes_plain_bodies() {
        let headers = HeaderMap::new();
        assert_eq!(inflate(&headers, b"{}").as_deref(), Some(&b"{}"[..]));
        let gzip = |data: &[u8]| {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        };
        assert_eq!(inflate(&headers, &gzip(b"hello")).as_deref(), Some(&b"hello"[..]));
        let bomb = gzip(&vec![0u8; MAX_INFLATED as usize + 1]);
        assert!(bomb.len() < MAX_BODY);
        assert!(inflate(&headers, &bomb).is_none());
    }
}
