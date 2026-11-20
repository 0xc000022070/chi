use axum::{
    Extension, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, Redirect, Response},
    routing::{get, post},
};
use maud::{Markup, html};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use shell::{
    Ctx, Failure, Role, ago, autolink, client_ip, glyph, markdown, org_page, repo_glyph, utc,
};

use crate::{BUILD_COLUMNS, Build, Builds, artifacts, build_row};

const BASE: &str = "builds";

pub fn routes() -> Router<Builds> {
    Router::new()
        .route("/", get(builds))
        .route("/{id}", get(build))
        .route("/{id}/deliver", post(redeliver))
        .route("/{id}/pipeline", post(rerun))
        .route("/{id}/artifacts/{name}", get(download))
}

impl Builds {
    pub async fn pulse(&self, org_id: i64) -> Result<String, Failure> {
        let latest = self
            .db
            .call(move |conn| {
                conn.query_row(
                    &format!("SELECT {BUILD_COLUMNS} FROM builds WHERE org_id = ?1 ORDER BY id DESC LIMIT 1"),
                    [org_id],
                    build_row,
                )
                .optional()
            })
            .await?;
        let Some(build) = latest else {
            return Ok(String::new());
        };
        let repo = self.shell.repo(org_id, build.repo_id).await?;
        Ok(html! {
            a href={ (BASE) "/" (build.id) } {
                "[#]" b { (repo.map_or_else(|| "?".to_owned(), |repo| repo.name)) " " (build.version) }
                (build.status) " · " (ago(build.started_at))
            }
        }
        .into_string())
    }
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
}

fn short(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}

fn file_size(size: i64) -> String {
    match size {
        ..1024 => format!("{size} B"),
        1024..1_048_576 => format!("{:.0} KB", size as f64 / 1024.0),
        _ => format!("{:.1} MB", size as f64 / 1_048_576.0),
    }
}

fn search(
    conn: &rusqlite::Connection,
    org_id: i64,
    q: &str,
    limit: i64,
) -> rusqlite::Result<Vec<Build>> {
    conn.prepare(&format!(
        "SELECT {BUILD_COLUMNS} FROM builds
         WHERE org_id = ?1 AND (?2 = '' OR instr(version, ?2) OR instr(commit_sha, ?2) OR instr(?2, commit_sha) AND commit_sha != '' OR branch = ?2 OR tag = ?2)
         ORDER BY id DESC LIMIT ?3"
    ))?
    .query_map((org_id, q, limit), build_row)?
    .collect()
}

impl Builds {
    fn build_json(&self, ctx: &Ctx, build: &Build, repos: &[shell::Repo]) -> Value {
        let repo = repos.iter().find(|repo| repo.id == build.repo_id);
        json!({
            "id": build.id,
            "repo": repo.map(|repo| &repo.name),
            "repo_url": repo.map(|repo| &repo.url),
            "version": build.version,
            "commit": build.commit,
            "branch": build.branch,
            "tag": build.tag,
            "builder": build.builder,
            "status": build.status,
            "started": utc(build.started_at),
            "finished": build.finished_at.map(utc),
            "url": self.shell.url(&ctx.href(&format!("/{BASE}/{}", build.id))),
        })
    }

    pub async fn builds_json(&self, ctx: &Ctx, q: &str, limit: i64) -> Result<Value, Failure> {
        let (org_id, q) = (ctx.org_id, q.to_owned());
        let rows = self
            .db
            .call(move |conn| search(conn, org_id, &q, limit))
            .await?;
        let repos = self.shell.repos(org_id).await?;
        Ok(rows
            .iter()
            .map(|build| self.build_json(ctx, build, &repos))
            .collect())
    }

    pub async fn one_build_json(&self, ctx: &Ctx, id: i64) -> Result<Value, Failure> {
        let build = self
            .build(id)
            .await?
            .filter(|build| build.org_id == ctx.org_id)
            .ok_or_else(Failure::not_found)?;
        let (files, (deliveries, _), extras) = self
            .db
            .call(move |conn| Ok((artifacts(conn, id)?, activity(conn, id)?, extras(conn, id)?)))
            .await?;
        let mut found = self.build_json(ctx, &build, &self.shell.repos(ctx.org_id).await?);
        for (key, value) in extras_json(&extras).as_object().into_iter().flatten() {
            found[key] = value.clone();
        }
        found["files"] = files
            .iter()
            .map(|file| json!({ "name": file.name, "size": file.size, "sha256": file.sha256 }))
            .collect();
        found["published"] = deliveries
            .iter()
            .map(|(target, ok, output, at)| json!({ "target": target, "ok": ok, "output": output, "at": utc(*at) }))
            .collect();
        Ok(found)
    }
}

