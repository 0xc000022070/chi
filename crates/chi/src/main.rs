mod home;
mod mcp;

use axum::{
    Extension, Router,
    extract::State,
    middleware,
    response::Html,
    routing::{get, post},
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::ExitCode};

const USAGE: &str = "usage: chi serve | chi push ... | chi passwd <email> | chi import-linear --org <slug>

serve reads its settings from the environment:
  CHI_URL     public url, e.g. https://chi.example.com (required)
  CHI_DATA    directory for databases and build files (default ./data)
  CHI_LISTEN  address to bind (default 127.0.0.1:8080)
  CHI_HOOKS   directory of executables; each one gets every event as JSON on stdin
  CHI_CLAUDE, CHI_CODEX  agent CLIs (default: claude and codex from PATH)
  CHI_AGENT_SLOTS        agent jobs run at once (default 2)
  CHI_GITHUB_SECRET      GitHub webhook secret for POST /api/github (or the github-secret credential)

passwd reads the new password from stdin, uses CHI_DATA and signs the user out everywhere
import-linear copies a Linear workspace into the org using LINEAR_API_KEY and CHI_DATA; run it again to update";

fn hooks(dir: Option<String>) -> Result<Vec<PathBuf>, String> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let mut hooks = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("{dir}: {e}"))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            hooks.push(path);
        }
    }
    hooks.sort();
    Ok(hooks)
}

#[derive(Clone)]
struct Apps {
    shell: shell::Shell,
    bugs: bugs::Bugs,
    builds: builds::Builds,
    agents: agents::Agents,
    tracker: tracker::Tracker,
}

async fn status(
    State(apps): State<Apps>,
    Extension(ctx): Extension<shell::Ctx>,
) -> Result<Html<String>, shell::Failure> {
    Ok(Html(
        apps.bugs.pulse(ctx.org_id).await?
            + &apps.builds.pulse(ctx.org_id).await?
            + &apps.agents.pulse(ctx.org_id).await?
            + &apps.tracker.pulse(&ctx).await?,
    ))
}

async fn serve() -> Result<(), String> {
    let url = std::env::var("CHI_URL").map_err(|_| USAGE)?;
    let data = PathBuf::from(std::env::var("CHI_DATA").unwrap_or_else(|_| "data".to_owned()));
    let listen = std::env::var("CHI_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;

    let shell = shell::Shell::open(&data, &url, hooks(std::env::var("CHI_HOOKS").ok())?)?;
    let bugs = bugs::Bugs::open(&data, shell.clone())?;
    let agents = agents::Agents::open(&data, shell.clone())?;
    let builds = builds::Builds::open(&data, shell.clone(), agents.clone())?;
    let tracker = tracker::Tracker::open(&data, shell.clone(), agents.clone(), bugs.clone())?;
    shell.bootstrap().await.map_err(|failure| failure.1)?;

    let apps = Apps {
        shell: shell.clone(),
        bugs,
        builds,
        agents,
        tracker,
    };
    let live = Router::new()
        .route("/status", get(status))
        .merge(home::routes())
        .with_state(apps.clone());
    let mcp = Router::new()
        .route("/mcp", post(mcp::serve))
        .route_layer(middleware::from_fn_with_state(shell.clone(), shell::bearer))
        .with_state(apps.clone());
    let app = shell.router(
        apps.bugs
            .ui()
            .merge(apps.builds.ui())
            .merge(apps.agents.ui())
            .merge(apps.tracker.ui())
            .merge(live),
        apps.bugs
            .ingest()
            .merge(apps.builds.api())
            .merge(apps.agents.api())
            .merge(apps.tracker.api())
            .merge(mcp),
    );
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("{listen}: {e}"))?;
    println!("chi listening on {listen}");
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

fn passwd(email: Option<&String>) -> Result<(), String> {
    let email = email.ok_or(USAGE)?;
    let data = PathBuf::from(std::env::var("CHI_DATA").unwrap_or_else(|_| "data".to_owned()));
    let mut password = String::new();
    std::io::stdin()
        .read_line(&mut password)
        .map_err(|e| e.to_string())?;
    shell::set_password(&data, email, password.trim_end_matches(['\r', '\n']))
}

fn import_linear(args: &[String]) -> Result<(), String> {
    let ["--org", slug] = args.iter().map(String::as_str).collect::<Vec<_>>()[..] else {
        return Err(USAGE.to_owned());
    };
    let key = std::env::var("LINEAR_API_KEY").map_err(|_| "LINEAR_API_KEY is not set")?;
    let data = PathBuf::from(std::env::var("CHI_DATA").unwrap_or_else(|_| "data".to_owned()));
    println!("{}", tracker::import_linear(&data, slug, &key)?);
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.first().map(String::as_str) {
        Some("serve") => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(8)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|runtime| runtime.block_on(serve())),
        Some("push") => builds::push(&args[1..]),
        Some("passwd") => passwd(args.get(1)),
        Some("import-linear") => import_linear(&args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
