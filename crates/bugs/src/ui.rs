use std::io::Read;

use axum::{
    Extension, Form, Router,
    extract::{Path, Query, State},
    response::{Html, Redirect},
    routing::{get, post},
};
use flate2::read::GzDecoder;
use maud::{Markup, html};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use shell::{Ctx, Failure, Role, ago, now, org_page, secret, utc};

use crate::{Bugs, envelope};

const BASE: &str = "bugs";
const STATUSES: [&str; 3] = ["open", "resolved", "ignored"];
const IS_DEV: &str = "i.environment IN ('development', 'dev', 'local')";

fn environment_badge(environment: &str) -> Option<&'static str> {
    match environment {
        "development" | "dev" | "local" => Some("dev"),
        "staging" | "stg" => Some("stg"),
        _ => None,
    }
}

pub fn routes() -> Router<Bugs> {
    Router::new()
        .route("/", get(issues))
        .route("/{id}", get(issue))
        .route("/{id}/status", post(set_status))
        .route("/projects", get(projects).post(create_project))
        .route("/projects/{id}/keys", post(create_key))
        .route("/keys/{key}/revoke", post(revoke_key))
}

impl Bugs {
    pub async fn pulse(&self, org_id: i64) -> Result<String, Failure> {
        let (open, latest) = self
            .db
            .call(move |conn| {
                let open: i64 = conn.query_row(
                    &format!("SELECT count(*) FROM issues i JOIN projects p ON p.id = i.project_id WHERE p.org_id = ?1 AND i.status = 'open' AND NOT {IS_DEV}"),
                    [org_id],
                    |row| row.get(0),
                )?;
                let latest = conn
                    .query_row(
                        &format!("SELECT i.id, i.title, p.name, i.last_seen FROM issues i JOIN projects p ON p.id = i.project_id
                         WHERE p.org_id = ?1 AND i.status = 'open' AND NOT {IS_DEV} ORDER BY i.last_seen DESC LIMIT 1"),
                        [org_id],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?)),
                    )
                    .optional()?;
                Ok((open, latest))
            })
            .await?;
        Ok(html! {
            a href=(BASE) class=(if open > 0 { "hot" } else { "calm" }) { "[!] " (open) " open" }
            @if let Some((id, title, project, seen)) = latest {
                a href={ (BASE) "/" (id) } title=(title) {
                    b { (project) } (title.chars().take(60).collect::<String>()) " · " (ago(seen))
                }
            }
        }
        .into_string())
    }
}

const EVENT_KEYS: [&str; 11] = [
    "message",
    "logentry",
    "exception",
    "tags",
    "release",
    "environment",
    "platform",
    "transaction",
    "request",
    "contexts",
    "breadcrumbs",
];

impl Bugs {
    pub async fn issues_json(&self, ctx: &Ctx, status: &str, limit: i64) -> Result<Value, Failure> {
        if !STATUSES.contains(&status) {
            return Err(Failure::bad_request("status is open, resolved or ignored"));
        }
        let (org_id, status) = (ctx.org_id, status.to_owned());
        let rows = self
            .db
            .call(move |conn| list_issues(conn, org_id, &status, limit))
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "id": row.id,
                    "title": row.title,
                    "culprit": row.culprit,
                    "level": row.level,
                    "events": row.count,
                    "last_seen": utc(row.last_seen),
                    "project": row.project,
                    "environment": row.environment,
                    "url": self.shell.url(&ctx.href(&format!("/{BASE}/{}", row.id))),
                })
            })
            .collect())
    }

    pub async fn issue_json(&self, ctx: &Ctx, id: i64) -> Result<Value, Failure> {
        let org_id = ctx.org_id;
        let detail = self
            .db
            .call(move |conn| load_issue(conn, id, org_id))
            .await?
            .ok_or_else(Failure::not_found)?;
        let repo = match detail.repo_id {
            Some(repo_id) => self.shell.repo(org_id, repo_id).await?.map(|repo| repo.url),
            None => None,
        };
        let event = latest_event(&detail)?;
        let latest: serde_json::Map<String, Value> = EVENT_KEYS
            .iter()
            .filter(|key| !event[**key].is_null())
            .map(|key| ((*key).to_owned(), event[*key].clone()))
            .collect();
        Ok(json!({
            "id": id,
            "title": detail.title,
            "level": detail.level,
            "status": detail.status,
            "events": detail.count,
            "first_seen": utc(detail.first_seen),
            "last_seen": utc(detail.last_seen),
            "project": detail.project,
            "repo": repo,
            "source_path_prefix": detail.path_prefix,
            "url": self.shell.url(&ctx.href(&format!("/{BASE}/{id}"))),
            "recent": detail
                .recent
                .iter()
                .map(|(event_id, at, release)| json!({ "event_id": event_id, "received": utc(*at), "release": release }))
                .collect::<Vec<_>>(),
            "latest_event": latest,
        }))
    }
}

