use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{Value, json};
use shell::{Failure, Repo, now};

use crate::{Build, Builds, MAX_ARTIFACTS_PER_BUILD, artifacts, file_sha256, safe_name};

const REALIZE_TIMEOUT: u64 = 1800;
const MAX_STEP_TIMEOUT: u64 = 3600;
const MAX_LOG: u64 = 64 * 1024;
const MAX_NOTES: u64 = 200 * 1024;
const MAX_PROMPT: u64 = 1024 * 1024;
const MAX_LINKS: usize = 50;

pub struct Runner {
    bwrap: PathBuf,
    nix: PathBuf,
    secrets: Option<PathBuf>,
    dir: PathBuf,
    one_at_a_time: tokio::sync::Mutex<()>,
}

impl Runner {
    pub fn new(data: &Path) -> Self {
        let program = |var: &str, name: &str| {
            std::env::var_os(var).map_or_else(|| PathBuf::from(name), PathBuf::from)
        };
        Self {
            bwrap: program("CHI_BWRAP", "bwrap"),
            nix: program("CHI_NIX", "nix"),
            secrets: std::env::var_os("CHI_SECRETS")
                .or_else(|| std::env::var_os("CREDENTIALS_DIRECTORY"))
                .map(PathBuf::from),
            dir: data.join("runs"),
            one_at_a_time: tokio::sync::Mutex::new(()),
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    name: String,
    #[serde(default)]
    run: String,
    agent: Option<AgentStep>,
    #[serde(default)]
    optional: bool,
    #[serde(default = "step_timeout")]
    timeout: u64,
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct AgentStep {
    prompt: String,
    output: String,
    #[serde(default = "claude")]
    cli: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    system: String,
}

fn step_timeout() -> u64 {
    600
}

fn claude() -> String {
    "claude".to_owned()
}

fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl Manifest {
    fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self =
            serde_json::from_str(text).map_err(|e| format!("pipeline.json: {e}"))?;
        for step in &manifest.steps {
            let files = step
                .agent
                .iter()
                .flat_map(|agent| [&agent.prompt, &agent.output]);
            let bad_run = step
                .run
                .split('/')
                .any(|part| part.is_empty() || part == "..")
                && !step.run.is_empty();
            if !plain(&step.name)
                || bad_run
                || !step.secrets.iter().all(|secret| plain(secret))
                || !files.into_iter().all(|file| safe_name(file))
            {
                return Err(format!(
                    "pipeline.json: step {:?} has a bad name, program, secret or file",
                    step.name
                ));
            }
        }
        Ok(manifest)
    }
}

fn sandbox(
    out: &Path,
    program: &Path,
    run_dir: &Path,
    artifacts: &Path,
    secrets: &[(String, PathBuf)],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--unshare-all",
        "--share-net",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--ro-bind",
        "/nix/store",
        "/nix/store",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]
    .iter()
    .map(OsString::from)
    .collect();
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
    let bind = |args: &mut Vec<OsString>, flag: &str, from: &Path, to: &str| {
        args.extend([flag.into(), from.into(), to.into()])
    };
    bind(&mut args, "--ro-bind", &run_dir.join("in"), "/chi/in");
    bind(&mut args, "--ro-bind", artifacts, "/chi/in/artifacts");
    bind(&mut args, "--bind", &run_dir.join("work"), "/chi/work");
    args.extend(["--dir".into(), "/run/chi/secrets".into()]);
    for (name, file) in secrets {
        bind(
            &mut args,
            "--ro-bind",
            file,
            &format!("/run/chi/secrets/{name}"),
        );
    }
    let env = [
        ("HOME", "/tmp".into()),
        ("TMPDIR", "/tmp".into()),
        ("LANG", "C.UTF-8".into()),
        ("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt".into()),
        ("PATH", out.join("bin").into_os_string()),
        ("CHI_IN", "/chi/in".into()),
        ("CHI_WORK", "/chi/work".into()),
        ("CHI_SECRETS", "/run/chi/secrets".into()),
    ];
    for (key, value) in env {
        args.extend(["--setenv".into(), key.into(), value]);
    }
    args.extend(["--chdir".into(), "/chi/work".into(), program.into()]);
    args
}

fn parse_links(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (label, url) = line
                .trim()
                .rsplit_once(char::is_whitespace)
                .unwrap_or(("", line.trim()));
            let ok =
                (url.starts_with("https://") || url.starts_with("http://")) && url.len() <= 2000;
            ok.then(|| {
                (
                    if label.trim().is_empty() {
                        url
                    } else {
                        label.trim()
                    }
                    .chars()
                    .take(200)
                    .collect(),
                    url.to_owned(),
                )
            })
        })
        .take(MAX_LINKS)
        .collect()
}

