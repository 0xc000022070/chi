mod feed;
mod github;
mod linear;
mod mcp;
mod pages;
mod query;
mod store;
mod ui;

use std::{path::Path as FsPath, path::PathBuf, sync::Arc};

use agents::{Agents, Request};
use axum::Router;
use bugs::Bugs;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use shell::{Ctx, Db, Failure, Person, Role, Shell, identifiers, secret};

pub use linear::import as import_linear;
pub use feed::SCOPES;
pub use mcp::tools;
pub use store::Issue;
use store::{Saved, atomic};

pub const STATUSES: [&str; 7] = [
    "backlog",
    "todo",
    "in_progress",
    "in_review",
    "done",
    "cancelled",
    "duplicate",
];
pub const OPEN: [&str; 4] = ["backlog", "todo", "in_progress", "in_review"];
pub const PRIORITIES: [&str; 5] = ["none", "urgent", "high", "medium", "low"];
const MAX_UPLOAD: usize = 25 * 1024 * 1024;
const MAX_COMMENT: usize = 100_000;
const INLINE: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const AGENT_SYSTEM: &str = "You are {agent}, a teammate in the chi issue tracker. You cannot run tools, read files or change the issue: \
you only write the comment that will be posted under your name. Be concrete and brief, use Markdown, refer to issues by their ids.";

#[derive(Clone)]
pub struct Tracker {
    db: Db,
    shell: Shell,
    agents: Agents,
    bugs: Bugs,
    files: Arc<PathBuf>,
    github_secret: Option<Arc<str>>,
}

pub struct People(pub Vec<Person>);

impl People {
    pub fn find(&self, who: &str) -> Option<&Person> {
        let who = who.trim().trim_start_matches('@');
        let mut handles = self
            .0
            .iter()
            .filter(|person| person.handle().eq_ignore_ascii_case(who));
        self.0
            .iter()
            .find(|person| person.email.eq_ignore_ascii_case(who))
            .or_else(|| handles.next().filter(|_| handles.next().is_none()))
    }

    pub fn get(&self, id: Option<i64>) -> Option<&Person> {
        id.and_then(|id| self.0.iter().find(|person| person.id == id))
    }

    pub fn name(&self, id: Option<i64>) -> String {
        match id {
            None => String::new(),
            Some(id) => self
                .get(Some(id))
                .map_or_else(|| format!("user {id}"), |person| person.handle().to_owned()),
        }
    }

    pub fn mentioned(&self, text: &str) -> Vec<&Person> {
        let mut found: Vec<&Person> = Vec::new();
        for (i, _) in text.match_indices('@') {
            if text[..i]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace() && c != '(')
            {
                continue;
            }
            let handle: String = text[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .collect();
            if let Some(person) = self.find(handle.trim_end_matches('.'))
                && !found.iter().any(|seen| seen.id == person.id)
            {
                found.push(person);
            }
        }
        found
    }
}

pub fn split_identifier(text: &str) -> Option<(String, i64)> {
    let text = text.trim().to_ascii_uppercase();
    if identifiers(&text) != [(0, text.len())] {
        return None;
    }
    let (prefix, number) = text.rsplit_once('-')?;
    Some((prefix.to_owned(), number.parse().ok()?))
}

fn loose<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Number(f64),
        Text(String),
    }
    Ok(
        Option::<Loose>::deserialize(deserializer)?.map(|value| match value {
            Loose::Number(number) => number.to_string(),
            Loose::Text(text) => text,
        }),
    )
}

fn label_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Labels {
        List(Vec<String>),
        Text(String),
    }
    Ok(
        Option::<Labels>::deserialize(deserializer)?.map(|labels| match labels {
            Labels::List(list) => list,
            Labels::Text(text) => text.split(',').map(str::to_owned).collect(),
        }),
    )
}

#[derive(Deserialize, Default)]
pub struct Changes {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    #[serde(default, deserialize_with = "loose")]
    pub priority: Option<String>,
    #[serde(default, deserialize_with = "loose")]
    pub estimate: Option<String>,
    pub assignee: Option<String>,
    pub project: Option<String>,
    pub milestone: Option<String>,
    pub parent: Option<String>,
    pub due_date: Option<String>,
    #[serde(default, deserialize_with = "label_list")]
    pub labels: Option<Vec<String>>,
    pub sort: Option<f64>,
}

