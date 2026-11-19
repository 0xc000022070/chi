mod client;
mod pipeline;
mod ui;
mod updates;

use std::{
    collections::HashMap,
    io::Read,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use http_body_util::BodyExt;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shell::{Db, Failure, Limiter, Repo, Shell, client_ip, now};
use tokio::io::AsyncWriteExt;

pub use client::push;

const MAX_CHUNK: u64 = 90 * 1024 * 1024;
const MAX_ARTIFACT: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARTIFACTS_PER_BUILD: usize = 10;
const KEPT_BUILDS_WITH_FILES: i64 = 20;
const STALE_UPLOAD: i64 = 86_400;
const PART: &str = ".part";

#[derive(Clone)]
pub struct Builds {
    db: Db,
    shell: Shell,
    dir: Arc<PathBuf>,
    updates_dir: Arc<PathBuf>,
    uploading: Arc<tokio::sync::Mutex<()>>,
    per_ip: Arc<Limiter>,
    runner: Arc<pipeline::Runner>,
    agents: agents::Agents,
}

#[derive(Clone)]
struct Build {
    id: i64,
    org_id: i64,
    repo_id: i64,
    version: String,
    commit: String,
    branch: String,
    tag: String,
    builder: String,
    status: String,
    started_at: i64,
    finished_at: Option<i64>,
}

const BUILD_COLUMNS: &str = "id, org_id, repo_id, version, commit_sha, branch, tag, builder, status, started_at, finished_at";

fn build_row(row: &rusqlite::Row) -> rusqlite::Result<Build> {
    Ok(Build {
        id: row.get(0)?,
        org_id: row.get(1)?,
        repo_id: row.get(2)?,
        version: row.get(3)?,
        commit: row.get(4)?,
        branch: row.get(5)?,
        tag: row.get(6)?,
        builder: row.get(7)?,
        status: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

struct Artifact {
    name: String,
    size: i64,
    sha256: String,
}

fn artifacts(conn: &Connection, build_id: i64) -> rusqlite::Result<Vec<Artifact>> {
    conn.prepare("SELECT name, size, sha256 FROM artifacts WHERE build_id = ?1 ORDER BY name")?
        .query_map([build_id], |row| {
            Ok(Artifact {
                name: row.get(0)?,
                size: row.get(1)?,
                sha256: row.get(2)?,
            })
        })?
        .collect()
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && !name.starts_with('.')
        && !name.ends_with(PART)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn file_sha256(path: &FsPath) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(shell::hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn expire(conn: &Connection, repo_id: i64, tag: &str) -> rusqlite::Result<Vec<i64>> {
    let old: Vec<i64> = conn
        .prepare(
            "SELECT id FROM builds WHERE repo_id = ?1 AND tag = ?2 AND status = 'finished'
             ORDER BY id DESC LIMIT -1 OFFSET ?3",
        )?
        .query_map((repo_id, tag, KEPT_BUILDS_WITH_FILES), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for build_id in &old {
        conn.execute("DELETE FROM artifacts WHERE build_id = ?1", [build_id])?;
    }
    Ok(old)
}

impl Builds {
    pub fn open(data: &FsPath, shell: Shell, agents: agents::Agents) -> Result<Self, String> {
        let dir = data.join("builds");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let updates_dir = data.join("updates");
        updates::clear_partial(&updates_dir).map_err(|e| e.to_string())?;
        let db = Db::open(&data.join("builds.db"), include_str!("schema.sql"))
            .map_err(|e| e.to_string())?;
        let builds = Self {
            db,
            shell,
            dir: Arc::new(dir),
            updates_dir: Arc::new(updates_dir),
            uploading: Arc::default(),
            per_ip: Arc::new(Limiter::new(2.0, 60.0)),
            runner: Arc::new(pipeline::Runner::new(data)),
            agents,
        };
        let sweeper = builds.clone();
        tokio::spawn(async move {
            if let Err(Failure(_, message)) = sweeper.sweep_runs().await {
                eprintln!("builds: could not fail interrupted pipeline runs: {message}");
            }
        });
        Ok(builds)
    }

    pub fn api(&self) -> Router {
        Router::new()
            .route("/api/builds", post(create_build))
            .route(
                "/api/builds/{id}/artifacts/{name}",
                put(upload).get(api_download),
            )
            .route("/api/builds/{id}/finish", post(finish))
            .route("/api/builds/latest", get(latest))
            .route("/api/builds/base", get(base))
            .route("/app.apk", get(app))
            .route("/app.version", get(app_version))
            .merge(updates::routes())
            .with_state(self.clone())
    }

    pub fn ui(&self) -> Router {
        shell::mount("/builds", ui::routes(), self.clone())
    }

    fn build_dir(&self, build_id: i64) -> PathBuf {
        self.dir.join(build_id.to_string())
    }

    async fn repo(&self, headers: &HeaderMap) -> Result<Repo, Failure> {
        if !self.per_ip.allow(&client_ip(headers)) {
            return Err(Failure::new(StatusCode::TOO_MANY_REQUESTS, "slow down"));
        }
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        self.shell
            .repo_by_token(token)
            .await?
            .ok_or_else(|| Failure::new(StatusCode::UNAUTHORIZED, "unknown upload token"))
    }

    async fn build(&self, id: i64) -> Result<Option<Build>, Failure> {
        self.db
            .call(move |conn| {
                conn.query_row(
                    &format!("SELECT {BUILD_COLUMNS} FROM builds WHERE id = ?1"),
                    [id],
                    build_row,
                )
                .optional()
            })
            .await
    }

    async fn own_build(&self, repo: &Repo, id: i64) -> Result<Build, Failure> {
        self.build(id)
            .await?
            .filter(|build| build.repo_id == repo.id)
            .ok_or_else(Failure::not_found)
    }

    async fn serve(
        &self,
        build_id: i64,
        name: &str,
        who: String,
        ip: String,
    ) -> Result<Response, Failure> {
        if !safe_name(name) {
            return Err(Failure::not_found());
        }
        let file = tokio::fs::File::open(self.build_dir(build_id).join(name))
            .await
            .map_err(|_| Failure::not_found())?;
        let size = file.metadata().await?.len();
        let logged = name.to_owned();
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO downloads (build_id, name, who, ip, at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    (build_id, logged, who, ip, now()),
                )
            })
            .await?;
        let kind = if name.ends_with(".apk") {
            "application/vnd.android.package-archive"
        } else {
            "application/octet-stream"
        };
        Ok((
            [
                (header::CONTENT_TYPE, kind.to_owned()),
                (header::CONTENT_LENGTH, size.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
                (header::CACHE_CONTROL, "private, no-store".to_owned()),
            ],
            Body::from_stream(tokio_util::io::ReaderStream::new(file)),
        )
            .into_response())
    }

    async fn deliver(&self, build_id: i64) -> Result<(), Failure> {
        let Some(build) = self.build(build_id).await? else {
            return Ok(());
        };
        let files = self.db.call(move |conn| artifacts(conn, build_id)).await?;
        let Some((org, home)) = self.shell.org_address(build.org_id).await? else {
            return Ok(());
        };
        let repo = self
            .shell
            .repo(build.org_id, build.repo_id)
            .await?
            .map(|repo| repo.name)
            .unwrap_or_default();
        let dir = self.build_dir(build_id);
        let event = json!({
            "type": format!("build.{}", build.status),
            "org": org,
            "repo": repo,
            "build_id": build.id,
            "version": build.version,
            "commit": build.commit,
            "branch": build.branch,
            "tag": build.tag,
            "builder": build.builder,
            "url": format!("{home}/builds/{build_id}"),
            "artifacts": files.iter().map(|file| json!({
                "name": file.name,
                "size": file.size,
                "sha256": file.sha256,
                "path": dir.join(&file.name),
            })).collect::<Vec<_>>(),
        });
        let results = self.shell.run_hooks(&event).await;
        self.db
            .call(move |conn| {
                for result in &results {
                    conn.execute(
                        "INSERT INTO deliveries (build_id, target, ok, output, at) VALUES (?1, ?2, ?3, ?4, ?5)",
                        (build_id, &result.hook, result.ok, &result.output, now()),
                    )?;
                }
                Ok(())
            })
            .await
    }

    fn deliver_in_background(&self, build_id: i64) {
        let builds = self.clone();
        tokio::spawn(async move {
            if let Err(Failure(_, message)) = builds.pipeline(build_id).await {
                eprintln!("builds: pipeline of build {build_id} failed: {message}");
            }
            if let Err(Failure(_, message)) = builds.deliver(build_id).await {
                eprintln!("builds: delivery of build {build_id} failed: {message}");
            }
        });
    }
}

#[derive(Deserialize)]
struct NewBuild {
    version: String,
    #[serde(default)]
    commit: String,
    #[serde(default)]
    branch: String,
    #[serde(default = "prerelease")]
    tag: String,
    #[serde(default)]
    builder: String,
    #[serde(default)]
    failed: bool,
    #[serde(default)]
    base: String,
    #[serde(default)]
    commits: Vec<Value>,
    #[serde(default)]
    diffstat: String,
    #[serde(default)]
    diff: String,
}

fn prerelease() -> String {
    "prerelease".to_owned()
}

impl NewBuild {
    fn check(&mut self) -> Result<String, Failure> {
        let fields = [
            &self.version,
            &self.commit,
            &self.branch,
            &self.tag,
            &self.builder,
        ];
        if self.version.is_empty() || fields.iter().any(|field| field.len() > 200) {
            return Err(Failure::bad_request(
                "version is required and every field is at most 200 bytes",
            ));
        }
        if !["prerelease", "release"].contains(&self.tag.as_str()) {
            return Err(Failure::bad_request("tag is prerelease or release"));
        }
        let commits = Value::from(std::mem::take(&mut self.commits)).to_string();
        if self.base.len() > 200
            || commits.len() > 2 * client::MAX_DIFF
            || self.diffstat.len() > client::MAX_DIFF
            || self.diff.len() > client::MAX_DIFF + 100
        {
            return Err(Failure::bad_request(
                "the commits, diffstat or diff are too large",
            ));
        }
        Ok(commits)
    }
}

async fn create_build(
    State(builds): State<Builds>,
    headers: HeaderMap,
    Json(mut new): Json<NewBuild>,
) -> Result<Json<Value>, Failure> {
    let repo = builds.repo(&headers).await?;
    let commits = new.check()?;
    let failed = new.failed;
    let (id, stale) = builds
        .db
        .call(move |conn| {
            let stale: Vec<i64> = conn
                .prepare("SELECT id FROM builds WHERE status = 'uploading' AND started_at < ?1")?
                .query_map([now() - STALE_UPLOAD], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            conn.execute(
                "UPDATE builds SET status = 'failed', finished_at = ?1 WHERE status = 'uploading' AND started_at < ?2",
                (now(), now() - STALE_UPLOAD),
            )?;
            conn.execute(
                "INSERT INTO builds (org_id, repo_id, version, commit_sha, branch, tag, builder, status, started_at, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                (
                    repo.org_id,
                    repo.id,
                    new.version,
                    new.commit,
                    new.branch,
                    new.tag,
                    new.builder,
                    if failed { "failed" } else { "uploading" },
                    now(),
                    failed.then(now),
                ),
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO contexts (build_id, base, commits, diffstat, diff) VALUES (?1, ?2, ?3, ?4, ?5)",
                (id, new.base, commits, new.diffstat, new.diff),
            )?;
            Ok((id, stale))
        })
        .await?;
    for build_id in stale {
        let _ = tokio::fs::remove_dir_all(builds.build_dir(build_id)).await;
    }
    if failed {
        builds.deliver_in_background(id);
    }
    builds.shell.publish(repo.org_id, "builds");
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct Offset {
    #[serde(default)]
    offset: u64,
}

/// Appends one chunk. A chunk is accepted only at the current end of the file, so
/// a client that lost its place gets `409` with the size received and resumes there.
async fn upload(
    State(builds): State<Builds>,
    Path((id, name)): Path<(i64, String)>,
    Query(Offset { offset }): Query<Offset>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Failure> {
    let repo = builds.repo(&headers).await?;
    let build = builds.own_build(&repo, id).await?;
    if build.status != "uploading" || !safe_name(&name) {
        return Err(Failure::bad_request(
            "build is closed or the artifact name is not allowed",
        ));
    }
    let _one_at_a_time = builds.uploading.lock().await;
    let dir = builds.build_dir(id);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{name}{PART}"));
    let received = tokio::fs::metadata(&path)
        .await
        .map_or(0, |meta| meta.len());
    if received == 0 && artifact_count(&dir).await? >= MAX_ARTIFACTS_PER_BUILD {
        return Err(Failure::bad_request("too many artifacts in this build"));
    }
    if offset != received {
        return Ok((StatusCode::CONFLICT, Json(json!({ "received": received }))).into_response());
    }
    let written = append(&path, body, received).await?;
    Ok(Json(json!({ "received": received + written })).into_response())
}

async fn artifact_count(dir: &std::path::Path) -> std::io::Result<usize> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut count = 0;
    while entries.next_entry().await?.is_some() {
        count += 1;
    }
    Ok(count)
}

async fn append(path: &std::path::Path, mut body: Body, received: u64) -> Result<u64, Failure> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut written = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Failure::bad_request("upload interrupted"))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        written += data.len() as u64;
        if written > MAX_CHUNK || received + written > MAX_ARTIFACT {
            file.set_len(received).await?;
            return Err(Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "chunk or artifact too large",
            ));
        }
        file.write_all(&data).await?;
    }
    file.flush().await?;
    Ok(written)
}

#[derive(Deserialize)]
struct Finish {
    artifacts: HashMap<String, String>,
}

fn verify(
    dir: &std::path::Path,
    expected: &HashMap<String, String>,
) -> std::io::Result<Result<Vec<Artifact>, String>> {
    let mut files = Vec::new();
    for (name, expected) in expected {
        if !safe_name(name) {
            return Ok(Err(format!("{name}: name not allowed")));
        }
        let part = dir.join(format!("{name}{PART}"));
        let Ok(meta) = std::fs::metadata(&part) else {
            return Ok(Err(format!("{name}: never uploaded")));
        };
        let actual = file_sha256(&part)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Ok(Err(format!(
                "{name}: sha256 is {actual}, sender expected {expected}"
            )));
        }
        files.push(Artifact {
            name: name.clone(),
            size: meta.len() as i64,
            sha256: actual,
        });
    }
    for file in &files {
        std::fs::rename(
            dir.join(format!("{}{PART}", file.name)),
            dir.join(&file.name),
        )?;
    }
    Ok(Ok(files))
}

fn close(
    conn: &mut rusqlite::Connection,
    id: i64,
    files: &[Artifact],
    repo_id: i64,
    tag: &str,
) -> rusqlite::Result<Vec<i64>> {
    let tx = conn.transaction()?;
    for file in files {
        tx.execute(
            "INSERT INTO artifacts (build_id, name, size, sha256) VALUES (?1, ?2, ?3, ?4)",
            (id, &file.name, file.size, &file.sha256),
        )?;
    }
    tx.execute(
        "UPDATE builds SET status = 'finished', finished_at = ?2 WHERE id = ?1",
        (id, now()),
    )?;
    let expired = expire(&tx, repo_id, tag)?;
    tx.commit()?;
    Ok(expired)
}

async fn finish(
    State(builds): State<Builds>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(finish): Json<Finish>,
) -> Result<Json<Value>, Failure> {
    let repo = builds.repo(&headers).await?;
    let build = builds.own_build(&repo, id).await?;
    if build.status != "uploading" {
        return Err(Failure::bad_request("build is already closed"));
    }
    let _one_at_a_time = builds.uploading.lock().await;
    let dir = builds.build_dir(id);
    let checked = tokio::task::spawn_blocking(move || verify(&dir, &finish.artifacts)).await??;
    let files = checked.map_err(|message| Failure(StatusCode::UNPROCESSABLE_ENTITY, message))?;

    let (repo_id, tag) = (build.repo_id, build.tag);
    let expired = builds
        .db
        .call(move |conn| close(conn, id, &files, repo_id, &tag))
        .await?;
    for build_id in expired {
        let _ = tokio::fs::remove_dir_all(builds.build_dir(build_id)).await;
    }
    builds.deliver_in_background(id);
    builds.shell.publish(build.org_id, "builds");
    let home = builds
        .shell
        .org_address(build.org_id)
        .await?
        .map(|(_, home)| home)
        .unwrap_or_default();
    Ok(Json(
        json!({ "id": id, "url": format!("{home}/builds/{id}") }),
    ))
}

async fn api_download(
    State(builds): State<Builds>,
    Path((id, name)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let repo = builds.repo(&headers).await?;
    let build = builds.own_build(&repo, id).await?;
    builds
        .serve(
            build.id,
            &name,
            format!("token:{}", repo.name),
            client_ip(&headers),
        )
        .await
}

#[derive(Deserialize)]
struct Latest {
    name: String,
    #[serde(default = "prerelease")]
    tag: String,
}

async fn latest(
    State(builds): State<Builds>,
    Query(query): Query<Latest>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let repo = builds.repo(&headers).await?;
    let (repo_id, name, tag) = (repo.id, query.name.clone(), query.tag);
    let build_id: Option<i64> = builds
        .db
        .call(move |conn| {
            conn.query_row(
                "SELECT b.id FROM builds b JOIN artifacts a ON a.build_id = b.id
                 WHERE b.repo_id = ?1 AND b.tag = ?2 AND b.status = 'finished' AND a.name = ?3
                 ORDER BY b.id DESC LIMIT 1",
                (repo_id, tag, name),
                |row| row.get(0),
            )
            .optional()
        })
        .await?;
    let build_id = build_id.ok_or_else(Failure::not_found)?;
    builds
        .serve(
            build_id,
            &query.name,
            format!("token:{}", repo.name),
            client_ip(&headers),
        )
        .await
}

#[derive(Deserialize)]
struct BaseQuery {
    #[serde(default = "prerelease")]
    tag: String,
}

async fn base(
    State(builds): State<Builds>,
    Query(query): Query<BaseQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, Failure> {
    let repo = builds.repo(&headers).await?;
    let repo_id = repo.id;
    let commit: Option<String> = builds
        .db
        .call(move |conn| {
            conn.query_row(
                "SELECT commit_sha FROM builds WHERE repo_id = ?1 AND tag = ?2 AND status = 'finished' AND commit_sha != '' ORDER BY id DESC LIMIT 1",
                (repo_id, query.tag),
                |row| row.get(0),
            )
            .optional()
        })
        .await?;
    Ok(Json(json!({ "commit": commit.unwrap_or_default() })))
}

#[derive(Deserialize)]
struct AppQuery {
    v: Option<String>,
}

async fn app(
    State(builds): State<Builds>,
    Query(query): Query<AppQuery>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    if !builds.per_ip.allow(&client_ip(&headers)) {
        return Err(Failure::new(StatusCode::TOO_MANY_REQUESTS, "slow down"));
    }
    let (build_id, _) = release_apk(&builds, query.v).await?;
    builds
        .serve(build_id, "chi.apk", "app".to_owned(), client_ip(&headers))
        .await
}

async fn app_version(State(builds): State<Builds>) -> Result<String, Failure> {
    Ok(release_apk(&builds, None).await?.1)
}

async fn release_apk(builds: &Builds, version: Option<String>) -> Result<(i64, String), Failure> {
    let build: Option<(i64, String)> = builds
        .db
        .call(move |conn| {
            conn.query_row(
                "SELECT b.id, b.version FROM builds b JOIN artifacts a ON a.build_id = b.id
                 WHERE b.tag = 'release' AND b.status = 'finished' AND a.name = 'chi.apk'
                   AND (?1 IS NULL OR b.version = ?1)
                 ORDER BY b.id DESC LIMIT 1",
                [version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
        })
        .await?;
    build.ok_or_else(Failure::not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names_cannot_leave_their_directory() {
        assert!(safe_name("app-staging.apk"));
        for bad in ["", "..", ".hidden", "a/b.apk", "a\\b", "x.part", "sp ace"] {
            assert!(!safe_name(bad), "{bad}");
        }
    }

    #[test]
    fn files_are_kept_only_for_the_newest_builds_of_a_channel() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        for id in 1..=KEPT_BUILDS_WITH_FILES + 2 {
            let tag = if id == 2 { "release" } else { "prerelease" };
            conn.execute(
                "INSERT INTO builds (id, org_id, repo_id, version, commit_sha, branch, tag, builder, status, started_at)
                 VALUES (?1, 1, 1, 'v', '', '', ?2, '', 'finished', 0)",
                (id, tag),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO artifacts (build_id, name, size, sha256) VALUES (?1, 'a.apk', 1, '')",
                [id],
            )
            .unwrap();
        }
        assert_eq!(expire(&conn, 1, "prerelease").unwrap(), [1]);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, KEPT_BUILDS_WITH_FILES + 1);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM builds", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, KEPT_BUILDS_WITH_FILES + 2);
    }

    #[test]
    fn sha256_of_a_file_matches_the_known_digest() {
        let path = std::env::temp_dir().join(format!("chi-sha-{}", shell::secret()));
        std::fs::write(&path, b"abc").unwrap();
        let digest = file_sha256(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