#[derive(Deserialize)]
struct IssuesQuery {
    status: Option<String>,
}

struct IssueRow {
    id: i64,
    title: String,
    culprit: String,
    level: String,
    count: i64,
    last_seen: i64,
    project: String,
    environment: String,
}

fn list_issues(
    conn: &rusqlite::Connection,
    org_id: i64,
    status: &str,
    limit: i64,
) -> rusqlite::Result<Vec<IssueRow>> {
    conn.prepare(&format!(
        "SELECT i.id, i.title, i.culprit, i.level, i.count, i.last_seen, p.name, i.environment, {IS_DEV} AS dev
         FROM issues i JOIN projects p ON p.id = i.project_id
         WHERE p.org_id = ?1 AND i.status = ?2 ORDER BY dev, i.last_seen DESC LIMIT ?3"
    ))?
    .query_map((org_id, status, limit), |row| {
        Ok(IssueRow {
            id: row.get(0)?,
            title: row.get(1)?,
            culprit: row.get(2)?,
            level: row.get(3)?,
            count: row.get(4)?,
            last_seen: row.get(5)?,
            project: row.get(6)?,
            environment: row.get(7)?,
        })
    })?
    .collect()
}

async fn issues(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<IssuesQuery>,
) -> Result<Html<String>, Failure> {
    let status = query.status.unwrap_or_else(|| "open".to_owned());
    let (wanted, org_id) = (status.clone(), ctx.org_id);
    let rows = bugs
        .db
        .call(move |conn| list_issues(conn, org_id, &wanted, 200))
        .await?;

    Ok(org_page(
        &ctx,
        "bugs",
        html! {
            div.tabs {
                @for tab in STATUSES {
                    a href={ (BASE) "?status=" (tab) } aria-current=[(tab == status).then_some("page")] { (tab) }
                }
                a href={ (BASE) "/projects" } class="end" { "projects" }
            }
            @if rows.is_empty() { p.dim { "Nothing here." } }
            @for row in &rows {
                a.row href={ (BASE) "/" (row.id) } {
                    strong { span.dim { "#" (row.id) } " " (row.title) }
                    span { (row.culprit) }
                    br;
                    span {
                        span class={ "badge " (row.level) } { (row.level) }
                        @if let Some(badge) = environment_badge(&row.environment) { " " span.badge { (badge) } }
                        " " (row.project) " · " (row.count) " events · " (utc(row.last_seen))
                    }
                }
            }
        },
    ))
}

struct IssueDetail {
    title: String,
    level: String,
    status: String,
    count: i64,
    first_seen: i64,
    last_seen: i64,
    project: String,
    repo_id: Option<i64>,
    path_prefix: String,
    latest: Option<Vec<u8>>,
    recent: Vec<(Option<String>, i64, Option<String>)>,
}

fn source_url(
    repo: &str,
    path_prefix: &str,
    release: Option<&str>,
    frame: &Value,
) -> Option<String> {
    if repo.is_empty() || frame["in_app"] == false {
        return None;
    }
    let path = frame["filename"]
        .as_str()?
        .strip_prefix(path_prefix)?
        .trim_start_matches('/');
    Some(format!(
        "{}/blob/{}/{path}#L{}",
        repo.trim_end_matches('/'),
        release.unwrap_or("HEAD"),
        frame["lineno"].as_u64().unwrap_or(1)
    ))
}