fn read_capped(path: &Path, max: u64) -> Option<String> {
    use std::io::Read;
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut text = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max)
        .read_to_end(&mut text)
        .ok()?;
    Some(String::from_utf8_lossy(&text).into_owned())
}

async fn replace(path: &Path, text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Err(e) = tokio::fs::remove_file(path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e);
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(text.as_bytes()).await
}

fn produced(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut produced = Vec::new();
    let real_dir = std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir());
    if let Some(Ok(entries)) = real_dir.then(|| std::fs::read_dir(dir)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if safe_name(&name) && entry.file_type().is_ok_and(|kind| kind.is_file()) {
                produced.push((name, entry.path()));
            }
        }
    }
    produced
}

fn tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let size = file.metadata().map_or(0, |meta| meta.len());
    let _ = file.seek(SeekFrom::Start(size.saturating_sub(MAX_LOG)));
    let mut text = Vec::new();
    let _ = file.read_to_end(&mut text);
    String::from_utf8_lossy(&text).trim().to_owned()
}

fn previous(conn: &Connection, build: &Build) -> rusqlite::Result<Value> {
    conn.query_row(
        "SELECT b.id, b.version, b.commit_sha, coalesce(n.text, '') FROM builds b LEFT JOIN notes n ON n.build_id = b.id
         WHERE b.repo_id = ?1 AND b.tag = ?2 AND b.status = 'finished' AND b.id < ?3 ORDER BY b.id DESC LIMIT 1",
        (build.repo_id, &build.tag, build.id),
        |row| Ok(json!({ "id": row.get::<_, i64>(0)?, "version": row.get::<_, String>(1)?, "commit": row.get::<_, String>(2)?, "notes": row.get::<_, String>(3)? })),
    )
    .optional()
    .map(Option::unwrap_or_default)
}

type Context = (String, String, String, String);