fn problem(message: String) -> Failure {
    Failure::bad_request(&message)
}

fn notify_saved(
    conn: &rusqlite::Connection,
    org_id: i64,
    issue_id: i64,
    actor: Option<i64>,
    title: &str,
    mentioned: Vec<i64>,
    assigned: Vec<i64>,
) -> rusqlite::Result<(Vec<i64>, Vec<i64>, Vec<i64>)> {
    for user in &mentioned {
        store::subscribe(conn, issue_id, *user)?;
    }
    let watchers = store::subscribers(conn, issue_id)?;
    store::notify(
        conn,
        org_id,
        &assigned,
        issue_id,
        actor,
        &format!("assigned you {title}"),
    )?;
    let mentioned: Vec<i64> = mentioned
        .into_iter()
        .filter(|user| !assigned.contains(user))
        .collect();
    store::notify(
        conn,
        org_id,
        &mentioned,
        issue_id,
        actor,
        &format!("mentioned you in {title}"),
    )?;
    Ok((watchers, assigned, mentioned))
}

fn notify_comment(
    tx: &rusqlite::Transaction,
    org_id: i64,
    issue_id: i64,
    key: &str,
    author: Option<i64>,
    humans: &[i64],
    real: &[i64],
) -> rusqlite::Result<()> {
    let watchers: Vec<i64> = store::subscribers(tx, issue_id)?
        .into_iter()
        .filter(|user| Some(*user) != author && !humans.contains(user) && real.contains(user))
        .collect();
    store::notify(
        tx,
        org_id,
        humans,
        issue_id,
        author,
        &format!("mentioned you in {key}"),
    )?;
    store::notify(
        tx,
        org_id,
        &watchers,
        issue_id,
        author,
        &format!("commented on {key}"),
    )
}

fn error_description(error_id: i64, error: &Value) -> String {
    let text = |key: &str| error[key].as_str().unwrap_or("").to_owned();
    let mut description = format!(
        "Reported error [#{error_id}]({}).\n\n- level: {}\n- events: {}\n- first seen: {}\n- last seen: {}\n- project: {}\n",
        text("url"),
        text("level"),
        error["events"],
        text("first_seen"),
        text("last_seen"),
        text("project"),
    );
    let exception = &error["latest_event"]["exception"];
    if !exception.is_null() {
        let mut dump = serde_json::to_string_pretty(exception).unwrap_or_default();
        if dump.len() > 8000 {
            let mut cut = 8000;
            while !dump.is_char_boundary(cut) {
                cut -= 1;
            }
            dump.truncate(cut);
            dump += "\n…";
        }
        description += &format!("\n```json\n{dump}\n```\n");
    }
    description
}

fn describe(field: &str, old: &str, new: &str) -> String {
    match (field, old.is_empty(), new.is_empty()) {
        ("description", _, _) => "edited the description".to_owned(),
        ("relation", true, _) => format!("added relation {new}"),
        ("relation", _, _) => format!("removed relation {old}"),
        ("link", _, _) => format!("linked {new}"),
        (field, true, _) => format!("set {field} to {new}"),
        (field, _, true) => format!("cleared {field} (was {old})"),
        (field, _, _) => format!("{field}: {old} → {new}"),
    }
}

impl Tracker {
    pub fn open(data: &FsPath, shell: Shell, agents: Agents, bugs: Bugs) -> Result<Self, String> {
        let db = Db::open(&data.join("tracker.db"), include_str!("schema.sql"))
            .map_err(|e| e.to_string())?;
        let files = data.join("tracker").join("files");
        std::fs::create_dir_all(&files).map_err(|e| format!("{}: {e}", files.display()))?;
        let github_secret = std::env::var("CHI_GITHUB_SECRET")
            .ok()
            .or_else(|| {
                let dir = std::env::var("CREDENTIALS_DIRECTORY").ok()?;
                std::fs::read_to_string(PathBuf::from(dir).join("github-secret")).ok()
            })
            .map(|secret| secret.trim().to_owned())
            .filter(|secret| !secret.is_empty())
            .map(Arc::from);
        let tracker = Self {
            db,
            shell,
            agents,
            bugs,
            files: Arc::new(files),
            github_secret,
        };
        let starter = tracker.shell.clone();
        tokio::spawn(async move {
            for (name, _) in agents::AGENTS {
                if let Err(Failure(_, message)) = starter.agent(name).await {
                    eprintln!("tracker: could not create the {name} user: {message}");
                }
            }
        });
        Ok(tracker)
    }

