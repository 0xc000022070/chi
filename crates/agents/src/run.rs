use std::{path::Path, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::oneshot,
};

use crate::Job;

const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
const ANSWER: &str = "answer.txt";

pub enum Stop {
    Cancelled,
    Failed(String),
}

fn invocation(job: &Job) -> (Vec<String>, String) {
    let mut args: Vec<String> = Vec::new();

    let input = if job.agent == "codex" {
        args.extend(
            [
                "exec",
                "--skip-git-repo-check",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--color",
                "never",
                "-o",
                ANSWER,
            ]
            .map(String::from),
        );
        if !job.model.is_empty() {
            args.extend(["-m".to_owned(), job.model.clone()]);
        }
        args.push("-".to_owned());
        if job.system.is_empty() {
            job.prompt.clone()
        } else {
            format!("{}\n\n{}", job.system, job.prompt)
        }
    } else {
        args.extend(
            [
                "--print",
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
            ]
            .map(String::from),
        );
        if !job.model.is_empty() {
            args.extend(["--model".to_owned(), job.model.clone()]);
        }
        if !job.system.is_empty() {
            args.extend(["--system-prompt".to_owned(), job.system.clone()]);
        }
        job.prompt.clone()
    };
    (args, input)
}

async fn read_capped(
    mut from: impl AsyncRead + Unpin,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let (mut kept, mut buf, mut over) = (Vec::new(), [0u8; 8192], false);
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            return Ok((kept, over));
        }
        let room = cap - kept.len();
        kept.extend_from_slice(&buf[..n.min(room)]);
        over |= n > room;
    }
}

/// The CLI sees the store, its scratch dir and `$HOME`, where its login lives, but not chi's data,
/// credentials or environment. Killing bubblewrap takes every process the CLI started with it.
fn sandbox(dir: &Path, cli: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "--unshare-all",
        "--share-net",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--ro-bind",
        "/nix/store",
        "/nix/store",
        "--ro-bind-try",
        "/run/current-system",
        "/run/current-system",
        "--ro-bind-try",
        "/bin",
        "/bin",
        "--ro-bind-try",
        "/usr",
        "/usr",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]
    .map(Into::into)
    .into();
    for etc in [
        "resolv.conf",
        "hosts",
        "ssl",
        "static",
        "nsswitch.conf",
        "passwd",
        "group",
    ] {
        let path = format!("/etc/{etc}");
        args.extend(["--ro-bind-try".into(), path.clone().into(), path.into()]);
    }
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy();
        let kept = ["PATH", "HOME", "LANG", "TERM"].contains(&name.as_ref())
            || ["ANTHROPIC_", "CLAUDE_", "OPENAI_", "CODEX_"]
                .iter()
                .any(|prefix| name.starts_with(prefix));
        if kept {
            if name == "HOME" {
                args.extend(["--bind".into(), value.clone(), value.clone()]);
            }
            args.extend(["--setenv".into(), key, value]);
        }
    }
    args.extend([
        "--bind".into(),
        dir.into(),
        dir.into(),
        "--chdir".into(),
        dir.into(),
        cli.into(),
    ]);
    args
}

async fn execute(
    bwrap: Option<&Path>,
    cli: &Path,
    dir: &Path,
    args: Vec<String>,
    input: String,
) -> Result<(bool, Vec<u8>, bool, String), String> {
    let mut command = match bwrap {
        Some(bwrap) => {
            let mut command = Command::new(bwrap);
            command.args(sandbox(dir, cli));
            command
        }
        None => Command::new(cli),
    };
    let mut child = command
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{}: {e}", cli.display()))?;
    let (mut stdin, stdout, stderr) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let feed = async move {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        drop(stdin);
    };
    let (_, out, err, status) = tokio::join!(
        feed,
        read_capped(stdout.expect("stdout is piped"), MAX_OUTPUT),
        read_capped(stderr.expect("stderr is piped"), MAX_STDERR),
        child.wait(),
    );
    let ((out, over), (err, _)) = (
        out.map_err(|e| e.to_string())?,
        err.map_err(|e| e.to_string())?,
    );
    let status = status.map_err(|e| e.to_string())?;
    Ok((
        status.success(),
        out,
        over,
        String::from_utf8_lossy(&err).trim().to_owned(),
    ))
}