fn context(conn: &Connection, build_id: i64) -> rusqlite::Result<Context> {
    conn.query_row(
        "SELECT base, commits, diffstat, diff FROM contexts WHERE build_id = ?1",
        [build_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .map(|found| {
        found.unwrap_or_else(|| (String::new(), "[]".to_owned(), String::new(), String::new()))
    })
}

fn set_step(conn: &Connection, id: i64, status: &str, output: &str) -> rusqlite::Result<usize> {
    let stamp = now();
    conn.execute(
        "UPDATE steps SET status = ?2, output = ?3,
           started_at = CASE WHEN ?2 = 'running' THEN ?4 ELSE started_at END,
           finished_at = CASE WHEN ?2 IN ('done', 'failed') THEN ?4 ELSE finished_at END
         WHERE id = ?1",
        (id, status, output, stamp),
    )
}

impl Builds {
    pub(crate) async fn sweep_runs(&self) -> Result<(), Failure> {
        if let Err(e) = tokio::fs::remove_dir_all(&self.runner.dir).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("builds: could not clear {}: {e}", self.runner.dir.display())
        }
        self.db
            .call(|conn| {
                conn.execute(
                    "UPDATE runs SET status = 'failed', error = 'chi restarted during the run', finished_at = ?1 WHERE status = 'running'",
                    [now()],
                )?;
                conn.execute("UPDATE steps SET status = 'skipped' WHERE status IN ('pending', 'running')", [])
            })
            .await
            .map(drop)
    }

    pub(crate) async fn running(&self, build_id: i64) -> Result<bool, Failure> {
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE build_id = ?1 AND status = 'running')",
                    [build_id],
                    |row| row.get(0),
                )
            })
            .await
    }

    pub(crate) async fn pipeline(&self, build_id: i64) -> Result<(), Failure> {
        let Some(build) = self
            .build(build_id)
            .await?
            .filter(|build| build.status == "finished")
        else {
            return Ok(());
        };
        let Some(repo) = self
            .shell
            .repo(build.org_id, build.repo_id)
            .await?
            .filter(|repo| !repo.pipeline.is_empty())
        else {
            return Ok(());
        };
        let _one_at_a_time = self.runner.one_at_a_time.lock().await;
        let flake = repo.pipeline.replace("{rev}", &build.commit);
        let logged = flake.clone();
        let run_id = self
            .db
            .call(move |conn| {
                conn.execute("INSERT INTO runs (build_id, flake, status, started_at) VALUES (?1, ?2, 'running', ?3)", (build_id, logged, now()))?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        self.shell.publish(build.org_id, "builds");
        let run_dir = self.runner.dir.join(run_id.to_string());
        let outcome = if repo.pipeline.contains("{rev}") && build.commit.is_empty() {
            Err("the pipeline flake pins {rev} but the build has no commit".to_owned())
        } else {
            self.execute(run_id, &run_dir, &build, &repo, &flake).await
        };
        let _ = tokio::fs::remove_dir_all(&run_dir).await;
        let (status, error) = match outcome {
            Ok(()) => ("done", String::new()),
            Err(error) => ("failed", error),
        };
        self.db
            .call(move |conn| {
                conn.execute("UPDATE runs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1", (run_id, status, error, now()))?;
                conn.execute("UPDATE steps SET status = 'skipped' WHERE run_id = ?1 AND status IN ('pending', 'running')", [run_id])
            })
            .await?;
        self.shell.publish(build.org_id, "builds");
        Ok(())
    }

    async fn realize(&self, flake: &str) -> Result<PathBuf, String> {
        let mut command = tokio::process::Command::new(&self.runner.bwrap);
        command
            .args([
                "--unshare-all",
                "--share-net",
                "--die-with-parent",
                "--new-session",
                "--ro-bind",
                "/nix/store",
                "/nix/store",
                "--ro-bind",
                "/nix/var/nix/daemon-socket",
                "/nix/var/nix/daemon-socket",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--setenv",
                "HOME",
                "/tmp",
                "--setenv",
                "NIX_REMOTE",
                "daemon",
            ])
            .args(
                [
                    "nix",
                    "static",
                    "resolv.conf",
                    "hosts",
                    "ssl",
                    "passwd",
                    "group",
                    "nsswitch.conf",
                ]
                .iter()
                .flat_map(|etc| {
                    [
                        "--ro-bind-try".to_owned(),
                        format!("/etc/{etc}"),
                        format!("/etc/{etc}"),
                    ]
                }),
            )
            .arg(&self.runner.nix)
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "build",
                "--no-link",
                "--print-out-paths",
            ])
            .arg(format!("{flake}#chi-pipeline"))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let built = tokio::time::timeout(Duration::from_secs(REALIZE_TIMEOUT), command.output())
            .await
            .map_err(|_| "nix build timed out".to_owned())?
            .map_err(|e| format!("{}: {e}", self.runner.bwrap.display()))?;
        if !built.status.success() {
            let stderr = String::from_utf8_lossy(&built.stderr);
            return Err(format!(
                "nix build {flake}#chi-pipeline failed:\n{}",
                stderr
                    .lines()
                    .rev()
                    .take(40)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let out = String::from_utf8_lossy(&built.stdout)
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_owned();
        if !out.starts_with("/nix/store/") {
            return Err(format!("nix build printed no store path: {out:?}"));
        }
        Ok(PathBuf::from(out))
    }

    async fn prepare(&self, run_dir: &Path, build: &Build, repo: &Repo) -> Result<(), Failure> {
        let (id, home) = (
            build.id,
            self.shell
                .org_address(build.org_id)
                .await?
                .map(|(_, home)| home)
                .unwrap_or_default(),
        );
        let snapshot = build.clone();
        let (files, previous, (base, commits, diffstat, diff)) = self
            .db
            .call(move |conn| {
                Ok((
                    artifacts(conn, id)?,
                    previous(conn, &snapshot)?,
                    context(conn, id)?,
                ))
            })
            .await?;
        let described = json!({
            "id": build.id,
            "repo": repo.name,
            "repo_url": repo.url,
            "version": build.version,
            "commit": build.commit,
            "branch": build.branch,
            "tag": build.tag,
            "builder": build.builder,
            "base": base,
            "url": format!("{home}/builds/{id}"),
            "artifacts": files.iter().map(|file| json!({ "name": file.name, "size": file.size, "sha256": file.sha256, "path": format!("/chi/in/artifacts/{}", file.name) })).collect::<Vec<_>>(),
        });
        let input = run_dir.join("in");
        tokio::fs::create_dir_all(input.join("artifacts")).await?;
        tokio::fs::create_dir_all(run_dir.join("work").join("artifacts")).await?;
        tokio::fs::create_dir_all(self.build_dir(id)).await?;
        for (name, text) in [
            ("build.json", described.to_string()),
            ("previous.json", previous.to_string()),
            ("commits.json", commits),
            ("diffstat.txt", diffstat),
            ("diff.patch", diff),
        ] {
            tokio::fs::write(input.join(name), text).await?;
        }
        Ok(())
    }

    async fn execute(
        &self,
        run_id: i64,
        run_dir: &Path,
        build: &Build,
        repo: &Repo,
        flake: &str,
    ) -> Result<(), String> {
        let out = self.realize(flake).await?;
        let manifest = Manifest::parse(
            &tokio::fs::read_to_string(out.join("pipeline.json"))
                .await
                .map_err(|e| format!("pipeline.json: {e}"))?,
        )?;
        let names: Vec<String> = manifest
            .steps
            .iter()
            .map(|step| step.name.clone())
            .collect();
        let ids: Vec<i64> = self
            .db
            .call(move |conn| {
                names
                    .iter()
                    .map(|name| {
                        conn.execute(
                            "INSERT INTO steps (run_id, name, status) VALUES (?1, ?2, 'pending')",
                            (run_id, name),
                        )?;
                        Ok(conn.last_insert_rowid())
                    })
                    .collect()
            })
            .await
            .map_err(|failure| failure.1)?;
        self.prepare(run_dir, build, repo)
            .await
            .map_err(|failure| failure.1)?;
        let mut failed = None;
        for (step, id) in manifest.steps.iter().zip(ids) {
            if !step.tags.is_empty() && !step.tags.contains(&build.tag) {
                self.step(id, "skipped", "not run for this tag").await;
                continue;
            }
            self.step(id, "running", "").await;
            self.shell.publish(build.org_id, "builds");
            let (ok, output) = match &step.agent {
                Some(agent) => self.agent_step(run_dir, build, step, agent).await,
                None => self.command_step(&out, run_dir, build, repo, step).await,
            };
            self.step(id, if ok { "done" } else { "failed" }, &output)
                .await;
            if !ok && !step.optional {
                failed = Some(format!("step {} failed", step.name));
                break;
            }
        }
        self.collect(run_dir, build)
            .await
            .map_err(|failure| failure.1)?;
        failed.map_or(Ok(()), Err)
    }

    async fn step(&self, id: i64, status: &'static str, output: &str) {
        let output = output.to_owned();
        if let Err(Failure(_, message)) = self
            .db
            .call(move |conn| set_step(conn, id, status, &output))
            .await
        {
            eprintln!("builds: step {id} not saved: {message}");
        }
    }

    async fn agent_step(
        &self,
        run_dir: &Path,
        build: &Build,
        step: &Step,
        agent: &AgentStep,
    ) -> (bool, String) {
        let work = run_dir.join("work");
        let Some(prompt) = read_capped(&work.join(&agent.prompt), MAX_PROMPT) else {
            return (
                false,
                format!(
                    "no {} in the work directory: an earlier step writes the prompt",
                    agent.prompt
                ),
            );
        };
        let mut request = agents::Request::new(
            build.org_id,
            &agent.cli,
            prompt,
            &format!("build {} {}", build.id, step.name),
            "pipeline",
        );
        request.model = agent.model.clone();
        request.system = agent.system.clone();
        request.timeout = step.timeout.min(MAX_STEP_TIMEOUT) as i64;
        match self.agents.run(request).await {
            Ok(answer) => match replace(&work.join(&agent.output), &answer).await {
                Ok(()) => (
                    true,
                    format!(
                        "{} wrote {} bytes to {}",
                        agent.cli,
                        answer.len(),
                        agent.output
                    ),
                ),
                Err(error) => (false, error.to_string()),
            },
            Err(error) => (false, error),
        }
    }

    fn secrets(
        &self,
        repo: &Repo,
        org: &str,
        step: &Step,
    ) -> Result<Vec<(String, PathBuf)>, String> {
        step.secrets
            .iter()
            .map(|name| {
                let file = format!("{org}.{}.{name}", repo.name);
                let path = self
                    .runner
                    .secrets
                    .as_ref()
                    .map(|dir| dir.join(&file))
                    .filter(|path| path.is_file());
                path.map(|path| (name.clone(), path)).ok_or_else(|| {
                    format!("secret {name} is missing: put it in the chi host's secrets as {file}")
                })
            })
            .collect()
    }

    async fn command_step(
        &self,
        out: &Path,
        run_dir: &Path,
        build: &Build,
        repo: &Repo,
        step: &Step,
    ) -> (bool, String) {
        let program = out.join(if step.run.is_empty() {
            format!("bin/{}", step.name)
        } else {
            step.run.clone()
        });
        let org = self
            .shell
            .org_address(build.org_id)
            .await
            .ok()
            .flatten()
            .map(|(slug, _)| slug)
            .unwrap_or_default();
        let secrets = match self.secrets(repo, &org, step) {
            Ok(secrets) => secrets,
            Err(error) => return (false, error),
        };
        let log = run_dir.join(format!("{}.log", step.name));
        let Ok(file) = std::fs::File::create(&log) else {
            return (false, "could not create the step log".to_owned());
        };
        let Ok(errors) = file.try_clone() else {
            return (false, "could not create the step log".to_owned());
        };
        let mut command = tokio::process::Command::new(&self.runner.bwrap);
        command
            .args(sandbox(
                out,
                &program,
                run_dir,
                &self.build_dir(build.id),
                &secrets,
            ))
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(errors)
            .kill_on_drop(true);
        let limit = Duration::from_secs(step.timeout.clamp(1, MAX_STEP_TIMEOUT));
        let ran = match command.spawn() {
            Ok(mut child) => tokio::time::timeout(limit, child.wait()).await,
            Err(error) => return (false, format!("{}: {error}", self.runner.bwrap.display())),
        };
        let output = tail(&log);
        match ran {
            Ok(Ok(status)) if status.success() => (true, output),
            Ok(Ok(status)) => (false, format!("{output}\n[{status}]")),
            Ok(Err(error)) => (false, error.to_string()),
            Err(_) => (
                false,
                format!("{output}\n[timed out after {}s]", limit.as_secs()),
            ),
        }
    }

    async fn collect(&self, run_dir: &Path, build: &Build) -> Result<(), Failure> {
        let (work, dir, id) = (run_dir.join("work"), self.build_dir(build.id), build.id);
        let notes = read_capped(&work.join("notes.md"), MAX_NOTES);
        let links = read_capped(&work.join("links"), MAX_NOTES)
            .map(|text| parse_links(&text))
            .unwrap_or_default();
        let mut produced = produced(&work.join("artifacts"));
        let kept = self.db.call(move |conn| artifacts(conn, id)).await?;
        let room = MAX_ARTIFACTS_PER_BUILD.saturating_sub(
            kept.iter()
                .filter(|file| !produced.iter().any(|(name, _)| *name == file.name))
                .count(),
        );
        produced.sort();
        produced.truncate(room);
        let mut files = Vec::new();
        for (name, from) in produced {
            let to = dir.join(&name);
            tokio::fs::copy(&from, &to).await?;
            let size = tokio::fs::metadata(&to).await?.len() as i64;
            let sha256 = tokio::task::spawn_blocking(move || file_sha256(&to)).await??;
            files.push((name, size, sha256));
        }
        self.db
            .call(move |conn| {
                let tx = conn.transaction()?;
                if let Some(text) = notes.filter(|text| !text.trim().is_empty()) {
                    tx.execute("INSERT OR REPLACE INTO notes (build_id, text, at) VALUES (?1, ?2, ?3)", (id, text, now()))?;
                }
                for (label, url) in &links {
                    tx.execute("INSERT OR REPLACE INTO links (build_id, label, url) VALUES (?1, ?2, ?3)", (id, label, url))?;
                }
                for (name, size, sha256) in &files {
                    tx.execute("INSERT OR REPLACE INTO artifacts (build_id, name, size, sha256) VALUES (?1, ?2, ?3, ?4)", (id, name, size, sha256))?;
                }
                tx.commit()
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_files_are_read_back_from_the_work_dir() {
        let dir = std::env::temp_dir().join(format!("chi-read-{}", shell::secret()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plain"), "ok").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join("link")).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(dir.join("fifo"))
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(read_capped(&dir.join("plain"), 10).as_deref(), Some("ok"));
        assert_eq!(read_capped(&dir.join("link"), 10), None);
        assert_eq!(read_capped(&dir.join("fifo"), 10), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn links_need_an_http_url_last() {
        let links = parse_links(
            "Drive folder https://drive.google.com/x\nnot a link\nhttps://bare.example\nftp x ftp://y\n",
        );
        assert_eq!(
            links,
            [
                (
                    "Drive folder".to_owned(),
                    "https://drive.google.com/x".to_owned()
                ),
                (
                    "https://bare.example".to_owned(),
                    "https://bare.example".to_owned()
                )
            ]
        );
    }

    #[test]
    fn manifests_cannot_escape_their_package_or_the_work_dir() {
        assert!(Manifest::parse(r#"{"steps":[{"name":"notes","agent":{"prompt":"p.txt","output":"notes.md"}},{"name":"drive","secrets":["gdrive"]}]}"#).is_ok());
        for bad in [
            r#"{"steps":[{"name":"x","run":"../../bin/sh"}]}"#,
            r#"{"steps":[{"name":"x","run":"/bin/sh"}]}"#,
            r#"{"steps":[{"name":"a b"}]}"#,
            r#"{"steps":[{"name":"x","secrets":["../k"]}]}"#,
            r#"{"steps":[{"name":"x","agent":{"prompt":"../p","output":"o"}}]}"#,
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_sandbox_binds_secrets_read_only_and_runs_the_program_last() {
        let args = sandbox(
            Path::new("/nix/store/x"),
            Path::new("/nix/store/x/bin/s"),
            Path::new("/r"),
            Path::new("/b"),
            &[("k".to_owned(), PathBuf::from("/s/o.r.k"))],
        );
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert!(
            args.windows(3)
                .any(|w| w == ["--ro-bind", "/s/o.r.k", "/run/chi/secrets/k"])
        );
        assert!(
            args.windows(3)
                .any(|w| w == ["--bind", "/r/work", "/chi/work"])
        );
        assert!(
            args.windows(3)
                .any(|w| w == ["--setenv", "PATH", "/nix/store/x/bin"])
        );
        assert_eq!(args.last(), Some(&"/nix/store/x/bin/s"));
        assert!(
            !args
                .iter()
                .any(|arg| *arg == "--share-pid" || *arg == "/home")
        );
    }
}