    pub fn ui(&self) -> Router {
        ui::routes().merge(pages::routes()).with_state(self.clone())
    }

    pub fn api(&self) -> Router {
        github::routes().with_state(self.clone())
    }

    pub async fn people(&self, org_id: i64) -> Result<People, Failure> {
        Ok(People(self.shell.people(org_id).await?))
    }

    pub async fn issue(&self, ctx: &Ctx, key: &str) -> Result<Issue, Failure> {
        let (org_id, key) = (ctx.org_id, key.to_owned());
        self.db
            .call(move |conn| store::issue_by_key(conn, org_id, &key))
            .await?
            .ok_or_else(Failure::not_found)
    }

    pub async fn list(
        &self,
        ctx: &Ctx,
        text: &str,
        default_order: &str,
        limit: i64,
    ) -> Result<Vec<Issue>, Failure> {
        let who = self.people(ctx.org_id).await?;
        let compiled = query::compile(text, ctx.user_id, &who).map_err(problem)?;
        let order = compiled
            .order
            .or_else(|| query::order(default_order))
            .unwrap_or("i.updated_at DESC");
        let org_id = ctx.org_id;
        self.db
            .call(move |conn| {
                store::issues(
                    conn,
                    org_id,
                    &compiled.condition,
                    compiled.params,
                    order,
                    limit,
                )
            })
            .await
    }

    pub async fn save(
        &self,
        ctx: &Ctx,
        key: Option<&str>,
        changes: Changes,
    ) -> Result<Issue, Failure> {
        ctx.require(Role::Maintainer)?;
        let id = match key {
            Some(key) => Some(self.issue(ctx, key).await?.id),
            None => None,
        };
        self.write(ctx.org_id, Some(ctx.user_id), &ctx.email, id, changes, None)
            .await
    }

    async fn write(
        &self,
        org_id: i64,
        actor: Option<i64>,
        requester: &str,
        id: Option<i64>,
        changes: Changes,
        error_ref: Option<i64>,
    ) -> Result<Issue, Failure> {
        let who = Arc::new(self.people(org_id).await?);
        let people = who.clone();
        let saved = self
            .db
            .call(move |conn| {
                atomic(conn, |tx| {
                    store::save(tx, org_id, actor, &people, id, changes, error_ref)
                })
            })
            .await?
            .map_err(problem)?;
        self.after_save(org_id, actor, requester, &who, &saved)
            .await?;
        Ok(saved.issue)
    }

    async fn after_save(
        &self,
        org_id: i64,
        actor: Option<i64>,
        requester: &str,
        who: &People,
        saved: &Saved,
    ) -> Result<(), Failure> {
        let issue = &saved.issue;
        let key = issue.identifier();
        let human =
            |id: &i64| Some(*id) != actor && who.get(Some(*id)).is_some_and(|person| !person.agent);
        let mentioned: Vec<i64> = if saved.created
            || saved
                .changes
                .iter()
                .any(|change| change.field == "description")
        {
            who.mentioned(&issue.description)
                .iter()
                .map(|person| person.id)
                .filter(human)
                .collect()
        } else {
            Vec::new()
        };
        let assigned: Vec<i64> = saved.assigned.into_iter().filter(human).collect();
        let summary = saved
            .changes
            .iter()
            .filter(|change| change.field != "assignee")
            .map(|change| describe(change.field, &change.old, &change.new))
            .collect::<Vec<_>>()
            .join("; ");
        let (issue_id, created, title) =
            (issue.id, saved.created, format!("{key}: {}", issue.title));
        let (watchers, assigned, mentioned) = self
            .db
            .call(move |conn| {
                notify_saved(conn, org_id, issue_id, actor, &title, mentioned, assigned)
            })
            .await?;
        if !created && !summary.is_empty() {
            let rest: Vec<i64> = watchers
                .into_iter()
                .filter(|user| human(user) && !assigned.contains(user) && !mentioned.contains(user))
                .collect();
            let text = format!("{summary} on {key}");
            self.db
                .call(move |conn| store::notify(conn, org_id, &rest, issue_id, actor, &text))
                .await?;
        }
        if let Some(agent) = who
            .get(saved.assigned)
            .filter(|person| person.agent && Some(person.id) != actor)
        {
            let ask = "You were assigned this issue. Say what it needs: the likely cause, a plan, and open questions.";
            self.reply(
                org_id,
                issue.clone(),
                agent.handle().to_owned(),
                requester.to_owned(),
                ask.to_owned(),
            );
        }
        self.shell.publish(org_id, "issues");
        let changes: Vec<Value> = saved
            .changes
            .iter()
            .map(|change| json!({ "field": change.field, "old": change.old, "new": change.new }))
            .collect();
        self.event(
            org_id,
            if created {
                "issue.created"
            } else {
                "issue.updated"
            },
            issue,
            json!({ "changes": changes }),
        );
        Ok(())
    }

