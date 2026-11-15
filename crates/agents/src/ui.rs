use axum::{
    Extension, Form, Router,
    extract::{Path, Query, State},
    response::{Html, Redirect},
    routing::{get, post},
};
use maud::html;
use serde::Deserialize;
use shell::{Ctx, Failure, Role, ago, org_page, utc};

use crate::{AGENTS, Agents, DEFAULT_TIMEOUT, Request};

const BASE: &str = "agents";
const TABS: [&str; 6] = ["", "queued", "running", "done", "failed", "cancelled"];

pub fn routes() -> Router<Agents> {
    Router::new()
        .route("/", get(jobs).post(submit))
        .route("/{id}", get(job))
        .route("/{id}/cancel", post(cancel))
        .route("/{id}/retry", post(retry))
}

impl Agents {
    pub async fn pulse(&self, org_id: i64) -> Result<String, Failure> {
        let busy = self
            .db
            .call(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM jobs WHERE org_id = ?1 AND status IN ('queued', 'running')",
                    [org_id],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await?;
        Ok(if busy == 0 {
            String::new()
        } else {
            html! { a href=(BASE) { "[*]" b { (busy) " agent jobs" } "working" } }.into_string()
        })
    }
}

#[derive(Deserialize)]
struct Tab {
    #[serde(default)]
    status: String,
}

async fn jobs(
    State(agents): State<Agents>,
    Extension(ctx): Extension<Ctx>,
    Query(Tab { status }): Query<Tab>,
) -> Result<Html<String>, Failure> {
    let rows = agents.jobs(ctx.org_id, &status, 200).await?;
    Ok(org_page(
        &ctx,
        "agents",
        html! {
            div.tabs data-live="agents" {
                @for tab in TABS {
                    a href={ (BASE) "?status=" (tab) } aria-current=[(tab == status).then_some("page")] { (if tab.is_empty() { "all" } else { tab }) }
                }
            }
            @if ctx.role >= Role::Maintainer {
                details {
                    summary data-key="c" { "new job" }
                    form method="post" action=(BASE) {
                        label { "agent " select name="agent" { @for (name, _) in AGENTS { option { (name) } } } }
                        label { "model " input name="model" placeholder="default"; }
                        label { "timeout (s) " input name="timeout" type="number" min="10" max="1800" value=(DEFAULT_TIMEOUT); }
                        label { "system " textarea name="system" rows="2" {} }
                        label { "prompt " textarea name="prompt" rows="8" required {} }
                        button { "queue" }
                    }
                }
            }
            @if rows.is_empty() { p.dim { "No jobs." } }
            @for row in &rows {
                a.row href={ (BASE) "/" (row.id) } {
                    strong { span.dim { "#" (row.id) } " " (row.agent) @if !row.model.is_empty() { " " span.dim { (row.model) } } }
                    span {
                        span class={ "badge " (row.status) } { (row.status) }
                        " " (row.source) " · " (row.requester) " · " (ago(row.created_at))
                    }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
struct NewJob {
    agent: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    system: String,
    prompt: String,
    timeout: Option<i64>,
}

async fn submit(
    State(agents): State<Agents>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<NewJob>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let id = agents
        .submit(Request {
            org_id: ctx.org_id,
            agent: form.agent,
            model: form.model.trim().to_owned(),
            system: form.system,
            prompt: form.prompt,
            source: "ui".to_owned(),
            requester: ctx.email.clone(),
            timeout: form.timeout.unwrap_or(DEFAULT_TIMEOUT),
        })
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}

async fn job(
    State(agents): State<Agents>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Html<String>, Failure> {
    let job = agents
        .job(id)
        .await?
        .filter(|job| job.org_id == ctx.org_id)
        .ok_or_else(Failure::not_found)?;
    let title = format!("agent job #{id}");
    let active = matches!(job.status.as_str(), "queued" | "running");
    Ok(org_page(
        &ctx,
        &title,
        html! {
            h1 { (title) }
            dl data-live=[active.then_some("agents")] {
                dt { "status" } dd { span class={ "badge " (job.status) } { (job.status) } }
                dt { "agent" } dd { (job.agent) @if !job.model.is_empty() { " " (job.model) } }
                dt { "source" } dd { (job.source) }
                dt { "requested by" } dd { (job.requester) }
                dt { "timeout" } dd { (job.timeout) "s" }
                dt { "queued" } dd { (utc(job.created_at)) }
                @if let Some(at) = job.started_at { dt { "started" } dd { (utc(at)) } }
                @if let Some(at) = job.finished_at { dt { "ended" } dd { (utc(at)) } }
            }
            @if ctx.role >= Role::Maintainer {
                @if active {
                    form method="post" action={ (BASE) "/" (id) "/cancel" } { button { "cancel" } }
                } @else {
                    form method="post" action={ (BASE) "/" (id) "/retry" } { button { "run again" } }
                }
            }
            @if !job.error.is_empty() { h2 { "Error" } pre { (job.error) } }
            @if !job.output.is_empty() { h2 { "Answer" } pre { (job.output) } }
            @if !job.system.is_empty() { h2 { "System" } pre { (job.system) } }
            h2 { "Prompt" } pre { (job.prompt) }
        },
    ))
}

async fn cancel(
    State(agents): State<Agents>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    agents.cancel(ctx.org_id, id).await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}

async fn retry(
    State(agents): State<Agents>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let old = agents
        .job(id)
        .await?
        .filter(|job| job.org_id == ctx.org_id)
        .ok_or_else(Failure::not_found)?;
    let id = agents
        .submit(Request {
            org_id: ctx.org_id,
            agent: old.agent,
            model: old.model,
            system: old.system,
            prompt: old.prompt,
            source: old.source,
            requester: ctx.email.clone(),
            timeout: old.timeout,
        })
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/{BASE}/{id}"))))
}
