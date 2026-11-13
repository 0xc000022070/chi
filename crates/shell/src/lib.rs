mod oauth;
mod sso;
mod store;
mod ui;
mod web;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub use oauth::bearer;
pub use store::Repo;
pub use ui::{glyph, org_page, repo_glyph};

#[derive(Debug)]
pub struct Failure(pub StatusCode, pub String);

impl Failure {
    pub fn new(status: StatusCode, message: &str) -> Self {
        Self(status, message.to_owned())
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not found")
    }

    pub fn bad_request(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden")
    }
}

impl<E: std::error::Error> From<E> for Failure {
    fn from(error: E) -> Self {
        eprintln!("internal error: {error}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path, schema: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "cache_size", -256)?;
        conn.execute_batch(schema)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, Failure> {
        let conn = self.0.clone();
        Ok(tokio::task::spawn_blocking(move || f(&mut conn.lock().expect("a panicking query aborts the process"))).await??)
    }
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn utc(ts: i64) -> String {
    let (days, secs) = (ts.div_euclid(86400), ts.rem_euclid(86400));
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}Z", secs / 3600, secs % 3600 / 60)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("os entropy");
    hex(&bytes)
}

pub fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("local")
        .to_owned()
}

pub struct Limiter {
    buckets: Mutex<HashMap<String, (f64, i64)>>,
    per_second: f64,
    burst: f64,
}

const MAX_KEYS: usize = 20_000;

impl Limiter {
    pub fn new(per_second: f64, burst: f64) -> Self {
        Self { buckets: Mutex::new(HashMap::new()), per_second, burst }
    }

    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, now())
    }

    fn allow_at(&self, key: &str, now: i64) -> bool {
        let mut buckets = self.buckets.lock().expect("no holder of the bucket map panics");
        if buckets.len() >= MAX_KEYS {
            buckets.clear();
        }
        let (tokens, last) = buckets.entry(key.to_owned()).or_insert((self.burst, now));
        *tokens = (*tokens + (now - *last) as f64 * self.per_second).min(self.burst);
        *last = now;
        if *tokens < 1.0 {
            return false;
        }
        *tokens -= 1.0;
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Role {
    Viewer,
    Maintainer,
    Owner,
}

impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "maintainer" => Some(Self::Maintainer),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Maintainer => "maintainer",
            Self::Owner => "owner",
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub org_id: i64,
    pub org_name: String,
    pub slug: String,
    pub user_id: i64,
    pub email: String,
    pub role: Role,
}

impl Ctx {
    pub fn require(&self, role: Role) -> Result<(), Failure> {
        if self.role >= role { Ok(()) } else { Err(Failure::forbidden()) }
    }

    pub fn href(&self, path: &str) -> String {
        format!("/{}{path}", self.slug)
    }
}

struct Pending {
    provider_id: i64,
    verifier: String,
    next: String,
}

pub struct HookResult {
    pub hook: String,
    pub ok: bool,
    pub output: String,
}

#[derive(Clone)]
pub struct Shell {
    db: Db,
    url: Arc<str>,
    pending: Held<Pending>,
    codes: Held<oauth::Code>,
    hooks: Arc<Vec<PathBuf>>,
    logins: Arc<Limiter>,
}

type Held<T> = Arc<Mutex<HashMap<String, (T, i64)>>>;

const PENDING_TTL: i64 = 300;

fn hold<T>(map: &Mutex<HashMap<String, (T, i64)>>, value: T) -> String {
    let key = secret();
    let now = now();
    let mut map = map.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    map.retain(|_, (_, expires)| *expires > now);
    map.insert(key.clone(), (value, now + PENDING_TTL));
    key
}

fn take<T>(map: &Mutex<HashMap<String, (T, i64)>>, key: &str) -> Option<T> {
    let (value, expires) = map.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(key)?;
    (expires > now()).then_some(value)
}
const HOOK_TIMEOUT: Duration = Duration::from_secs(120);

impl Shell {
    pub fn open(data: &Path, url: &str, hooks: Vec<PathBuf>) -> Result<Self, String> {
        let url = url.trim_end_matches('/');
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!("the url must start with http:// or https://: {url}"));
        }
        let db = Db::open(&data.join("chi.db"), include_str!("schema.sql")).map_err(|e| e.to_string())?;
        add_repo_kind(&db.0.lock().expect("nothing else holds the connection yet")).map_err(|e| e.to_string())?;
        Ok(Self {
            db,
            url: url.into(),
            pending: Arc::default(),
            codes: Arc::default(),
            hooks: Arc::new(hooks),
            logins: Arc::new(Limiter::new(0.05, 10.0)),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    fn hold(&self, pending: Pending) -> String {
        hold(&self.pending, pending)
    }

    fn take(&self, key: &str) -> Option<Pending> {
        take(&self.pending, key)
    }

    pub async fn repos(&self, org_id: i64) -> Result<Vec<Repo>, Failure> {
        self.db.call(move |conn| store::repos(conn, org_id)).await
    }

    pub async fn repo(&self, org_id: i64, id: i64) -> Result<Option<Repo>, Failure> {
        Ok(self.repos(org_id).await?.into_iter().find(|repo| repo.id == id))
    }

    pub async fn repo_by_token(&self, token: &str) -> Result<Option<Repo>, Failure> {
        let hash = sha256_hex(token.as_bytes());
        self.db.call(move |conn| store::repo_by_token(conn, &hash)).await
    }

    pub async fn org_address(&self, org_id: i64) -> Result<Option<(String, String)>, Failure> {
        let org = self.db.call(move |conn| store::org(conn, org_id)).await?;
        Ok(org.map(|org| { let home = self.url(&format!("/{}", org.slug)); (org.slug, home) }))
    }

    pub async fn bootstrap(&self) -> Result<(), Failure> {
        let token = secret();
        let hash = sha256_hex(token.as_bytes());
        let needed = self
            .db
            .call(move |conn| {
                if store::has_admin(conn)? {
                    return Ok(false);
                }
                conn.execute("DELETE FROM invites WHERE org_id IS NULL", [])?;
                store::create_invite(conn, &hash, None, Role::Owner, now() + 86_400)?;
                Ok(true)
            })
            .await?;
        if needed {
            println!("no suite admin yet, create one at: {}", self.url(&format!("/invite/{token}")));
        }
        Ok(())
    }

    pub async fn run_hooks(&self, event: &Value) -> Vec<HookResult> {
        let payload = event.to_string();
        let mut results = Vec::new();
        for hook in self.hooks.iter() {
            let name = hook.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let outcome = tokio::time::timeout(HOOK_TIMEOUT, run_hook(hook, &payload)).await;
            let (ok, output) = match outcome {
                Ok(Ok((ok, output))) => (ok, output),
                Ok(Err(error)) => (false, error.to_string()),
                Err(_) => (false, "timed out".to_owned()),
            };
            if !ok {
                eprintln!("hook {name} failed: {output}");
            }
            results.push(HookResult { hook: name, ok, output });
        }
        results
    }
}

fn add_repo_kind(conn: &Connection) -> rusqlite::Result<()> {
    let present: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('repos') WHERE name = 'kind')", [], |row| row.get(0))?;
    if !present {
        conn.execute("ALTER TABLE repos ADD COLUMN kind TEXT NOT NULL DEFAULT ''", [])?;
    }
    Ok(())
}

