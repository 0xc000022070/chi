use std::path::Path as FsPath;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use http_body_util::BodyExt;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shell::{Failure, now};
use tokio::io::AsyncWriteExt;

use crate::{Builds, MAX_CHUNK, PART};

const MAX_BYTES: i64 = 1024 * 1024 * 1024;
const KEPT_PER_RUNTIME: i64 = 3;
/// A file is uploaded before the update that names it, so it is unreferenced for a moment.
const UNPUBLISHED_GRACE: i64 = 3600;
const MAX_FILES_PER_UPDATE: usize = 2000;

pub(crate) fn routes() -> Router<Builds> {
    Router::new()
        .route("/api/builds/updates", post(publish))
        .route("/api/builds/updates/missing", post(missing))
        .route("/api/builds/updates/assets/{sha256}", put(upload))
        .route("/updates/assets/{sha256}", get(asset))
        .route("/updates/{org}/{repo}", get(manifest))
}

pub(crate) fn clear_partial(dir: &FsPath) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.to_string_lossy().ends_with(PART) {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn word(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'/'))
        && !value.contains("..")
}

fn base64url(hex: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0))
        .collect();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .fold(0u32, |n, byte| (n << 8) | u32::from(*byte))
            << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

fn iso(ts: i64) -> String {
    let minute = shell::utc(ts);
    format!(
        "{}T{}:{:02}.000Z",
        &minute[..10],
        &minute[11..16],
        ts.rem_euclid(60)
    )
}

/// Returns the files to delete from disk, or `None` when `incoming` bytes cannot fit: the
/// newest update of a repo, channel and platform is never dropped.
fn make_room(conn: &Connection, incoming: i64) -> rusqlite::Result<Option<Vec<String>>> {
    conn.execute(
        "DELETE FROM updates WHERE id IN (SELECT id FROM (
           SELECT id, ROW_NUMBER() OVER (
             PARTITION BY repo_id, channel, platform, runtime ORDER BY id DESC
           ) AS position FROM updates
         ) WHERE position > ?1)",
        [KEPT_PER_RUNTIME],
    )?;
    let mut gone = Vec::new();
    loop {
        let unused: Vec<String> = conn
            .prepare(
                "DELETE FROM update_assets WHERE at < ?1
                   AND sha256 NOT IN (SELECT sha256 FROM update_files) RETURNING sha256",
            )?
            .query_map([now() - UNPUBLISHED_GRACE], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        gone.extend(unused);
        let stored: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM update_assets",
            [],
            |row| row.get(0),
        )?;
        if stored + incoming <= MAX_BYTES {
            return Ok(Some(gone));
        }
        let dropped = conn.execute(
            "DELETE FROM updates WHERE id = (SELECT MIN(id) FROM updates WHERE id NOT IN (
               SELECT MAX(id) FROM updates GROUP BY repo_id, channel, platform
             ))",
            [],
        )?;
        if dropped == 0 {
            return Ok(None);
        }
    }
}

async fn remove_files(builds: &Builds, files: Vec<String>) {
    for sha256 in files {
        let _ = tokio::fs::remove_file(builds.updates_dir.join(sha256)).await;
    }
}

#[derive(Deserialize)]
struct Missing {
    sha256: Vec<String>,
}

async fn missing(
    State(builds): State<Builds>,
    headers: HeaderMap,
    Json(asked): Json<Missing>,
) -> Result<Json<Value>, Failure> {
    builds.repo(&headers).await?;
    if asked.sha256.len() > MAX_FILES_PER_UPDATE {
        return Err(Failure::bad_request("too many files"));
    }
    let absent = builds
        .db
        .call(move |conn| {
            let mut known =
                conn.prepare("SELECT 1 FROM update_assets WHERE sha256 = ?1 AND stored = 1")?;
            let mut absent = Vec::new();
            for sha256 in asked.sha256 {
                if !known.exists([&sha256])? {
                    absent.push(sha256);
                }
            }
            Ok(absent)
        })
        .await?;
    Ok(Json(json!({ "sha256": absent })))
}

