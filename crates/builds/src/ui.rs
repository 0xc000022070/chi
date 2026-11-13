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
use shell::{Ctx, Failure, Role, ago, client_ip, glyph, org_page, repo_glyph, utc};

use crate::{BUILD_COLUMNS, Build, Builds, artifacts, build_row};

const BASE: &str = "builds";

pub fn routes() -> Router<Builds> {
    Router::new()
        .route("/", get(builds))
        .route("/{id}", get(build))
        .route("/{id}/deliver", post(redeliver))
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
        let Some(build) = latest else { return Ok(String::new()) };
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

async fn builds(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Query(Search { q }): Query<Search>,
) -> Result<Html<String>, Failure> {
    let (org_id, wanted) = (ctx.org_id, q.clone());
    let rows = builds
        .db
        .call(move |conn| {
            conn.prepare(&format!(
                "SELECT {BUILD_COLUMNS} FROM builds
                 WHERE org_id = ?1 AND (?2 = '' OR instr(version, ?2) OR instr(commit_sha, ?2) OR instr(?2, commit_sha) AND commit_sha != '' OR branch = ?2 OR tag = ?2)
                 ORDER BY id DESC LIMIT 200"
            ))?
            .query_map((org_id, wanted), build_row)?
            .collect::<rusqlite::Result<Vec<Build>>>()
        })
        .await?;
    let repos = builds.shell.repos(org_id).await?;
    let repo = |id: i64| repos.iter().find(|repo| repo.id == id).map_or(("--", "?"), |repo| (repo_glyph(&repo.kind), repo.name.as_str()));

    Ok(org_page(
        &ctx,
        "builds",
        html! {
            form method="get" { input type="search" name="q" value=(q) aria-label="Search builds" placeholder="version, commit, branch or tag"; }
            @if rows.is_empty() { p.dim { "No builds." } }
            @for row in &rows {
                @let (kind, name) = repo(row.repo_id);
                a.row href={ (BASE) "/" (row.id) } {
                    strong { (glyph(kind)) (name) " " (row.version) }
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

type Delivery = (String, bool, String, i64);
type Download = (String, String, String, i64);

fn activity(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<(Vec<Delivery>, Vec<Download>)> {
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
    let build = builds.build(id).await?.filter(|build| build.org_id == ctx.org_id).ok_or_else(Failure::not_found)?;
    let (files, (deliveries, downloads)) = builds.db.call(move |conn| Ok((artifacts(conn, id)?, activity(conn, id)?))).await?;
    let repo = builds.shell.repo(build.org_id, build.repo_id).await?;
    let title = format!("{} {}", repo.as_ref().map_or("?", |repo| repo.name.as_str()), build.version);

    Ok(org_page(
        &ctx,
        &title,
        html! {
            h1 { (title) }
            (facts(&build, repo.as_ref()))
            h2 { "Files" }
            @if files.is_empty() { p.dim { "No files stored for this build." } }
            @for file in &files {
                a.row href={ (BASE) "/" (id) "/artifacts/" (file.name) } {
                    strong { (file.name) }
                    span { (file_size(file.size)) " · sha256 " (file.sha256) }
                }
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
    let build = builds.build(id).await?.filter(|build| build.org_id == ctx.org_id).ok_or_else(Failure::not_found)?;
    if build.status == "uploading" {
        return Err(Failure::bad_request("build is still uploading"));
    }
    builds.deliver(id).await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}

async fn download(
    State(builds): State<Builds>,
    Extension(ctx): Extension<Ctx>,
    Path((id, name)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    builds.build(id).await?.filter(|build| build.org_id == ctx.org_id).ok_or_else(Failure::not_found)?;
    builds.serve(id, &name, ctx.email, client_ip(&headers)).await
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