fn tags(event: &Value) -> Vec<(String, String)> {
    let text = |value: &Value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    };
    match &event["tags"] {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), text(value)))
            .collect(),
        Value::Array(pairs) => pairs
            .iter()
            .map(|pair| (text(&pair[0]), text(&pair[1])))
            .collect(),
        _ => Vec::new(),
    }
}

fn issue_detail(row: &rusqlite::Row) -> rusqlite::Result<IssueDetail> {
    Ok(IssueDetail {
        title: row.get(0)?,
        level: row.get(1)?,
        status: row.get(2)?,
        count: row.get(3)?,
        first_seen: row.get(4)?,
        last_seen: row.get(5)?,
        project: row.get(6)?,
        repo_id: row.get(7)?,
        path_prefix: row.get(8)?,
        latest: row.get(9)?,
        recent: Vec::new(),
    })
}

fn load_issue(
    conn: &rusqlite::Connection,
    id: i64,
    org_id: i64,
) -> rusqlite::Result<Option<IssueDetail>> {
    let detail = conn
        .query_row(
            "SELECT i.title, i.level, i.status, i.count, i.first_seen, i.last_seen,
                    p.name, p.repo_id, p.path_prefix,
                    (SELECT payload FROM events WHERE issue_id = i.id ORDER BY id DESC LIMIT 1)
             FROM issues i JOIN projects p ON p.id = i.project_id WHERE i.id = ?1 AND p.org_id = ?2",
            (id, org_id),
            issue_detail,
        )
        .optional()?;
    let Some(mut detail) = detail else {
        return Ok(None);
    };
    detail.recent = conn
        .prepare("SELECT event_id, received_at, release FROM events WHERE issue_id = ?1 ORDER BY id DESC LIMIT 20")?
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(detail))
}

fn latest_event(detail: &IssueDetail) -> std::io::Result<Value> {
    let mut json = Vec::new();
    if let Some(payload) = &detail.latest {
        GzDecoder::new(&payload[..]).read_to_end(&mut json)?;
    }
    Ok(serde_json::from_slice(&json).unwrap_or(Value::Null))
}

fn facts(detail: &IssueDetail, event: &Value) -> Markup {
    html! {
        dl {
            dt { "project" } dd { (detail.project) }
            dt { "level" } dd { span class={ "badge " (detail.level) } { (detail.level) } }
            dt { "status" } dd { (detail.status) }
            dt { "events" } dd { (detail.count) }
            dt { "first seen" } dd { (utc(detail.first_seen)) }
            dt { "last seen" } dd { (utc(detail.last_seen)) }
            @if let Some(environment) = event["environment"].as_str() { dt { "environment" } dd { (environment) } }
            @if let Some(release) = event["release"].as_str() {
                dt { "release" } dd { a href={ "builds?q=" (shell::url_encode(release)) } { (release) } }
            }
            @if let Some(user) = event["user"]["id"].as_str() { dt { "user" } dd { (user) } }
        }
    }
}

fn status_buttons(id: i64, current: &str) -> Markup {
    html! {
        div.quick {
            @for status in STATUSES {
                @if status != current {
                    form.inline method="post" action={ (BASE) "/" (id) "/status" } {
                        button class=(status) name="status" value=(status) { "Mark " (status) }
                    }
                }
            }
        }
    }
}

fn stack(detail: &IssueDetail, event: &Value, repo: &str) -> Markup {
    html! {
        h2 { "Stack" }
        @if detail.latest.is_none() { p.dim { "No stored event for this issue." } }
        @for frame in envelope::frames(event).iter().rev() {
            div.frame.lib[frame["in_app"] != true] {
                code {
                    @let location = format!(
                        "{}:{}",
                        frame["filename"].as_str().unwrap_or("?"),
                        frame["lineno"].as_u64().unwrap_or(0)
                    );
                    @if let Some(url) = source_url(repo, &detail.path_prefix, event["release"].as_str(), frame) {
                        a href=(url) { (location) }
                    } @else { (location) }
                    " in " (frame["function"].as_str().unwrap_or("?"))
                }
                @if let Some(line) = frame["context_line"].as_str() { pre { (line.trim()) } }
            }
        }
    }
}