async fn receive(mut body: Body, part: &FsPath, sha256: &str) -> Result<u64, Failure> {
    let mut file = tokio::fs::File::create(part).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Failure::bad_request("upload interrupted"))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        size += data.len() as u64;
        if size > MAX_CHUNK {
            return Err(Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "file too large",
            ));
        }
        hasher.update(&data);
        file.write_all(&data).await?;
    }
    file.flush().await?;
    if shell::hex(&hasher.finalize()) != sha256 {
        return Err(Failure::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "sha256 does not match",
        ));
    }
    Ok(size)
}

fn reserve(conn: &mut Connection, sha256: &str, size: i64) -> rusqlite::Result<Option<Vec<String>>> {
    let tx = conn.transaction()?;
    if tx
        .prepare("SELECT 1 FROM update_assets WHERE sha256 = ?1")?
        .exists([sha256])?
    {
        return Ok(Some(Vec::new()));
    }
    let Some(gone) = make_room(&tx, size)? else {
        return Ok(None);
    };
    tx.execute(
        "INSERT INTO update_assets (sha256, size, at, stored) VALUES (?1, ?2, ?3, 0)",
        (sha256, size, now()),
    )?;
    tx.commit()?;
    Ok(Some(gone))
}

async fn upload(
    State(builds): State<Builds>,
    Path(sha256): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, Failure> {
    builds.repo(&headers).await?;
    if !is_sha256(&sha256) {
        return Err(Failure::bad_request("the name is the lowercase sha256"));
    }
    let _one_at_a_time = builds.uploading.lock().await;
    let part = builds.updates_dir.join(format!("{sha256}{PART}"));
    let size = match receive(body, &part, &sha256).await {
        Ok(size) => size,
        Err(failure) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(failure);
        }
    };

    let name = sha256.clone();
    let room = builds
        .db
        .call(move |conn| reserve(conn, &name, size as i64))
        .await?;
    let Some(gone) = room else {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(Failure::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "updates are full: the newest update of every channel already fills the cap",
        ));
    };
    remove_files(&builds, gone).await;
    tokio::fs::rename(&part, builds.updates_dir.join(&sha256)).await?;
    let name = sha256;
    builds
        .db
        .call(move |conn| {
            conn.execute(
                "UPDATE update_assets SET stored = 1 WHERE sha256 = ?1",
                [name],
            )
        })
        .await?;
    Ok(Json(json!({ "size": size })))
}

#[derive(Deserialize)]
struct File {
    sha256: String,
    key: String,
    #[serde(rename = "contentType")]
    content_type: String,
    #[serde(default, rename = "fileExtension")]
    extension: String,
}

impl File {
    fn check(&self) -> bool {
        is_sha256(&self.sha256)
            && word(&self.key, 100)
            && !self.key.contains('/')
            && word(&self.content_type, 100)
            && (self.extension.is_empty()
                || (self.extension.starts_with('.')
                    && !self.extension.contains('/')
                    && word(&self.extension, 20)))
    }

    fn manifest(&self, builds: &Builds) -> Value {
        let mut entry = json!({
            "hash": base64url(&self.sha256),
            "key": self.key,
            "contentType": self.content_type,
            "url": builds.shell.url(&format!("/updates/assets/{}", self.sha256)),
        });
        if !self.extension.is_empty() {
            entry["fileExtension"] = json!(self.extension);
        }
        entry
    }
}

#[derive(Deserialize)]
struct NewUpdate {
    channel: String,
    platform: String,
    #[serde(rename = "runtimeVersion")]
    runtime: String,
    #[serde(default)]
    commit: String,
    #[serde(rename = "launchAsset")]
    launch: File,
    #[serde(default)]
    assets: Vec<File>,
    #[serde(default)]
    extra: Value,
}

