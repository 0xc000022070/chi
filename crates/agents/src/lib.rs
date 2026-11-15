mod run;
mod ui;

use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    middleware,
    routing::{get, post},
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use shell::{Ctx, Db, Failure, Role, Shell, now, utc};
use tokio::sync::{Notify, broadcast, oneshot};

pub const AGENTS: [(&str, &str); 2] = [("claude", "sonnet"), ("codex", "")];
const MAX_PROMPT: usize = 1024 * 1024;
const MAX_TIMEOUT: i64 = 1800;
const DEFAULT_TIMEOUT: i64 = 300;

#[derive(Clone)]
pub struct Agents {
    db: Db,
    shell: Shell,
    wake: Arc<Notify>,
    done: broadcast::Sender<i64>,
    running: Arc<Mutex<HashMap<i64, oneshot::Sender<()>>>>,
    clis: Arc<HashMap<&'static str, PathBuf>>,
    bwrap: Option<Arc<PathBuf>>,
}

pub struct Request {
    pub org_id: i64,
    pub agent: String,
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub source: String,
    pub requester: String,
    pub timeout: i64,
}

impl Request {
    pub fn new(org_id: i64, agent: &str, prompt: String, source: &str, requester: &str) -> Self {
        Self {
            org_id,
            agent: agent.to_owned(),
            model: String::new(),
            system: String::new(),
            prompt,
            source: source.to_owned(),
            requester: requester.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

pub struct Job {
    pub id: i64,
    pub org_id: i64,
    pub agent: String,
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub source: String,
    pub requester: String,
    pub timeout: i64,
    pub status: String,
    pub output: String,
    pub error: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

const JOB_COLUMNS: &str = "id, org_id, agent, model, system, prompt, source, requester, timeout, status, output, error, created_at, started_at, finished_at";

fn job_row(row: &rusqlite::Row) -> rusqlite::Result<Job> {
    let (
        id,
        org_id,
        agent,
        model,
        system,
        prompt,
        source,
        requester,
        timeout,
        status,
        output,
        error,
        created_at,
        started_at,
        finished_at,
    ) = <(
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        i64,
        Option<i64>,
        Option<i64>,
    )>::try_from(row)?;
    Ok(Job {
        id,
        org_id,
        agent,
        model,
        system,
        prompt,
        source,
        requester,
        timeout,
        status,
        output,
        error,
        created_at,
        started_at,
        finished_at,
    })
}

fn claim(conn: &rusqlite::Connection) -> rusqlite::Result<Option<Job>> {
    conn.query_row(
        &format!(
            "UPDATE jobs SET status = 'running', started_at = ?1
             WHERE id = (SELECT id FROM jobs WHERE status = 'queued' ORDER BY id LIMIT 1)
             RETURNING {JOB_COLUMNS}"
        ),
        [now()],
        job_row,
    )
    .optional()
}

impl Agents {
    pub fn open(data: &FsPath, shell: Shell) -> Result<Self, String> {
        let db = Db::open(&data.join("agents.db"), include_str!("schema.sql"))
            .map_err(|e| e.to_string())?;
        let clis = AGENTS
            .iter()
            .map(|(name, _)| {
                (
                    *name,
                    std::env::var(format!("CHI_{}", name.to_uppercase()))
                        .map_or_else(|_| PathBuf::from(name), PathBuf::from),
                )
            })
            .collect();
        let slots = std::env::var("CHI_AGENT_SLOTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2usize)
            .max(1);
        let agents = Self {
            db,
            shell,
            wake: Arc::default(),
            done: broadcast::channel(64).0,
            running: Arc::default(),
            clis: Arc::new(clis),
            bwrap: match std::env::var_os("CHI_BWRAP") {
                None => Some(Arc::new(PathBuf::from("bwrap"))),
                Some(path) if path.is_empty() => None,
                Some(path) => Some(Arc::new(PathBuf::from(path))),
            },
        };
        let interrupted = agents.clone();
        tokio::spawn(async move {
            let swept = interrupted
                .db
                .call(|conn| {
                    conn.execute(
                        "UPDATE jobs SET status = 'failed', error = 'chi restarted while this job ran', finished_at = ?1 WHERE status = 'running'",
                        [now()],
                    )
                })
                .await;
            if let Err(Failure(_, message)) = swept {
                eprintln!("agents: could not fail interrupted jobs: {message}");
            }
            for _ in 0..slots {
                tokio::spawn(interrupted.clone().work());
            }
        });
        Ok(agents)
    }

    async fn work(self) {
        loop {
            match self.db.call(|conn| claim(conn)).await {
                Ok(Some(job)) => self.execute(job).await,
                Ok(None) => {
                    drop(tokio::time::timeout(Duration::from_secs(30), self.wake.notified()).await)
                }
                Err(Failure(_, message)) => {
                    eprintln!("agents: claim failed: {message}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn execute(&self, job: Job) {
        let (cancel, cancelled) = oneshot::channel();
        self.running
            .lock()
            .expect("no holder of the running map panics")
            .insert(job.id, cancel);
        self.shell.publish(job.org_id, "agents");
        let cli = self
            .clis
            .get(job.agent.as_str())
            .cloned()
            .unwrap_or_else(|| PathBuf::from(&job.agent));
        let outcome = run::run(
            self.bwrap.as_deref().map(PathBuf::as_path),
            &cli,
            &job,
            cancelled,
        )
        .await;
        self.running
            .lock()
            .expect("no holder of the running map panics")
            .remove(&job.id);
        let (id, org_id) = (job.id, job.org_id);
        let (status, output, error) = match outcome {
            Ok(output) => ("done", output, String::new()),
            Err(run::Stop::Cancelled) => ("cancelled", String::new(), "cancelled".to_owned()),
            Err(run::Stop::Failed(error)) => ("failed", String::new(), error),
        };
        let saved = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE jobs SET status = ?2, output = ?3, error = ?4, finished_at = ?5 WHERE id = ?1",
                    (id, status, output, error, now()),
                )
            })
            .await;
        if let Err(Failure(_, message)) = saved {
            eprintln!("agents: job {id} result lost: {message}");
        }
        let _ = self.done.send(id);
        self.shell.publish(org_id, "agents");
    }

    pub async fn submit(&self, request: Request) -> Result<i64, Failure> {
        if !AGENTS.iter().any(|(name, _)| *name == request.agent) {
            return Err(Failure::bad_request("agent is claude or codex"));
        }
        if request.prompt.trim().is_empty()
            || request.prompt.len() + request.system.len() > MAX_PROMPT
            || request.model.len() > 100
        {
            return Err(Failure::bad_request(
                "a prompt is required, at most 1 MiB, and a model name is short",
            ));
        }
        let model = if request.model.is_empty() {
            AGENTS
                .iter()
                .find(|(name, _)| *name == request.agent)
                .map_or("", |(_, model)| model)
                .to_owned()
        } else {
            request.model
        };
        let org_id = request.org_id;
        let id = self
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs (org_id, agent, model, system, prompt, source, requester, timeout, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    (
                        request.org_id,
                        request.agent,
                        model,
                        request.system,
                        request.prompt,
                        request.source,
                        request.requester,
                        request.timeout.clamp(10, MAX_TIMEOUT),
                        now(),
                    ),
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        self.wake.notify_one();
        self.shell.publish(org_id, "agents");
        Ok(id)
    }

    pub async fn job(&self, id: i64) -> Result<Option<Job>, Failure> {
        self.db
            .call(move |conn| {
                conn.query_row(
                    &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                    [id],
                    job_row,
                )
                .optional()
            })
            .await
    }

    pub async fn wait(&self, id: i64) -> Result<Job, Failure> {
        let mut finished = self.done.subscribe();
        loop {
            let job = self.job(id).await?.ok_or_else(Failure::not_found)?;
            if !matches!(job.status.as_str(), "queued" | "running") {
                return Ok(job);
            }
            let _ = tokio::time::timeout(Duration::from_secs(15), finished.recv()).await;
        }
    }

    pub async fn run(&self, request: Request) -> Result<String, String> {
        let id = self.submit(request).await.map_err(|failure| failure.1)?;
        let job = self.wait(id).await.map_err(|failure| failure.1)?;
        if job.status == "done" {
            Ok(job.output)
        } else {
            Err(format!("agent job {id} {}: {}", job.status, job.error))
        }
    }

    pub async fn cancel(&self, org_id: i64, id: i64) -> Result<(), Failure> {
        let queued = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE jobs SET status = 'cancelled', error = 'cancelled', finished_at = ?3 WHERE id = ?1 AND org_id = ?2 AND status = 'queued'",
                    (id, org_id, now()),
                )
            })
            .await?;
        if queued == 0 {
            let job = self
                .job(id)
                .await?
                .filter(|job| job.org_id == org_id)
                .ok_or_else(Failure::not_found)?;
            if job.status == "running"
                && let Some(stop) = self
                    .running
                    .lock()
                    .expect("no holder of the running map panics")
                    .remove(&id)
            {
                let _ = stop.send(());
            }
        } else {
            let _ = self.done.send(id);
        }
        self.shell.publish(org_id, "agents");
        Ok(())
    }

    pub fn ui(&self) -> Router {
        shell::mount("/agents", ui::routes(), self.clone())
    }

    pub fn api(&self) -> Router {
        Router::new()
            .route("/api/agents/jobs", post(api_submit))
            .route("/api/agents/jobs/{id}", get(api_job))
            .route_layer(middleware::from_fn_with_state(
                self.shell.clone(),
                shell::bearer,
            ))
            .with_state(self.clone())
    }

    pub fn job_json(&self, ctx: &Ctx, job: &Job) -> Value {
        json!({
            "id": job.id,
            "agent": job.agent,
            "model": job.model,
            "status": job.status,
            "source": job.source,
            "requester": job.requester,
            "system": job.system,
            "prompt": job.prompt,
            "output": job.output,
            "error": job.error,
            "created": utc(job.created_at),
            "started": job.started_at.map(utc),
            "finished": job.finished_at.map(utc),
            "url": self.shell.url(&ctx.href(&format!("/agents/{}", job.id))),
        })
    }

    pub async fn jobs_json(&self, ctx: &Ctx, status: &str, limit: i64) -> Result<Value, Failure> {
        let jobs = self.jobs(ctx.org_id, status, limit).await?;
        Ok(jobs
            .iter()
            .map(|job| {
                let mut found = self.job_json(ctx, job);
                for long in ["system", "prompt", "output"] {
                    found.as_object_mut().map(|object| object.remove(long));
                }
                found
            })
            .collect())
    }

    pub async fn one_job_json(&self, ctx: &Ctx, id: i64) -> Result<Value, Failure> {
        let job = self
            .job(id)
            .await?
            .filter(|job| job.org_id == ctx.org_id)
            .ok_or_else(Failure::not_found)?;
        Ok(self.job_json(ctx, &job))
    }

    async fn jobs(&self, org_id: i64, status: &str, limit: i64) -> Result<Vec<Job>, Failure> {
        let status = status.to_owned();
        self.db
            .call(move |conn| {
                conn.prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE org_id = ?1 AND (?2 = '' OR status = ?2) ORDER BY id DESC LIMIT ?3"
                ))?
                .query_map((org_id, status, limit), job_row)?
                .collect()
            })
            .await
    }

    pub async fn ask(&self, ctx: &Ctx, args: &Value, source: &str) -> Result<Value, Failure> {
        ctx.require(Role::Maintainer)?;
        let text = |key: &str| args[key].as_str().unwrap_or("").to_owned();
        let request = Request {
            org_id: ctx.org_id,
            agent: args["agent"].as_str().unwrap_or("claude").to_owned(),
            model: text("model"),
            system: text("system"),
            prompt: text("prompt"),
            source: source.to_owned(),
            requester: ctx.email.clone(),
            timeout: args["timeout"].as_i64().unwrap_or(DEFAULT_TIMEOUT),
        };
        let id = self.submit(request).await?;
        if args["wait"].as_bool() == Some(false) {
            return self.one_job_json(ctx, id).await;
        }
        let job = self.wait(id).await?;
        Ok(self.job_json(ctx, &job))
    }
}

#[derive(Deserialize)]
struct ApiJob {
    org: String,
    #[serde(flatten)]
    fields: Value,
}

async fn api_submit(
    State(agents): State<Agents>,
    Extension(orgs): Extension<Vec<Ctx>>,
    Json(body): Json<ApiJob>,
) -> Result<Json<Value>, Failure> {
    let ctx = orgs
        .iter()
        .find(|ctx| ctx.slug == body.org)
        .ok_or_else(Failure::not_found)?;
    Ok(Json(agents.ask(ctx, &body.fields, "api").await?))
}

async fn api_job(
    State(agents): State<Agents>,
    Extension(orgs): Extension<Vec<Ctx>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, Failure> {
    let job = agents.job(id).await?.ok_or_else(Failure::not_found)?;
    let ctx = orgs
        .iter()
        .find(|ctx| ctx.org_id == job.org_id)
        .ok_or_else(Failure::not_found)?;
    Ok(Json(agents.job_json(ctx, &job)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_claimed_once_in_order() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        for prompt in ["a", "b"] {
            conn.execute(
                "INSERT INTO jobs (org_id, agent, prompt, created_at) VALUES (1, 'claude', ?1, 0)",
                [prompt],
            )
            .unwrap();
        }
        assert_eq!(claim(&conn).unwrap().unwrap().prompt, "a");
        assert_eq!(claim(&conn).unwrap().unwrap().prompt, "b");
        assert!(claim(&conn).unwrap().is_none());
    }
}