pub async fn run(
    bwrap: Option<&Path>,
    cli: &Path,
    job: &Job,
    cancelled: oneshot::Receiver<()>,
) -> Result<String, Stop> {
    let dir = std::env::temp_dir().join(format!("chi-agent-{}-{}", job.id, shell::secret()));
    std::fs::create_dir_all(&dir).map_err(|e| Stop::Failed(format!("{}: {e}", dir.display())))?;
    let (args, input) = invocation(job);
    let limit = Duration::from_secs(job.timeout.max(1).unsigned_abs());
    let outcome = tokio::select! {
        done = tokio::time::timeout(limit, execute(bwrap, cli, &dir, args, input)) => match done {
            Err(_) => Err(Stop::Failed(format!("timed out after {}s", job.timeout))),
            Ok(Err(error)) => Err(Stop::Failed(error)),
            Ok(Ok((false, _, _, stderr))) => Err(Stop::Failed(if stderr.is_empty() { "the CLI failed without a message".to_owned() } else { stderr })),
            Ok(Ok((true, _, true, _))) => Err(Stop::Failed("the answer is over 4 MiB".to_owned())),
            Ok(Ok((true, stdout, false, _))) => {
                let answer = if job.agent == "codex" { std::fs::read(dir.join(ANSWER)).unwrap_or(stdout) } else { stdout };
                Ok(String::from_utf8_lossy(&answer).trim().to_owned())
            }
        },
        Ok(()) = cancelled => Err(Stop::Cancelled),
    };
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn job(agent: &str, timeout: i64) -> Job {
        Job {
            id: 1,
            org_id: 1,
            agent: agent.to_owned(),
            model: "m".to_owned(),
            system: "be brief".to_owned(),
            prompt: "hello".to_owned(),
            source: String::new(),
            requester: String::new(),
            timeout,
            status: "running".to_owned(),
            output: String::new(),
            error: String::new(),
            created_at: 0,
            started_at: None,
            finished_at: None,
        }
    }

    fn script(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("chi-fake-{name}-{}", shell::secret()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_sandbox_hides_chi_and_keeps_the_cli_login() {
        let args: Vec<String> = sandbox(Path::new("/tmp/job"), Path::new("claude"))
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(3)
                .any(|w| w == ["--bind", "/tmp/job", "/tmp/job"])
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("credentials") || arg == "/var/lib/chi")
        );
        assert_eq!(args.last().map(String::as_str), Some("claude"));
        assert!(args.contains(&"--clearenv".to_owned()));
    }

    #[test]
    fn claude_gets_the_system_prompt_as_a_flag_and_codex_inline() {
        let (args, input) = invocation(&job("claude", 5));
        assert_eq!(
            args,
            [
                "--print",
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--model",
                "m",
                "--system-prompt",
                "be brief"
            ]
        );
        assert_eq!(input, "hello");
        let (args, input) = invocation(&job("codex", 5));
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(args.contains(&"read-only".to_owned()));
        assert_eq!(input, "be brief\n\nhello");
    }

    #[tokio::test]
    async fn the_answer_is_stdout_and_failures_carry_stderr() {
        let echo = script("echo", "cat");
        assert!(
            matches!(run(None, &echo, &job("claude", 5), oneshot::channel().1).await, Ok(answer) if answer == "hello")
        );
        let fail = script("fail", "echo broken >&2; exit 3");
        assert!(
            matches!(run(None, &fail, &job("claude", 5), oneshot::channel().1).await, Err(Stop::Failed(error)) if error == "broken")
        );
        let slow = script("slow", "sleep 5");
        assert!(
            matches!(run(None, &slow, &job("claude", 1), oneshot::channel().1).await, Err(Stop::Failed(error)) if error.starts_with("timed out"))
        );
        let (stop, cancelled) = oneshot::channel();
        stop.send(()).unwrap();
        assert!(matches!(
            run(None, &slow, &job("claude", 5), cancelled).await,
            Err(Stop::Cancelled)
        ));
        for path in [echo, fail, slow] {
            std::fs::remove_file(path).unwrap();
        }
    }
}