async fn builds(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Query(Search { q }): Query<Search>,
) -> Result<Html<String>, Failure> {
    let (org_id, wanted) = (ctx.org_id, q.clone());
    let rows = builds
        .db
        .call(move |conn| search(conn, org_id, &wanted, 200))
        .await?;
    let repos = builds.shell.repos(org_id).await?;
    let repo = |id: i64| {
        repos
            .iter()
            .find(|repo| repo.id == id)
            .map_or(("--", "?"), |repo| {
                (repo_glyph(&repo.kind), repo.name.as_str())
            })
    };

    Ok(org_page(
        &ctx,
        "builds",
        html! {
            form method="get" data-live="builds" { input type="search" name="q" value=(q) aria-label="Search builds" placeholder="version, commit, branch or tag"; }
            @if rows.is_empty() { p.dim { "No builds." } }
            @for row in &rows {
                @let (kind, name) = repo(row.repo_id);
                a.row href={ (BASE) "/" (row.id) } {
                    strong { (glyph(kind)) (name) " " (row.version) " " span.dim { "#" (row.id) } }
                    span {
                        span class={ "badge " (row.status) } { (row.status) }
                        " " span.badge { (row.tag) } " " (row.branch) " " (short(&row.commit))
                        " · " (utc(row.started_at)) " · " (row.builder)
                    }
                }
            }
        },
    ))
}

struct Run {
    flake: String,
    status: String,
    error: String,
    started_at: i64,
    steps: Vec<(String, String, String)>,
}

struct Extras {
    notes: Option<String>,
    links: Vec<(String, String)>,
    run: Option<Run>,
    commits: Vec<Value>,
    diffstat: String,
}

fn last_run(conn: &rusqlite::Connection, build_id: i64) -> rusqlite::Result<Option<Run>> {
    let run = conn
        .query_row(
            "SELECT id, flake, status, error, started_at FROM runs WHERE build_id = ?1 ORDER BY id DESC LIMIT 1",
            [build_id],
            |row| Ok((row.get::<_, i64>(0)?, Run { flake: row.get(1)?, status: row.get(2)?, error: row.get(3)?, started_at: row.get(4)?, steps: Vec::new() })),
        )
        .optional()?;
    let Some((run_id, mut run)) = run else {
        return Ok(None);
    };
    run.steps = conn
        .prepare("SELECT name, status, output FROM steps WHERE run_id = ?1 ORDER BY id")?
        .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(run))
}

