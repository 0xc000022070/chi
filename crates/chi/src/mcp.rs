use axum::{
    Extension, Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use shell::Ctx;

const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

fn tools() -> Value {
    let org = json!({ "type": "string", "description": "org slug from list_orgs" });
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": 100, "default": 25 });
    let read_only = json!({ "readOnlyHint": true });
    json!([
        {
            "name": "list_orgs",
            "description": "Orgs you can read, with your role in each.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "list_issues",
            "description": "Crash and error issues reported by apps, most recently seen first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "org": org,
                    "status": { "type": "string", "enum": ["open", "resolved", "ignored"], "default": "open" },
                    "limit": limit,
                },
                "required": ["org"],
            },
            "annotations": read_only,
        },
        {
            "name": "get_issue",
            "description": "One issue with its latest event: exception, stack frames, tags, breadcrumbs, release.",
            "inputSchema": {
                "type": "object",
                "properties": { "org": org, "id": { "type": "integer" } },
                "required": ["org", "id"],
            },
            "annotations": read_only,
        },
        {
            "name": "list_builds",
            "description": "Builds, newest first. query matches part of a version or commit, or a whole branch or tag; tag is release or prerelease.",
            "inputSchema": {
                "type": "object",
                "properties": { "org": org, "query": { "type": "string" }, "limit": limit },
                "required": ["org"],
            },
            "annotations": read_only,
        },
        {
            "name": "get_build",
            "description": "One build with its files and where it was published.",
            "inputSchema": {
                "type": "object",
                "properties": { "org": org, "id": { "type": "integer" } },
                "required": ["org", "id"],
            },
            "annotations": read_only,
        },
    ])
}

async fn run(apps: &(bugs::Bugs, builds::Builds), orgs: &[Ctx], name: &str, args: &Value) -> Result<Value, String> {
    if name == "list_orgs" {
        return Ok(orgs.iter().map(|ctx| json!({ "slug": ctx.slug, "name": ctx.org_name, "role": ctx.role.as_str() })).collect());
    }
    let slug = args["org"].as_str().ok_or("org is required, see list_orgs")?;
    let ctx = orgs.iter().find(|ctx| ctx.slug == slug).ok_or("no such org, see list_orgs")?;
    let limit = args["limit"].as_i64().unwrap_or(25).clamp(1, 100);
    let id = || args["id"].as_i64().ok_or("id is required");
    let (bugs, builds) = apps;
    let found = match name {
        "list_issues" => bugs.issues_json(ctx, args["status"].as_str().unwrap_or("open"), limit).await,
        "get_issue" => bugs.issue_json(ctx, id()?).await,
        "list_builds" => builds.builds_json(ctx, args["query"].as_str().unwrap_or(""), limit).await,
        "get_build" => builds.one_build_json(ctx, id()?).await,
        _ => return Err(format!("unknown tool {name}")),
    };
    found.map_err(|failure| failure.1)
}

/// Streamable HTTP transport without streams: every request gets one JSON answer.
pub async fn serve(
    State(apps): State<(bugs::Bugs, builds::Builds)>,
    Extension(orgs): Extension<Vec<Ctx>>,
    Json(message): Json<Value>,
) -> Response {
    let Some(id) = message.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
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