impl NewUpdate {
    fn check(&self) -> bool {
        word(&self.channel, 60)
            && ["android", "ios"].contains(&self.platform.as_str())
            && word(&self.runtime, 200)
            && self.commit.len() <= 200
            && self.assets.len() < MAX_FILES_PER_UPDATE
            && self.launch.check()
            && self.assets.iter().all(File::check)
    }

    fn files(&self) -> Vec<String> {
        let mut files: Vec<String> = self.assets.iter().map(|file| file.sha256.clone()).collect();
        files.push(self.launch.sha256.clone());
        files.sort();
        files.dedup();
        files
    }

    fn id(&self, files: &[String]) -> String {
        let digest = Sha256::digest(
            format!(
                "{}\n{}\n{}\n{}\n{}",
                self.platform,
                self.runtime,
                self.channel,
                self.launch.sha256,
                files.join("\n")
            )
            .as_bytes(),
        );
        let hex = shell::hex(&digest[..16]);
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    }
}

struct Stored {
    repo_id: i64,
    org: String,
    repo: String,
    channel: String,
    platform: String,
    runtime: String,
    commit: String,
    manifest: String,
    at: i64,
    files: Vec<String>,
}

fn store(conn: &mut Connection, update: Stored) -> rusqlite::Result<Result<Vec<String>, String>> {
    let tx = conn.transaction()?;
    let mut known = tx.prepare("SELECT 1 FROM update_assets WHERE sha256 = ?1 AND stored = 1")?;
    for sha256 in &update.files {
        if !known.exists([sha256])? {
            return Ok(Err(format!("{sha256}: never uploaded")));
        }
    }
    drop(known);
    tx.execute(
        "INSERT INTO updates (repo_id, org, repo, channel, platform, runtime, commit_sha, manifest, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        (
            update.repo_id,
            update.org,
            update.repo,
            update.channel,
            update.platform,
            update.runtime,
            update.commit,
            update.manifest,
            update.at,
        ),
    )?;
    let id = tx.last_insert_rowid();
    for sha256 in &update.files {
        tx.execute(
            "INSERT INTO update_files (update_id, sha256) VALUES (?1, ?2)",
            (id, sha256),
        )?;
    }
    let gone = make_room(&tx, 0)?.unwrap_or_default();
    tx.commit()?;
    Ok(Ok(gone))
}