fn attachments(detail: &IssueDetail, event: &Value) -> Markup {
    let tags = tags(event);
    let extra = match &event["extra"] {
        Value::Null => None,
        extra => serde_json::to_string_pretty(extra).ok(),
    };
    html! {
        @if !tags.is_empty() {
            h2 { "Tags" }
            dl { @for (key, value) in &tags { dt { (key) } dd { (value) } } }
        }
        @if let Some(extra) = extra { h2 { "Extra" } pre { (extra) } }
        h2 { "Recent events" }
        @for (event_id, received_at, release) in &detail.recent {
            div.frame {
                (utc(*received_at))
                span.dim { " " (release.as_deref().unwrap_or("")) " " (event_id.as_deref().unwrap_or("")) }
            }
        }
    }
}

async fn issue(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let detail = bugs
        .db
        .call(move |conn| load_issue(conn, id, org_id))
        .await?
        .ok_or_else(Failure::not_found)?;
    let repo = match detail.repo_id {
        Some(repo_id) => bugs
            .shell
            .repo(org_id, repo_id)
            .await?
            .map(|repo| repo.url)
            .unwrap_or_default(),
        None => String::new(),
    };

    let event = latest_event(&detail)?;

    Ok(org_page(
        &ctx,
        &detail.title,
        html! {
            h1 { span.dim { "#" (id) } " " (detail.title) }
            (facts(&detail, &event))
            @if ctx.role >= Role::Maintainer {
                (status_buttons(id, &detail.status))
                form.inline method="post" action={ "issues/from-error/" (id) } { button { "create issue" } }
            }
            (stack(&detail, &event, &repo))
            (attachments(&detail, &event))
        },
    ))
}

#[derive(Deserialize)]
struct StatusForm {
    status: String,
}

async fn set_status(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
    Form(form): Form<StatusForm>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    if !STATUSES.contains(&form.status.as_str()) {
        return Err(Failure::bad_request("unknown status"));
    }
    let org_id = ctx.org_id;
    bugs
        .db
        .call(move |conn| {
            conn.execute(
                "UPDATE issues SET status = ?1 WHERE id = ?2 AND project_id IN (SELECT id FROM projects WHERE org_id = ?3)",
                (form.status, id, org_id),
            )
        })
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}

struct ProjectRow {
    id: i64,
    name: String,
    repo_id: Option<i64>,
    keys: Vec<(String, String)>,
}