pub fn ago(ts: i64) -> String {
    match (now() - ts).max(0) {
        secs @ 0..60 => format!("{secs}s ago"),
        secs @ 60..3600 => format!("{}m ago", secs / 60),
        secs @ 3600..86_400 => format!("{}h ago", secs / 3600),
        secs => format!("{}d ago", secs / 86_400),
    }
}

async fn run_hook(hook: &Path, payload: &str) -> std::io::Result<(bool, String)> {
    let mut child = tokio::process::Command::new(hook)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(payload.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    let text = if output.status.success() { output.stdout } else { output.stderr };
    let text: String = String::from_utf8_lossy(&text).trim().chars().take(500).collect();
    Ok((output.status.success(), text))
}

pub fn set_password(data: &Path, email: &str, password: &str) -> Result<(), String> {
    if password.len() < web::MIN_PASSWORD {
        return Err(format!("the password needs {}+ characters", web::MIN_PASSWORD));
    }
    let email = email.trim().to_lowercase();
    let hash = web::hash_password(password).map_err(|failure| failure.1)?;
    let conn = Connection::open(data.join("chi.db")).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
    let changed = conn
        .execute("UPDATE users SET password_hash = ?1 WHERE email = ?2", (&hash, &email))
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("no user {email}"));
    }
    for table in ["sessions", "grants"] {
        conn.execute(&format!("DELETE FROM {table} WHERE user_id = (SELECT id FROM users WHERE email = ?1)"), [&email])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_password_replaces_the_hash_and_ends_sessions() {
        let data = std::env::temp_dir().join(format!("chi-passwd-{}", secret()));
        std::fs::create_dir_all(&data).unwrap();
        let conn = Connection::open(data.join("chi.db")).unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute("INSERT INTO users (id, email, password_hash, created_at) VALUES (1, 'a@b.c', 'old', 0)", []).unwrap();
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ('t', 1, 9999999999)",
            [],
        )
        .unwrap();

        assert!(set_password(&data, "a@b.c", "short").is_err());
        assert!(set_password(&data, "nobody@b.c", "long enough password").is_err());
        set_password(&data, " A@B.c ", "long enough password").unwrap();

        let hash: String = conn.query_row("SELECT password_hash FROM users WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert!(web::password_matches("long enough password", &hash));
        let sessions: i64 = conn.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0)).unwrap();
        assert_eq!(sessions, 0);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn repo_kind_is_added_to_an_old_database_once() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE repos (id INTEGER PRIMARY KEY, name TEXT NOT NULL); INSERT INTO repos VALUES (1, 'a');").unwrap();
        add_repo_kind(&conn).unwrap();
        add_repo_kind(&conn).unwrap();
        let kind: String = conn.query_row("SELECT kind FROM repos", [], |row| row.get(0)).unwrap();
        assert_eq!(kind, "");
    }

    #[test]
    fn ago_picks_the_largest_unit() {
        assert_eq!(ago(now() - 5), "5s ago");
        assert_eq!(ago(now() - 150), "2m ago");
        assert_eq!(ago(now() - 2 * 86_400), "2d ago");
    }

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01 00:00Z");
        assert_eq!(utc(1_794_580_440), "2026-11-13 14:34Z");
    }

    #[test]
    fn limiter_spends_the_burst_then_refills_over_time() {
        let limiter = Limiter::new(1.0, 2.0);
        assert!(limiter.allow_at("a", 100));
        assert!(limiter.allow_at("a", 100));
        assert!(!limiter.allow_at("a", 100));
        assert!(limiter.allow_at("b", 100));
        assert!(limiter.allow_at("a", 101));
        assert!(!limiter.allow_at("a", 101));
    }

    #[test]
    fn roles_order_by_power() {
        assert!(Role::Owner > Role::Maintainer && Role::Maintainer > Role::Viewer);
        assert_eq!(Role::parse(Role::Maintainer.as_str()), Some(Role::Maintainer));
        assert_eq!(Role::parse("admin"), None);
    }
}