async fn publish(
    State(builds): State<Builds>,
    headers: HeaderMap,
    Json(new): Json<NewUpdate>,
) -> Result<Json<Value>, Failure> {
    let repo = builds.repo(&headers).await?;
    if !new.check() {
        return Err(Failure::bad_request(
            "channel, platform (android or ios), runtimeVersion and every file's sha256, key and contentType are required",
        ));
    }
    let (org, _) = builds
        .shell
        .org_address(repo.org_id)
        .await?
        .ok_or_else(Failure::not_found)?;

    let files = new.files();
    let id = new.id(&files);
    let at = now();
    let manifest = json!({
        "id": id,
        "createdAt": iso(at),
        "runtimeVersion": new.runtime,
        "launchAsset": new.launch.manifest(&builds),
        "assets": new.assets.iter().map(|file| file.manifest(&builds)).collect::<Vec<_>>(),
        "metadata": {},
        "extra": new.extra,
    })
    .to_string();
    let update = Stored {
        repo_id: repo.id,
        org,
        repo: repo.name,
        channel: new.channel,
        platform: new.platform,
        runtime: new.runtime,
        commit: new.commit,
        manifest,
        at,
        files,
    };
    let gone = builds
        .db
        .call(move |conn| store(conn, update))
        .await?
        .map_err(|message| Failure(StatusCode::UNPROCESSABLE_ENTITY, message))?;
    remove_files(&builds, gone).await;
    Ok(Json(json!({ "id": id })))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

async fn manifest(
    State(builds): State<Builds>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let platform = header_value(&headers, "expo-platform").to_owned();
    let runtime = header_value(&headers, "expo-runtime-version").to_owned();
    let channel = header_value(&headers, "expo-channel-name").to_owned();
    let newest: Option<String> = builds
        .db
        .call(move |conn| {
            conn.query_row(
                "SELECT manifest FROM updates
                 WHERE org = ?1 AND repo = ?2 AND channel = ?3 AND platform = ?4 AND runtime = ?5
                 ORDER BY id DESC LIMIT 1",
                (org, repo, channel, platform, runtime),
                |row| row.get(0),
            )
            .optional()
        })
        .await?;
    let common = [
        ("expo-protocol-version", "1"),
        ("expo-sfv-version", "0"),
        ("cache-control", "private, max-age=0"),
    ];
    Ok(match newest {
        Some(manifest) => (
            common,
            [(header::CONTENT_TYPE, "application/expo+json")],
            manifest,
        )
            .into_response(),
        None => (StatusCode::NO_CONTENT, common).into_response(),
    })
}

async fn asset(
    State(builds): State<Builds>,
    Path(sha256): Path<String>,
) -> Result<Response, Failure> {
    if !is_sha256(&sha256) {
        return Err(Failure::not_found());
    }
    let file = tokio::fs::File::open(builds.updates_dir.join(&sha256))
        .await
        .map_err(|_| Failure::not_found())?;
    let size = file.metadata().await?.len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_owned(),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn
    }

    fn add(conn: &Connection, runtime: &str, size: i64) -> i64 {
        conn.execute(
            "INSERT INTO updates (repo_id, org, repo, channel, platform, runtime, commit_sha, manifest, at)
             VALUES (1, 'cc', 'app', 'staging', 'android', ?1, '', '{}', 0)",
            [runtime],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let sha256 = format!("{id:064}");
        conn.execute(
            "INSERT INTO update_assets (sha256, size, at, stored) VALUES (?1, ?2, 0, 1)",
            (&sha256, size),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO update_files (update_id, sha256) VALUES (?1, ?2)",
            (id, &sha256),
        )
        .unwrap();
        id
    }

    fn ids(conn: &Connection) -> Vec<i64> {
        conn.prepare("SELECT id FROM updates ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_runtime_keeps_only_its_newest_updates() {
        let conn = store();
        for _ in 0..KEPT_PER_RUNTIME + 2 {
            add(&conn, "a", 1);
        }
        let other = add(&conn, "b", 1);
        let gone = make_room(&conn, 0).unwrap().unwrap();
        assert_eq!(gone.len(), 2);
        assert_eq!(ids(&conn), [3, 4, 5, other]);
    }

    #[test]
    fn the_cap_evicts_the_oldest_updates_but_never_the_newest_of_a_channel() {
        let conn = store();
        let third = MAX_BYTES / 3;
        let old_runtime = add(&conn, "a", third);
        let older = add(&conn, "b", third);
        let newest = add(&conn, "b", third);

        let gone = make_room(&conn, third).unwrap().unwrap();
        assert_eq!(gone, [format!("{old_runtime:064}")]);
        assert_eq!(ids(&conn), [older, newest]);

        assert_eq!(make_room(&conn, 2 * third).unwrap().map(|gone| gone.len()), Some(1));
        assert_eq!(ids(&conn), [newest]);
        assert!(make_room(&conn, MAX_BYTES).unwrap().is_none());
        assert_eq!(ids(&conn), [newest]);
    }

    #[test]
    fn a_file_waits_for_its_update_before_it_counts_as_unused() {
        let conn = store();
        conn.execute(
            "INSERT INTO update_assets (sha256, size, at, stored) VALUES ('fresh', 1, ?1, 1), ('stale', 1, 0, 1)",
            [now()],
        )
        .unwrap();
        assert_eq!(make_room(&conn, 0).unwrap().unwrap(), ["stale"]);
    }

    #[test]
    fn hashes_and_dates_take_the_form_the_client_parses() {
        assert_eq!(
            base64url("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
        );
        assert_eq!(iso(1_795_100_007), "2026-11-19T14:53:27.000Z");
        assert!(is_sha256(&"a".repeat(64)) && !is_sha256(&"A".repeat(64)) && !is_sha256("../x"));
    }
}