async fn projects(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let rows = bugs
        .db
        .call(move |conn| {
            let mut rows = conn
                .prepare("SELECT id, name, repo_id FROM projects WHERE org_id = ?1 ORDER BY name")?
                .query_map([org_id], |row| {
                    Ok(ProjectRow { id: row.get(0)?, name: row.get(1)?, repo_id: row.get(2)?, keys: Vec::new() })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for row in &mut rows {
                row.keys = conn
                    .prepare("SELECT public_key, label FROM keys WHERE project_id = ?1 ORDER BY created_at")?
                    .query_map([row.id], |key| Ok((key.get(0)?, key.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
            }
            Ok(rows)
        })
        .await?;
    let repos = bugs.shell.repos(org_id).await?;
    let hub = bugs.shell.url("");
    let (scheme, host) = hub.split_once("://").unwrap_or(("https", &hub));
    let maintainer = ctx.role >= Role::Maintainer;

    Ok(org_page(
        &ctx,
        "projects",
        html! {
            h1 { "Projects" }
            @if maintainer {
                button.primary onclick="document.getElementById('new-project').showModal()" { "new project" }
                dialog #new-project aria-labelledby="new-project-title" {
                    form.stack method="post" {
                        h2 #new-project-title { "New project" }
                        label { "Name" input name="name" required; }
                        label {
                            "Repo"
                            select name="repo_id" {
                                option value="" { "no repo" }
                                @for repo in &repos { option value=(repo.id) { (repo.name) } }
                            }
                        }
                        label { "Frame path prefix to strip" input name="path_prefix" placeholder="/workspace/"; }
                        div.actions {
                            button.primary { "create" }
                            button type="button" onclick="this.closest('dialog').close()" { "cancel" }
                        }
                    }
                }
            }
            @if rows.is_empty() { p.dim { "No projects yet." } }
            @for row in &rows {
                div.frame {
                    strong { (row.name) }
                    @if let Some(repo) = repos.iter().find(|repo| Some(repo.id) == row.repo_id) {
                        " " span.dim { (repo.name) }
                    }
                    @if maintainer {
                        @for (key, label) in &row.keys {
                            div {
                                span.dim { (label) } " "
                                form.inline method="post" action={ (BASE) "/keys/" (key) "/revoke" } { button { "revoke" } }
                                pre { (scheme) "://" (key) "@" (host) "/" (row.id) }
                            }
                        }
                        form.inline method="post" action={ (BASE) "/projects/" (row.id) "/keys" } {
                            input name="label" placeholder="new key label" aria-label="New key label" required;
                            " " button { "add key" }
                        }
                    }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
struct ProjectForm {
    name: String,
    #[serde(default)]
    repo_id: String,
    #[serde(default)]
    path_prefix: String,
}

async fn add_key(bugs: &Bugs, org_id: i64, project_id: i64, label: String) -> Result<(), Failure> {
    let key = secret()[..32].to_owned();
    let stored = key.clone();
    let added = bugs
        .db
        .call(move |conn| {
            conn.execute(
                "INSERT INTO keys (public_key, project_id, label, created_at)
                 SELECT ?1, id, ?3, ?4 FROM projects WHERE id = ?2 AND org_id = ?5",
                (stored, project_id, label, now(), org_id),
            )
        })
        .await?;
    if added == 0 {
        return Err(Failure::not_found());
    }
    bugs.keys
        .write()
        .expect("no holder of the key cache panics")
        .insert(key, project_id);
    Ok(())
}

async fn create_project(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<ProjectForm>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let name = form.name.trim().to_owned();
    if name.is_empty() {
        return Err(Failure::bad_request("a name is required"));
    }
    let repo_id = match form.repo_id.parse::<i64>() {
        Ok(repo_id) => Some(
            bugs.shell
                .repo(ctx.org_id, repo_id)
                .await?
                .ok_or_else(Failure::not_found)?
                .id,
        ),
        Err(_) => None,
    };
    let org_id = ctx.org_id;
    let project_id = bugs
        .db
        .call(move |conn| {
            let created = conn.execute(
                "INSERT INTO projects (org_id, name, repo_id, path_prefix) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                (org_id, name, repo_id, form.path_prefix.trim()),
            )?;
            Ok((created == 1).then(|| conn.last_insert_rowid()))
        })
        .await?
        .ok_or_else(|| Failure::bad_request("a project with that name exists"))?;
    add_key(&bugs, org_id, project_id, "default".to_owned()).await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/projects"))))
}

#[derive(Deserialize)]
struct KeyForm {
    label: String,
}

async fn create_key(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
    Path(project_id): Path<i64>,
    Form(form): Form<KeyForm>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    add_key(&bugs, ctx.org_id, project_id, form.label.trim().to_owned()).await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/projects"))))
}

async fn revoke_key(
    State(bugs): State<Bugs>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let (org_id, stored) = (ctx.org_id, key.clone());
    let revoked = bugs
        .db
        .call(move |conn| {
            conn.execute(
                "DELETE FROM keys WHERE public_key = ?1 AND project_id IN (SELECT id FROM projects WHERE org_id = ?2)",
                (stored, org_id),
            )
        })
        .await?;
    if revoked == 1 {
        bugs.keys
            .write()
            .expect("no holder of the key cache panics")
            .remove(&key);
    }
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/projects"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn badges_dev_and_staging_but_not_prod() {
        assert_eq!(environment_badge("development"), Some("dev"));
        assert_eq!(environment_badge("staging"), Some("stg"));
        assert_eq!(environment_badge("stg"), Some("stg"));
        assert_eq!(environment_badge("prod"), None);
        assert_eq!(environment_badge(""), None);
    }

    #[test]
    fn links_in_app_frames_to_the_repo_at_the_release() {
        let frame = json!({"filename": "/workspace/services/a.ts", "lineno": 57, "in_app": true});
        let repo = "https://github.com/cuentacero/gate-k9";
        assert_eq!(
            source_url(repo, "/workspace/", Some("e19cf46"), &frame).as_deref(),
            Some("https://github.com/cuentacero/gate-k9/blob/e19cf46/services/a.ts#L57")
        );
        assert!(source_url("", "/workspace/", None, &frame).is_none());
        let runtime = json!({"filename": "/encore/runtimes/js/x.ts", "lineno": 1});
        assert!(source_url(repo, "/workspace/", None, &runtime).is_none());
    }
}

pub const SEARCH_HELP: &str = "status:open|resolved|ignored is:open|closed|dev level: env: project:, #12 finds error 12";

type Row = (i64, String, String, String, String, String, i64, i64, String);

impl Bugs {
    async fn rows(&self, org_id: i64, condition: String, mut params: Vec<rusqlite::types::Value>, order: &'static str, limit: i64) -> Result<Vec<shell::Item>, Failure> {
        params.insert(0, org_id.into());
        params.push(limit.into());
        let rows: Vec<Row> = self
            .db
            .call(move |conn| {
                conn.prepare(&format!(
                    "SELECT i.id, i.title, i.culprit, i.level, i.status, i.environment, i.count, {order}, p.name
                     FROM issues i JOIN projects p ON p.id = i.project_id WHERE p.org_id = ? AND {condition} ORDER BY 8 DESC LIMIT ?"
                ))?
                .query_map(rusqlite::params_from_iter(params), |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?))
                })?
                .collect()
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, title, culprit, level, status, environment, count, at, project)| shell::Item {
                at,
                kind: "error",
                title,
                url: format!("{BASE}/{id}"),
                badge: if status == "open" { level } else { status },
                detail: [project, environment_badge(&environment).unwrap_or("").to_owned(), format!("{count}×"), culprit]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
            })
            .collect())
    }

    pub async fn find(&self, org_id: i64, query: &str, limit: i64) -> Result<Vec<shell::Item>, Failure> {
        let filter = shell::find::compile(query, "i.last_seen", &["i.title", "i.culprit", "'#' || i.id"], |term, filter| {
            let lower = || term.lower().into_iter().map(rusqlite::types::Value::from).collect();
            match term.key.as_str() {
                "status" => filter.add(term.negated, "i.status IN ({})", lower()),
                "level" => filter.add(term.negated, "lower(i.level) IN ({})", lower()),
                "env" => filter.add(term.negated, "lower(i.environment) IN ({})", lower()),
                "project" => filter.add(term.negated, "lower(p.name) IN ({})", lower()),
                "is" => {
                    let sql = match term.lower().first().map(String::as_str) {
                        Some("open") => "i.status = 'open'",
                        Some("closed") => "i.status != 'open'",
                        Some("dev") => IS_DEV,
                        _ => return Err("is: takes open, closed or dev".to_owned()),
                    };
                    filter.add(term.negated, sql, Vec::new());
                }
                _ => return Ok(false),
            }
            Ok(true)
        })
        .map_err(|message| Failure::bad_request(&message))?;
        self.rows(org_id, filter.condition(), filter.params, "i.last_seen", limit).await
    }

    pub async fn activity(&self, org_id: i64, before: i64, limit: i64) -> Result<Vec<shell::Item>, Failure> {
        let mut items = self.rows(org_id, "i.first_seen < ?".to_owned(), vec![before.into()], "i.first_seen", limit).await?;
        for item in &mut items {
            item.badge = format!("new {}", item.badge);
        }
        Ok(items)
    }
}
