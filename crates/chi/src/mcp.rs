use axum::{
    Extension, Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use shell::Ctx;

use crate::Apps;

const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    let (mut properties, mut required) = (properties, required.to_vec());
    if name != "list_orgs" {
        properties["org"] = json!({ "type": "string", "description": "org slug from list_orgs" });
        required.insert(0, "org");
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
        "annotations": { "readOnlyHint": read_only },
    })
}

fn tools() -> Value {
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": 100, "default": 25 });
    let id = json!({ "type": "integer" });
    let mut all = json!([
        tool(
            "list_orgs",
            "Orgs you can read, with your role in each.",
            json!({}),
            &[],
            true
        ),
        tool(
            "list_errors",
            "Crash and error groups reported by apps, most recently seen first.",
            json!({ "status": { "type": "string", "enum": ["open", "resolved", "ignored"], "default": "open" }, "limit": limit }),
            &[],
            true,
        ),
        tool(
            "get_error",
            "One error group with its latest event: exception, stack frames, tags, breadcrumbs, release.",
            json!({ "id": id }),
            &["id"],
            true,
        ),
        tool(
            "list_builds",
            "Builds, newest first. query matches part of a version or commit, or a whole branch or tag; tag is release or prerelease.",
            json!({ "query": { "type": "string" }, "limit": limit }),
            &[],
            true,
        ),
        tool(
            "get_build",
            "One build with its files and where it was published.",
            json!({ "id": id }),
            &["id"],
            true
        ),
        tool(
            "run_agent",
            "Asks the claude or codex CLI a question: text in, text out, no tools. Waits for the answer unless wait is false. Maintainers only.",
            json!({
                "agent": { "type": "string", "enum": agents::AGENTS.map(|(name, _)| name), "default": "claude" },
                "model": { "type": "string" },
                "system": { "type": "string" },
                "prompt": { "type": "string" },
                "timeout": { "type": "integer", "minimum": 10, "maximum": 1800, "default": 300 },
                "wait": { "type": "boolean", "default": true },
            }),
            &["prompt"],
            false,
        ),
        tool(
            "list_agent_jobs",
            "Agent jobs, newest first, without their prompts and answers.",
            json!({ "status": { "type": "string", "enum": ["queued", "running", "done", "failed", "cancelled"] }, "limit": limit }),
            &[],
            true,
        ),
        tool(
            "get_agent_job",
            "One agent job with its prompt and answer.",
            json!({ "id": id }),
            &["id"],
            true
        ),
    ]);
    if let Value::Array(all) = &mut all {
        all.extend(tracker::tools().into_iter().map(
            |(name, description, properties, required, read_only)| {
                tool(name, description, properties, required, read_only)
            },
        ));
    }
    all
}

async fn run(apps: &Apps, orgs: &[Ctx], name: &str, args: &Value) -> Result<Value, String> {
    if name == "list_orgs" {
        return Ok(orgs
            .iter()
            .map(|ctx| json!({ "slug": ctx.slug, "name": ctx.org_name, "role": ctx.role.as_str() }))
            .collect());
    }
    let slug = args["org"]
        .as_str()
        .ok_or("org is required, see list_orgs")?;
    let ctx = orgs
        .iter()
        .find(|ctx| ctx.slug == slug)
        .ok_or("no such org, see list_orgs")?;
    let limit = args["limit"].as_i64().unwrap_or(25).clamp(1, 100);
    let id = || args["id"].as_i64().ok_or("id is required");
    let text = |key: &str| args[key].as_str().unwrap_or("");
    let found = match name {
        "list_errors" => {
            apps.bugs
                .issues_json(ctx, args["status"].as_str().unwrap_or("open"), limit)
                .await
        }
        "get_error" => apps.bugs.issue_json(ctx, id()?).await,
        "list_builds" => apps.builds.builds_json(ctx, text("query"), limit).await,
        "get_build" => apps.builds.one_build_json(ctx, id()?).await,
        "run_agent" => apps.agents.ask(ctx, args, "mcp").await,
        "list_agent_jobs" => apps.agents.jobs_json(ctx, text("status"), limit).await,
        "get_agent_job" => apps.agents.one_job_json(ctx, id()?).await,
        _ => apps
            .tracker
            .call(ctx, name, args)
            .await
            .ok_or_else(|| format!("unknown tool {name}"))?,
    };
    found.map_err(|failure| failure.1)
}

/// Streamable HTTP transport without streams: every request gets one JSON answer.
pub async fn serve(
    State(apps): State<Apps>,
    Extension(orgs): Extension<Vec<Ctx>>,
    Json(message): Json<Value>,
) -> Response {
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = &message["params"];
    let result = match message["method"].as_str().unwrap_or("") {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            json!({
                "protocolVersion": VERSIONS.iter().find(|version| **version == asked).unwrap_or(&VERSIONS[0]),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "chi", "version": env!("CARGO_PKG_VERSION") },
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let outcome = run(&apps, &orgs, params["name"].as_str().unwrap_or(""), &params["arguments"]).await;
            let (text, failed) = match outcome {
                Ok(value) => (value.to_string(), false),
                Err(message) => (message, true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": failed })
        }
        _ => return Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } })).into_response(),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}
