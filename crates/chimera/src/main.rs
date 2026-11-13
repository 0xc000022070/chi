use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::ExitCode};

const USAGE: &str = "usage: chimera serve | chimera push ...

serve reads its settings from the environment:
  CHIMERA_URL     public url of the hub, e.g. https://chimera.example.com (required)
  CHIMERA_DATA    directory for databases and build files (default ./data)
  CHIMERA_LISTEN  address to bind (default 127.0.0.1:8080)
  CHIMERA_HOOKS   directory of executables; each one gets every event as JSON on stdin";

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

async fn serve() -> Result<(), String> {
    let url = std::env::var("CHIMERA_URL").map_err(|_| USAGE)?;
    let data = PathBuf::from(std::env::var("CHIMERA_DATA").unwrap_or_else(|_| "data".to_owned()));
    let listen = std::env::var("CHIMERA_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;

    let shell = shell::Shell::open(&data, &url, hooks(std::env::var("CHIMERA_HOOKS").ok())?)?;
    let faults = faults::Faults::open(&data, shell.clone())?;
    let depot = depot::Depot::open(&data, shell.clone())?;
    shell.bootstrap().await.map_err(|failure| failure.1)?;

    let app = shell.router(faults.ui().merge(depot.ui()), faults.ingest().merge(depot.api()));
    let listener = tokio::net::TcpListener::bind(&listen).await.map_err(|e| format!("{listen}: {e}"))?;
    println!("chimera listening on {listen}");
    axum::serve(listener, app).await.map_err(|e| e.to_string())
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
        Some("push") => depot::push(&args[1..]),
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
