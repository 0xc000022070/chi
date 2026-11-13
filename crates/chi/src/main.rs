mod mcp;

use axum::{
    Extension, Router,
    extract::State,
    middleware,
    response::Html,
    routing::{get, post},
};
use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::ExitCode};

const USAGE: &str = "usage: chi serve | chi push ... | chi passwd <email>

serve reads its settings from the environment:
  CHI_URL     public url, e.g. https://chi.example.com (required)
  CHI_DATA    directory for databases and build files (default ./data)
  CHI_LISTEN  address to bind (default 127.0.0.1:8080)
  CHI_HOOKS   directory of executables; each one gets every event as JSON on stdin

passwd reads the new password from stdin, uses CHI_DATA and signs the user out everywhere";

fn hooks(dir: Option<String>) -> Result<Vec<PathBuf>, String> {
    let Some(dir) = dir else { return Ok(Vec::new()) };
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

async fn status(
    State((bugs, builds)): State<(bugs::Bugs, builds::Builds)>,
    Extension(ctx): Extension<shell::Ctx>,
) -> Result<Html<String>, shell::Failure> {
    Ok(Html(bugs.pulse(ctx.org_id).await? + &builds.pulse(ctx.org_id).await?))
}

async fn serve() -> Result<(), String> {
    let url = std::env::var("CHI_URL").map_err(|_| USAGE)?;
    let data = PathBuf::from(std::env::var("CHI_DATA").unwrap_or_else(|_| "data".to_owned()));
    let listen = std::env::var("CHI_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;

    let shell = shell::Shell::open(&data, &url, hooks(std::env::var("CHI_HOOKS").ok())?)?;
    let bugs = bugs::Bugs::open(&data, shell.clone())?;
    let builds = builds::Builds::open(&data, shell.clone())?;
    shell.bootstrap().await.map_err(|failure| failure.1)?;

    let live = Router::new().route("/status", get(status)).with_state((bugs.clone(), builds.clone()));
    let mcp = Router::new()
        .route("/mcp", post(mcp::serve))
        .route_layer(middleware::from_fn_with_state(shell.clone(), shell::bearer))
        .with_state((bugs.clone(), builds.clone()));
    let app = shell.router(bugs.ui().merge(builds.ui()).merge(live), bugs.ingest().merge(builds.api()).merge(mcp));
    let listener = tokio::net::TcpListener::bind(&listen).await.map_err(|e| format!("{listen}: {e}"))?;
    println!("chi listening on {listen}");
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

fn passwd(email: Option<&String>) -> Result<(), String> {
    let email = email.ok_or(USAGE)?;
    let data = PathBuf::from(std::env::var("CHI_DATA").unwrap_or_else(|_| "data".to_owned()));
    let mut password = String::new();
    std::io::stdin().read_line(&mut password).map_err(|e| e.to_string())?;
    shell::set_password(&data, email, password.trim_end_matches(['\r', '\n']))
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