fn extras(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Extras> {
    let notes = conn
        .query_row("SELECT text FROM notes WHERE build_id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    let links = conn
        .prepare("SELECT label, url FROM links WHERE build_id = ?1 ORDER BY id")?
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let run = last_run(conn, id)?;
    let (commits, diffstat): (String, String) = conn
        .query_row(
            "SELECT commits, diffstat FROM contexts WHERE build_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    Ok(Extras {
        notes,
        links,
        run,
        commits: serde_json::from_str(&commits).unwrap_or_default(),
        diffstat,
    })
}

fn extras_json(extras: &Extras) -> Value {
    json!({
        "notes": extras.notes,
        "links": extras.links.iter().map(|(label, url)| json!({ "label": label, "url": url })).collect::<Vec<_>>(),
        "pipeline": extras.run.as_ref().map(|run| json!({
            "flake": run.flake,
            "status": run.status,
            "error": run.error,
            "started": utc(run.started_at),
            "steps": run.steps.iter().map(|(name, status, output)| json!({ "name": name, "status": status, "output": output })).collect::<Vec<_>>(),
        })),
        "commits": extras.commits,
        "diffstat": extras.diffstat,
    })
}

fn pipeline(extras: &Extras, repo_url: &str) -> Markup {
    html! {
        @if let Some(notes) = &extras.notes { h2 { "Notes" } div.md { (markdown(notes)) } }
        @if !extras.links.is_empty() {
            h2 { "Links" }
            @for (label, url) in &extras.links { a.row href=(url) rel="noopener" { strong { (label) } span.dim { (url) } } }
        }
        @if let Some(run) = &extras.run {
            h2 { "Pipeline " span class={ "badge " (run.status) } { (run.status) } }
            p.dim { (run.flake) " · " (utc(run.started_at)) }
            @if !run.error.is_empty() { pre { (run.error) } }
            @for (name, status, output) in &run.steps {
                details.frame open[status == "failed"] {
                    summary { strong { (name) } " " span class={ "badge " (status) } { (status) } }
                    @if !output.is_empty() { pre { (output) } }
                }
            }
        }
        @if !extras.commits.is_empty() {
            h2 { "Commits" }
            @for commit in &extras.commits {
                @let sha = commit["sha"].as_str().unwrap_or("");
                div.frame {
                    @if repo_url.is_empty() { code { (short(sha)) } } @else { a href={ (repo_url) "/commit/" (sha) } { code { (short(sha)) } } }
                    " " (autolink(commit["subject"].as_str().unwrap_or("")))
                    span.dim { " " (commit["author"].as_str().unwrap_or("")) }
                }
            }
            @if !extras.diffstat.is_empty() { details { summary { "diffstat" } pre { (extras.diffstat) } } }
        }
    }
}

type Delivery = (String, bool, String, i64);
type Download = (String, String, String, i64);

fn activity(
    conn: &rusqlite::Connection,
    id: i64,
) -> rusqlite::Result<(Vec<Delivery>, Vec<Download>)> {
    let deliveries = conn
        .prepare("SELECT target, ok, output, at FROM deliveries WHERE build_id = ?1 ORDER BY id DESC LIMIT 50")?
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let downloads = conn
        .prepare("SELECT name, who, ip, at FROM downloads WHERE build_id = ?1 ORDER BY id DESC LIMIT 100")?
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((deliveries, downloads))
}

fn facts(build: &Build, repo: Option<&shell::Repo>) -> Markup {
    html! {
        dl {
            dt { "status" } dd { span class={ "badge " (build.status) } { (build.status) } }
            dt { "tag" } dd { span.badge { (build.tag) } }
            dt { "branch" } dd { (build.branch) }
            dt { "commit" } dd {
                @match repo.filter(|repo| !repo.url.is_empty() && !build.commit.is_empty()) {
                    Some(repo) => a href={ (repo.url) "/commit/" (build.commit) } { (build.commit) },
                    None => (build.commit),
                }
            }
            dt { "built by" } dd { (build.builder) }
            dt { "started" } dd { (utc(build.started_at)) }
            @if let Some(finished_at) = build.finished_at { dt { "ended" } dd { (utc(finished_at)) } }
        }
    }
}

fn published(deliveries: &[Delivery]) -> Markup {
    html! {
        h2 { "Published to" }
        @if deliveries.is_empty() { p.dim { "Nowhere yet." } }
        @for (target, ok, output, at) in deliveries {
            div.frame {
                strong { (target) } " "
                @if *ok { span.badge { "ok" } } @else { span.badge.failed { "failed" } }
                span.dim { " " (utc(*at)) }
                @if !output.is_empty() { pre { (output) } }
            }
        }
    }
}

fn downloaded(downloads: &[Download]) -> Markup {
    html! {
        h2 { "Downloads" }
        @if downloads.is_empty() { p.dim { "Nobody yet." } }
        @for (name, who, ip, at) in downloads {
            div.frame { (who) span.dim { " " (name) " · " (ip) " · " (utc(*at)) } }
        }
    }
}

async fn build(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Html<String>, Failure> {
    let build = builds
        .build(id)
        .await?
        .filter(|build| build.org_id == ctx.org_id)
        .ok_or_else(Failure::not_found)?;
    let (files, (deliveries, downloads), extras) = builds
        .db
        .call(move |conn| Ok((artifacts(conn, id)?, activity(conn, id)?, extras(conn, id)?)))
        .await?;
    let repo = builds.shell.repo(build.org_id, build.repo_id).await?;
    let title = format!(
        "{} {}",
        repo.as_ref().map_or("?", |repo| repo.name.as_str()),
        build.version
    );

    Ok(org_page(
        &ctx,
        &title,
        html! {
            h1 data-live="builds" { (title) " " span.dim { "#" (id) } }
            (facts(&build, repo.as_ref()))
            h2 { "Files" }
            @if files.is_empty() { p.dim { "No files stored for this build." } }
            @for file in &files {
                a.row href={ (BASE) "/" (id) "/artifacts/" (file.name) } {
                    strong { (file.name) }
                    span { (file_size(file.size)) " · sha256 " (file.sha256) }
                }
            }
            (pipeline(&extras, repo.as_ref().map_or("", |repo| repo.url.as_str())))
            @if ctx.role >= Role::Maintainer && build.status == "finished" && repo.as_ref().is_some_and(|repo| !repo.pipeline.is_empty()) {
                form method="post" action={ (BASE) "/" (id) "/pipeline" } { button { "run pipeline again" } }
            }
            (published(&deliveries))
            @if ctx.role >= Role::Maintainer && build.status != "uploading" {
                form method="post" action={ (BASE) "/" (id) "/deliver" } { button { "publish again" } }
            }
            (downloaded(&downloads))
        },
    ))
}

async fn redeliver(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let build = builds
        .build(id)
        .await?
        .filter(|build| build.org_id == ctx.org_id)
        .ok_or_else(Failure::not_found)?;
    if build.status == "uploading" {
        return Err(Failure::bad_request("build is still uploading"));
    }
    builds.deliver(id).await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}

async fn rerun(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    builds
        .build(id)
        .await?
        .filter(|build| build.org_id == ctx.org_id && build.status == "finished")
        .ok_or_else(Failure::not_found)?;
    if builds.running(id).await? {
        return Err(Failure::bad_request(
            "the pipeline is already running for this build",
        ));
    }
    let runner = builds.clone();
    tokio::spawn(async move {
        if let Err(Failure(_, message)) = runner.pipeline(id).await {
            eprintln!("builds: pipeline of build {id} failed: {message}");
        }
    });
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}

async fn download(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Path((id, name)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    builds
        .build(id)
        .await?
        .filter(|build| build.org_id == ctx.org_id)
        .ok_or_else(Failure::not_found)?;
    builds
        .serve(id, &name, ctx.email, client_ip(&headers))
        .await
}

#[cfg(test)]
mod tests {
    use super::file_size;

    #[test]
    fn file_size_picks_a_readable_unit() {
        assert_eq!(file_size(512), "512 B");
        assert_eq!(file_size(16_948), "17 KB");
        assert_eq!(file_size(90_000_000), "85.8 MB");
    }
}

pub const SEARCH_HELP: &str = "status:uploading|finished|failed is:failed|ok|running repo: branch: tag: by:builder";

impl Builds {
    async fn items(
        &self,
        org_id: i64,
        condition: String,
        mut params: Vec<rusqlite::types::Value>,
        limit: i64,
    ) -> Result<Vec<shell::Item>, Failure> {
        params.insert(0, org_id.into());
        params.push(limit.into());
        let builds: Vec<Build> = self
            .db
            .call(move |conn| {
                conn.prepare(&format!(
                    "SELECT {BUILD_COLUMNS} FROM builds WHERE org_id = ? AND {condition} ORDER BY coalesce(finished_at, started_at) DESC LIMIT ?"
                ))?
                .query_map(rusqlite::params_from_iter(params), build_row)?
                .collect()
            })
            .await?;
        let repos = self.shell.repos(org_id).await?;
        Ok(builds
            .into_iter()
            .map(|build| shell::Item {
                at: build.finished_at.unwrap_or(build.started_at),
                kind: "build",
                title: format!(
                    "{} {}",
                    repos.iter().find(|repo| repo.id == build.repo_id).map_or("?", |repo| repo.name.as_str()),
                    build.version
                ),
                url: format!("{BASE}/{}", build.id),
                badge: if build.status == "uploading" { "running".to_owned() } else { build.status },
                detail: [build.branch, build.tag, build.commit.chars().take(8).collect(), build.builder]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
            })
            .collect())
    }

    pub async fn find(&self, org_id: i64, query: &str, limit: i64) -> Result<Vec<shell::Item>, Failure> {
        let repos = self.shell.repos(org_id).await?;
        let filter = shell::find::compile(
            query,
            "coalesce(finished_at, started_at)",
            &["version", "commit_sha", "branch", "tag", "builder"],
            |term, filter| {
                let lower = || term.lower().into_iter().map(rusqlite::types::Value::from).collect();
                match term.key.as_str() {
                    "status" => filter.add(term.negated, "status IN ({})", lower()),
                    "branch" => filter.add(term.negated, "lower(branch) IN ({})", lower()),
                    "tag" => filter.add(term.negated, "lower(tag) IN ({})", lower()),
                    "by" => filter.add(term.negated, "lower(builder) IN ({})", lower()),
                    "repo" => {
                        let wanted = term.lower();
                        let ids = repos
                            .iter()
                            .filter(|repo| wanted.contains(&repo.name.to_lowercase()))
                            .map(|repo| repo.id.into())
                            .chain([(-1).into()])
                            .collect();
                        filter.add(term.negated, "repo_id IN ({})", ids);
                    }
                    "is" => {
                        let sql = match term.lower().first().map(String::as_str) {
                            Some("failed") => "status = 'failed'",
                            Some("ok") => "status = 'finished'",
                            Some("running") => "status = 'uploading'",
                            _ => return Err("is: takes failed, ok or running".to_owned()),
                        };
                        filter.add(term.negated, sql, Vec::new());
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            },
        )
        .map_err(|message| Failure::bad_request(&message))?;
        self.items(org_id, filter.condition(), filter.params, limit).await
    }

    pub async fn activity(&self, org_id: i64, before: i64, limit: i64) -> Result<Vec<shell::Item>, Failure> {
        self.items(org_id, "coalesce(finished_at, started_at) < ?".to_owned(), vec![before.into()], limit).await
    }
}