    fn event(&self, org_id: i64, kind: &'static str, issue: &Issue, extra: Value) {
        let shell = self.shell.clone();
        let (key, title, status) = (
            issue.identifier(),
            issue.title.clone(),
            issue.status.clone(),
        );
        tokio::spawn(async move {
            let Ok(Some((org, home))) = shell.org_address(org_id).await else {
                return;
            };
            let mut event = json!({ "type": kind, "org": org, "issue": key, "title": title, "status": status, "url": format!("{home}/issues/{key}") });
            if let (Some(event), Value::Object(extra)) = (event.as_object_mut(), extra) {
                event.extend(extra);
            }
            shell.run_hooks(&event).await;
        });
    }

    pub async fn comment(
        &self,
        org_id: i64,
        issue: &Issue,
        author: Option<i64>,
        requester: &str,
        body: &str,
    ) -> Result<i64, Failure> {
        let body = body.trim().to_owned();
        if body.is_empty() || body.len() > MAX_COMMENT {
            return Err(Failure::bad_request(
                "a comment is between 1 and 100000 characters",
            ));
        }
        let who = self.people(org_id).await?;
        let by_agent = who.get(author).is_some_and(|person| person.agent);
        let mentioned = who.mentioned(&body);
        let humans: Vec<i64> = mentioned
            .iter()
            .filter(|person| !person.agent && Some(person.id) != author)
            .map(|person| person.id)
            .collect();
        let agents: Vec<String> = if by_agent {
            Vec::new()
        } else {
            mentioned
                .iter()
                .filter(|person| person.agent)
                .map(|person| person.handle().to_owned())
                .collect()
        };
        let real: Vec<i64> = who
            .0
            .iter()
            .filter(|person| !person.agent)
            .map(|person| person.id)
            .collect();
        let (issue_id, key) = (issue.id, issue.identifier());
        let comment = body.clone();
        let id = self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                let id = store::add_comment(&tx, issue_id, author, &comment)?;
                let subscriber = author.filter(|_| !by_agent);
                for user in subscriber.iter().chain(&humans) {
                    store::subscribe(&tx, issue_id, *user)?;
                }
                notify_comment(&tx, org_id, issue_id, &key, author, &humans, &real)?;
                tx.commit()?;
                Ok(id)
            })
            .await?;
        self.shell.publish(org_id, "issues");
        let author_name = who.name(author);
        self.event(
            org_id,
            "comment.created",
            issue,
            json!({ "author": author_name, "body": body }),
        );
        for agent in agents {
            self.reply(
                org_id,
                issue.clone(),
                agent,
                requester.to_owned(),
                body.clone(),
            );
        }
        Ok(id)
    }

    fn reply(&self, org_id: i64, issue: Issue, agent: String, requester: String, ask: String) {
        let tracker = self.clone();
        tokio::spawn(async move {
            let issue_id = issue.id;
            let thread = tracker
                .db
                .call(move |conn| store::comments(conn, issue_id))
                .await
                .unwrap_or_default();
            let who = tracker.people(org_id).await.unwrap_or(People(Vec::new()));
            let mut prompt = format!(
                "Issue {}: {}\nStatus: {}. Priority: {}. Labels: {}. Assignee: {}.\n\n{}\n",
                issue.identifier(),
                issue.title,
                issue.status,
                issue.priority_name(),
                issue.labels.join(", "),
                who.name(issue.assignee),
                issue.description
            );
            for comment in thread.iter().rev().take(30).rev() {
                prompt += &format!(
                    "\n--- @{} wrote:\n{}\n",
                    who.name(comment.author),
                    comment.body
                );
            }
            prompt += &format!("\n--- What you are asked now:\n{ask}\n");
            let mut request = Request::new(org_id, &agent, prompt, "tracker", &requester);
            request.system = AGENT_SYSTEM.replace("{agent}", &agent);
            let answer = tracker
                .agents
                .run(request)
                .await
                .unwrap_or_else(|error| format!("I could not answer: {error}"));
            let posted = match tracker.shell.agent(&agent).await {
                Ok(author) => tracker
                    .comment(org_id, &issue, Some(author), &requester, &answer)
                    .await
                    .map(drop),
                Err(failure) => Err(failure),
            };
            if let Err(Failure(_, message)) = posted {
                eprintln!(
                    "tracker: {agent} could not answer on {}: {message}",
                    issue.identifier()
                );
            }
        });
    }

    pub async fn from_error(&self, ctx: &Ctx, error_id: i64) -> Result<Issue, Failure> {
        ctx.require(Role::Maintainer)?;
        let org_id = ctx.org_id;
        let existing = self
            .db
            .call(move |conn| {
                let id = conn.query_row(
                    "SELECT id FROM issues WHERE org_id = ?1 AND error_ref = ?2",
                    (org_id, error_id),
                    |row| row.get(0),
                );
                match rusqlite::OptionalExtension::optional(id)? {
                    Some(id) => store::issue_by_id(conn, org_id, id),
                    None => Ok(None),
                }
            })
            .await?;
        if let Some(issue) = existing {
            return Ok(issue);
        }
        let error = self.bugs.issue_json(ctx, error_id).await?;
        let text = |key: &str| error[key].as_str().unwrap_or("").to_owned();
        let description = error_description(error_id, &error);
        let repo_id = match error["repo"].as_str() {
            Some(url) => self
                .shell
                .repos(org_id)
                .await?
                .into_iter()
                .find(|repo| repo.url == url)
                .map(|repo| repo.id),
            None => None,
        };
        let project = match repo_id {
            Some(repo_id) => self
                .db
                .call(move |conn| store::projects(conn, org_id))
                .await?
                .into_iter()
                .find(|p| p.repo_id == Some(repo_id))
                .map(|p| p.prefix),
            None => None,
        };
        let priority = match text("level").as_str() {
            "fatal" => "urgent",
            "error" => "high",
            _ => "medium",
        };
        let changes = Changes {
            title: Some(text("title").chars().take(500).collect()),
            description: Some(description),
            priority: Some(priority.to_owned()),
            labels: Some(vec!["bug".to_owned()]),
            project,
            status: Some("todo".to_owned()),
            ..Changes::default()
        };
        self.write(
            org_id,
            Some(ctx.user_id),
            &ctx.email,
            None,
            changes,
            Some(error_id),
        )
        .await
    }

    pub async fn upload(
        &self,
        ctx: &Ctx,
        issue: &Issue,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<i64, Failure> {
        ctx.require(Role::Maintainer)?;
        let name: String = name
            .trim()
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '"' | '\\' | '/'))
            .take(200)
            .collect();
        if name.is_empty() || bytes.is_empty() || bytes.len() > MAX_UPLOAD {
            return Err(Failure::bad_request("a file needs a name and up to 25 MB"));
        }
        let file = secret();
        tokio::fs::write(self.files.join(&file), &bytes).await?;
        let (org_id, issue_id, user, size) =
            (ctx.org_id, issue.id, ctx.user_id, bytes.len() as i64);
        let mime = mime
            .chars()
            .filter(|c| c.is_ascii_graphic())
            .take(100)
            .collect::<String>();
        let id = self
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO attachments (org_id, issue_id, title, file, mime, size, uploader, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    (org_id, issue_id, name, file, mime, size, user, shell::now()),
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        self.shell.publish(org_id, "issues");
        Ok(id)
    }

    pub async fn attach_link(
        &self,
        ctx: &Ctx,
        issue: &Issue,
        title: &str,
        url: &str,
    ) -> Result<i64, Failure> {
        ctx.require(Role::Maintainer)?;
        let (title, url) = (title.trim().to_owned(), url.trim().to_owned());
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() > 2000 {
            return Err(Failure::bad_request("a link is an http or https url"));
        }
        let title = if title.is_empty() {
            url.clone()
        } else {
            title.chars().take(200).collect()
        };
        let (org_id, issue_id, user) = (ctx.org_id, issue.id, ctx.user_id);
        let id = self
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO attachments (org_id, issue_id, title, url, uploader, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (org_id, issue_id, title, url, user, shell::now()),
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        self.shell.publish(org_id, "issues");
        Ok(id)
    }

    pub async fn relate(
        &self,
        ctx: &Ctx,
        issue: &Issue,
        kind: &str,
        other: &str,
        remove: bool,
    ) -> Result<(), Failure> {
        ctx.require(Role::Maintainer)?;
        if !store::RELATIONS.contains(&kind) {
            return Err(Failure::bad_request(
                "a relation is blocks, related or duplicate",
            ));
        }
        let other = self.issue(ctx, other).await?;
        if other.id == issue.id {
            return Err(Failure::bad_request("an issue cannot relate to itself"));
        }
        let (a, b, kind, user, label) = (
            issue.id,
            other.id,
            kind.to_owned(),
            ctx.user_id,
            other.identifier(),
        );
        self.db
            .call(move |conn| {
                let changed = if remove {
                    conn.execute("DELETE FROM relations WHERE kind = ?3 AND ((issue_id = ?1 AND other_id = ?2) OR (issue_id = ?2 AND other_id = ?1))", (a, b, &kind))?
                } else {
                    conn.execute("INSERT OR IGNORE INTO relations (issue_id, kind, other_id) VALUES (?1, ?2, ?3)", (a, &kind, b))?
                };
                if changed > 0 {
                    let (old, new) = if remove { (format!("{kind} {label}"), String::new()) } else { (String::new(), format!("{kind} {label}")) };
                    conn.execute("INSERT INTO history (issue_id, actor, field, old, new, at) VALUES (?1, ?2, 'relation', ?3, ?4, ?5)", (a, user, old, new, shell::now()))?;
                }
                Ok(())
            })
            .await?;
        self.shell.publish(ctx.org_id, "issues");
        Ok(())
    }

    async fn change<T: Send + 'static>(
        &self,
        ctx: &Ctx,
        topic: &str,
        write: impl FnOnce(&rusqlite::Connection) -> Result<T, store::Problem> + Send + 'static,
    ) -> Result<T, Failure> {
        ctx.require(Role::Maintainer)?;
        let value = self
            .db
            .call(move |conn| atomic(conn, write))
            .await?
            .map_err(problem)?;
        self.shell.publish(ctx.org_id, topic);
        Ok(value)
    }

    pub async fn edit_comment(
        &self,
        ctx: &Ctx,
        comment_id: i64,
        body: Option<String>,
    ) -> Result<String, Failure> {
        if body
            .as_deref()
            .is_some_and(|body| body.trim().is_empty() || body.len() > MAX_COMMENT)
        {
            return Err(Failure::bad_request(
                "a comment is between 1 and 100000 characters",
            ));
        }
        let (org_id, user, owner) = (ctx.org_id, ctx.user_id, ctx.role == Role::Owner);
        let found = self
            .db
            .call(move |conn| {
                let row = conn.query_row(
                    "SELECT c.author, p.prefix || '-' || i.number FROM comments c JOIN issues i ON i.id = c.issue_id JOIN projects p ON p.id = i.project_id
                     WHERE c.id = ?1 AND i.org_id = ?2",
                    (comment_id, org_id),
                    |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, String>(1)?)),
                );
                let Some((author, key)) = rusqlite::OptionalExtension::optional(row)? else { return Ok(None) };
                if author != Some(user) && !owner {
                    return Ok(Some(Err(key)));
                }
                match body {
                    Some(body) => conn.execute("UPDATE comments SET body = ?2, updated_at = ?3 WHERE id = ?1", (comment_id, body.trim(), shell::now()))?,
                    None => conn.execute("DELETE FROM comments WHERE id = ?1", [comment_id])?,
                };
                Ok(Some(Ok(key)))
            })
            .await?;
        match found {
            None => Err(Failure::not_found()),
            Some(Err(_)) => Err(Failure::forbidden()),
            Some(Ok(key)) => {
                self.shell.publish(org_id, "issues");
                Ok(key)
            }
        }
    }

    pub async fn set_subscribed(&self, ctx: &Ctx, issue: &Issue, on: bool) -> Result<(), Failure> {
        let (issue_id, user) = (issue.id, ctx.user_id);
        self.db
            .call(move |conn| {
                if on {
                    store::subscribe(conn, issue_id, user)
                } else {
                    conn.execute(
                        "DELETE FROM subscriptions WHERE issue_id = ?1 AND user_id = ?2",
                        (issue_id, user),
                    )
                    .map(drop)
                }
            })
            .await
    }

    pub async fn mark(&self, ctx: &Ctx, id: Option<i64>, read: bool) -> Result<(), Failure> {
        let (org_id, user) = (ctx.org_id, ctx.user_id);
        self.db
            .call(move |conn| {
                conn.execute(
                    "UPDATE notifications SET read_at = CASE WHEN ?4 THEN coalesce(read_at, ?5) END WHERE org_id = ?1 AND user_id = ?2 AND (?3 IS NULL OR id = ?3)",
                    (org_id, user, id, read, shell::now()),
                )
            })
            .await?;
        self.shell.publish(org_id, "inbox");
        Ok(())
    }

    pub async fn pulse(&self, ctx: &Ctx) -> Result<String, Failure> {
        let (org_id, user) = (ctx.org_id, ctx.user_id);
        let unread: i64 = self
            .db
            .call(move |conn| conn.query_row("SELECT count(*) FROM notifications WHERE org_id = ?1 AND user_id = ?2 AND read_at IS NULL", (org_id, user), |row| row.get(0)))
            .await?;
        Ok(if unread == 0 {
            String::new()
        } else {
            maud::html! { a href="inbox" { "[@]" b { (unread) " unread" } } }.into_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_need_a_word_start_and_a_known_handle() {
        let who = People(vec![
            Person {
                id: 1,
                email: "ana@x.y".to_owned(),
                agent: false,
            },
            Person {
                id: 2,
                email: "claude@agents".to_owned(),
                agent: true,
            },
        ]);
        let found: Vec<i64> = who
            .mentioned("hey @ana, (@claude) mail@ana.com @bob @ana.")
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(found, [1, 2]);
        let twins = People(vec![
            Person {
                id: 1,
                email: "ana@x.y".to_owned(),
                agent: false,
            },
            Person {
                id: 3,
                email: "ana@z.w".to_owned(),
                agent: false,
            },
        ]);
        assert!(twins.find("ana").is_none());
        assert_eq!(twins.find("ana@z.w").map(|p| p.id), Some(3));
        assert_eq!(split_identifier(" chi-7 "), Some(("CHI".to_owned(), 7)));
        assert_eq!(split_identifier("CHI-7x"), None);
    }

    #[test]
    fn changes_take_numbers_or_text_and_labels_as_a_list_or_commas() {
        let json: Changes =
            serde_json::from_value(json!({ "priority": 1, "estimate": "3", "labels": ["a", "b"] }))
                .unwrap();
        assert_eq!(
            (
                json.priority.as_deref(),
                json.estimate.as_deref(),
                json.labels.map(|l| l.len())
            ),
            (Some("1"), Some("3"), Some(2))
        );
        let form: Changes = serde_urlencoded_like("priority=high&labels=a%2Cb&sort=1.5");
        assert_eq!(
            (
                form.priority.as_deref(),
                form.labels.map(|l| l.len()),
                form.sort
            ),
            (Some("high"), Some(2), Some(1.5))
        );
    }

    fn serde_urlencoded_like(query: &str) -> Changes {
        axum::extract::Query::<Changes>::try_from_uri(&format!("/?{query}").parse().unwrap())
            .unwrap()
            .0
    }
}
