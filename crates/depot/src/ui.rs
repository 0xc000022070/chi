use axum::{
    Extension, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, Redirect, Response},
    routing::{get, post},
};
use maud::{Markup, html};
use serde::Deserialize;
use shell::{Ctx, Failure, Role, client_ip, org_page, utc};

use crate::{BUILD_COLUMNS, Build, Depot, artifacts, build_row};

const BASE: &str = "/depot";

pub fn routes() -> Router<Depot> {
    Router::new()
        .route("/", get(builds))
        .route("/builds/{id}", get(build))
        .route("/builds/{id}/deliver", post(redeliver))
        .route("/builds/{id}/artifacts/{name}", get(download))
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
}

fn short(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}

fn megabytes(size: i64) -> String {
    format!("{:.1} MB", size as f64 / 1_048_576.0)
}

async fn builds(
    State(depot): State<Depot>,
    Extension(ctx): Extension<Ctx>,
    Query(Search { q }): Query<Search>,
) -> Result<Html<String>, Failure> {
    let (org_id, wanted) = (ctx.org_id, q.clone());
    let rows = depot
        .db
        .call(move |conn| {
            conn.prepare(&format!(
                "SELECT {BUILD_COLUMNS} FROM builds
                 WHERE org_id = ?1 AND (?2 = '' OR instr(version, ?2) OR instr(commit_sha, ?2) OR instr(?2, commit_sha) AND commit_sha != '' OR branch = ?2 OR channel = ?2)
                 ORDER BY id DESC LIMIT 200"
            ))?
            .query_map((org_id, wanted), build_row)?
            .collect::<rusqlite::Result<Vec<Build>>>()
        })
        .await?;
    let repos = depot.shell.repos(org_id).await?;
    let repo_name = |id: i64| repos.iter().find(|repo| repo.id == id).map_or("?", |repo| repo.name.as_str());

    Ok(org_page(
        &ctx,
        "depot",
        html! {
            form method="get" { input type="search" name="q" value=(q) placeholder="version, commit, branch or channel"; }
            @if rows.is_empty() { p.dim { "No builds." } }
            @for row in &rows {
                a.row href={ (BASE) "/builds/" (row.id) } {
                    strong { (repo_name(row.repo_id)) " " (row.version) }
                    span {
                        span class={ "badge " (row.status) } { (row.status) }
                        " " (row.channel) " · " (row.branch) " " (short(&row.commit))
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
            dt { "channel" } dd { (build.channel) }
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
    State(depot): State<Depot>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Html<String>, Failure> {
    let build = depot.build(id).await?.filter(|build| build.org_id == ctx.org_id).ok_or_else(Failure::not_found)?;
    let (files, (deliveries, downloads)) = depot.db.call(move |conn| Ok((artifacts(conn, id)?, activity(conn, id)?))).await?;
    let repo = depot.shell.repo(build.org_id, build.repo_id).await?;
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
                a.row href={ (BASE) "/builds/" (id) "/artifacts/" (file.name) } {
                    strong { (file.name) }
                    span { (megabytes(file.size)) " · sha256 " (file.sha256) }
                }
            }
            (published(&deliveries))
            @if ctx.role >= Role::Maintainer && build.status != "uploading" {
                form method="post" action={ (BASE) "/builds/" (id) "/deliver" } { button { "publish again" } }
            }
            (downloaded(&downloads))
        },
    ))
}

async fn redeliver(
    State(depot): State<Depot>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let build = depot.build(id).await?.filter(|build| build.org_id == ctx.org_id).ok_or_else(Failure::not_found)?;
    if build.status == "uploading" {
        return Err(Failure::bad_request("build is still uploading"));
    }
    depot.deliver(id).await?;
    Ok(Redirect::to(&format!("{BASE}/builds/{id}")))
}

async fn download(
    State(depot): State<Depot>,
    Extension(ctx): Extension<Ctx>,
    Path((id, name)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    depot.build(id).await?.filter(|build| build.org_id == ctx.org_id).ok_or_else(Failure::not_found)?;
    depot.serve(id, &name, ctx.email, client_ip(&headers)).await
}
