use rusqlite::{Connection, OptionalExtension, Result, ToSql, params_from_iter, types::Value};
use shell::now;

use crate::{Changes, OPEN, PRIORITIES, People, STATUSES, query, split_identifier};

pub const ISSUE_FROM: &str = "FROM issues i JOIN projects p ON p.id = i.project_id LEFT JOIN milestones m ON m.id = i.milestone_id
     LEFT JOIN issues pi ON pi.id = i.parent_id LEFT JOIN projects pp ON pp.id = pi.project_id";
const ISSUE_COLUMNS: &str = "i.id, i.project_id, p.prefix, i.number, i.title, i.description, i.status, i.priority, i.estimate, i.assignee,
     i.creator, i.parent_id, i.milestone_id, i.due_date, i.error_ref, i.sort, i.created_at, i.updated_at, i.completed_at, p.name,
     coalesce(m.name, ''), coalesce(pp.prefix || '-' || pi.number, ''),
     coalesce((SELECT group_concat(l.name, ',') FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id), '')";

#[derive(Clone)]
pub struct Issue {
    pub id: i64,
    pub project_id: i64,
    pub prefix: String,
    pub number: i64,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: i64,
    pub estimate: Option<i64>,
    pub assignee: Option<i64>,
    pub creator: Option<i64>,
    pub parent_id: Option<i64>,
    pub milestone_id: Option<i64>,
    pub due_date: String,
    pub error_ref: Option<i64>,
    pub sort: f64,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub project: String,
    pub milestone: String,
    pub parent: String,
    pub labels: Vec<String>,
}

impl Issue {
    pub fn identifier(&self) -> String {
        format!("{}-{}", self.prefix, self.number)
    }

    pub fn priority_name(&self) -> &'static str {
        PRIORITIES
            .get(self.priority as usize)
            .copied()
            .unwrap_or("none")
    }
}

fn issue_row(row: &rusqlite::Row) -> Result<Issue> {
    let (
        id,
        project_id,
        prefix,
        number,
        title,
        description,
        status,
        priority,
        estimate,
        assignee,
        creator,
        parent_id,
        milestone_id,
        due_date,
        error_ref,
        sort,
    ) = <(
        i64,
        i64,
        String,
        i64,
        String,
        String,
        String,
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        Option<i64>,
        f64,
    )>::try_from(row)?;
    let labels: String = row.get(22)?;
    let mut labels: Vec<String> = labels
        .split(',')
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    labels.sort_by_key(|label| label.to_lowercase());
    Ok(Issue {
        id,
        project_id,
        prefix,
        number,
        title,
        description,
        status,
        priority,
        estimate,
        assignee,
        creator,
        parent_id,
        milestone_id,
        due_date,
        error_ref,
        sort,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
        completed_at: row.get(18)?,
        project: row.get(19)?,
        milestone: row.get(20)?,
        parent: row.get(21)?,
        labels,
    })
}

pub fn issues(
    conn: &Connection,
    org_id: i64,
    condition: &str,
    params: Vec<Value>,
    order: &str,
    limit: i64,
) -> Result<Vec<Issue>> {
    let sql = format!(
        "SELECT {ISSUE_COLUMNS} {ISSUE_FROM} WHERE i.org_id = ? AND {condition} ORDER BY {order} LIMIT ?"
    );
    let all = std::iter::once(Value::from(org_id))
        .chain(params)
        .chain([Value::from(limit)]);
    conn.prepare(&sql)?
        .query_map(params_from_iter(all), issue_row)?
        .collect()
}

pub fn issue_by_id(conn: &Connection, org_id: i64, id: i64) -> Result<Option<Issue>> {
    Ok(issues(conn, org_id, "i.id = ?", vec![Value::from(id)], "i.id", 1)?.pop())
}

pub fn issue_by_key(conn: &Connection, org_id: i64, key: &str) -> Result<Option<Issue>> {
    let Some((prefix, number)) = split_identifier(key) else {
        return Ok(None);
    };
    Ok(issues(
        conn,
        org_id,
        "p.prefix = ? AND i.number = ?",
        vec![Value::from(prefix), Value::from(number)],
        "i.id",
        1,
    )?
    .pop())
}

pub struct Change {
    pub field: &'static str,
    pub old: String,
    pub new: String,
}

pub struct Saved {
    pub issue: Issue,
    pub created: bool,
    pub changes: Vec<Change>,
    pub assigned: Option<i64>,
}

pub enum Problem {
    Db(rusqlite::Error),
    Invalid(String),
}

impl From<rusqlite::Error> for Problem {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error)
    }
}

fn invalid(message: impl Into<String>) -> Problem {
    Problem::Invalid(message.into())
}

pub fn atomic<T>(
    conn: &mut Connection,
    write: impl FnOnce(&Connection) -> Result<T, Problem>,
) -> Result<Result<T, String>> {
    let tx = conn.transaction()?;
    match write(&tx) {
        Ok(value) => {
            tx.commit()?;
            Ok(Ok(value))
        }
        Err(Problem::Invalid(message)) => Ok(Err(message)),
        Err(Problem::Db(error)) => Err(error),
    }
}

fn next_number(conn: &Connection, project_id: i64) -> Result<i64> {
    conn.query_row(
        "UPDATE projects SET next_number = next_number + 1 WHERE id = ?1 RETURNING next_number - 1",
        [project_id],
        |row| row.get(0),
    )
}

pub fn project_by_key(conn: &Connection, org_id: i64, key: &str) -> Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT id, prefix FROM projects WHERE org_id = ?1 AND (prefix = ?2 COLLATE NOCASE OR name = ?2 COLLATE NOCASE) ORDER BY prefix = ?2 COLLATE NOCASE DESC",
        (org_id, key.trim()),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

pub fn valid_date(date: &str) -> bool {
    let parts: Vec<&str> = date.split('-').collect();
    matches!(parts.as_slice(), [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2
        && y.chars().chain(m.chars()).chain(d.chars()).all(|c| c.is_ascii_digit())
        && (1..=12).contains(&m.parse::<u32>().unwrap_or(0)) && (1..=31).contains(&d.parse::<u32>().unwrap_or(0)))
}

struct Log<'a> {
    conn: &'a Connection,
    id: i64,
    changes: Vec<Change>,
}

impl Log<'_> {
    fn set(
        &mut self,
        column: &str,
        value: impl ToSql,
        field: &'static str,
        old: String,
        new: String,
    ) -> Result<()> {
        if old != new {
            self.conn.execute(
                &format!("UPDATE issues SET {column} = ?2 WHERE id = ?1"),
                (self.id, value),
            )?;
            self.changes.push(Change { field, old, new });
        }
        Ok(())
    }
}

pub fn clean_labels(names: &[String]) -> Result<Vec<String>, Problem> {
    let mut clean: Vec<String> = Vec::new();
    for name in names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
    {
        if name.contains(',') || name.chars().count() > 50 {
            return Err(invalid("a label is up to 50 characters without commas"));
        }
        if !clean.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
            clean.push(name.to_owned());
        }
    }
    clean.sort_by_key(|label| label.to_lowercase());
    Ok(clean)
}

fn create(
    conn: &Connection,
    org_id: i64,
    actor: Option<i64>,
    changes: &Changes,
    error_ref: Option<i64>,
) -> Result<i64, Problem> {
    let title = changes.title.as_deref().unwrap_or("").trim();
    if title.is_empty() {
        return Err(invalid("a title is required"));
    }
    let project = match changes
        .project
        .as_deref()
        .filter(|key| !key.trim().is_empty())
    {
        Some(key) => {
            project_by_key(conn, org_id, key)?
                .ok_or_else(|| invalid(format!("no project {key}")))?
                .0
        }
        None => conn
            .query_row(
                "SELECT id FROM projects WHERE org_id = ?1 ORDER BY id LIMIT 1",
                [org_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| invalid("create a project first"))?,
    };
    let number = next_number(conn, project)?;
    conn.execute(
        "INSERT INTO issues (org_id, project_id, number, title, creator, error_ref, sort, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, (SELECT coalesce(min(sort), 0) - 1 FROM issues WHERE org_id = ?1), ?7, ?7)",
        (org_id, project, number, title, actor, error_ref, now()),
    )?;
    Ok(conn.last_insert_rowid())
}

impl Log<'_> {
    fn title(&mut self, old: &Issue, title: Option<&str>) -> Result<(), Problem> {
        let Some(title) = title.map(str::trim) else {
            return Ok(());
        };
        if title.is_empty() || title.chars().count() > 500 {
            return Err(invalid("a title is required, up to 500 characters"));
        }
        Ok(self.set("title", title, "title", old.title.clone(), title.to_owned())?)
    }

    fn description(&mut self, old: &Issue, description: Option<&String>) -> Result<(), Problem> {
        let Some(description) = description.filter(|text| **text != old.description) else {
            return Ok(());
        };
        if description.len() > 200_000 {
            return Err(invalid("a description is up to 200 KB"));
        }
        Ok(self.set(
            "description",
            description,
            "description",
            String::new(),
            "edited".to_owned(),
        )?)
    }

    fn status(&mut self, old: &Issue, status: Option<&str>) -> Result<(), Problem> {
        let Some(status) = status.map(str::trim) else {
            return Ok(());
        };
        if !STATUSES.contains(&status) {
            return Err(invalid(format!("status is one of {}", STATUSES.join(", "))));
        }
        if status != old.status {
            self.set(
                "status",
                status,
                "status",
                old.status.clone(),
                status.to_owned(),
            )?;
            self.conn.execute(
                "UPDATE issues SET completed_at = CASE WHEN ?2 THEN coalesce(completed_at, ?3) END WHERE id = ?1",
                (self.id, !OPEN.contains(&status), now()),
            )?;
        }
        Ok(())
    }

    fn priority(&mut self, old: &Issue, priority: Option<&str>) -> Result<(), Problem> {
        let Some(priority) = priority else {
            return Ok(());
        };
        let value = query::priority(priority)
            .ok_or_else(|| invalid(format!("priority is one of {}", PRIORITIES.join(", "))))?;
        Ok(self.set(
            "priority",
            value,
            "priority",
            old.priority_name().to_owned(),
            PRIORITIES[value as usize].to_owned(),
        )?)
    }

    fn estimate(&mut self, old: &Issue, estimate: Option<&str>) -> Result<(), Problem> {
        let Some(estimate) = estimate else {
            return Ok(());
        };
        let value = match estimate.trim() {
            "" => None,
            text => Some(
                text.parse::<i64>()
                    .ok()
                    .filter(|n| (0..=1000).contains(n))
                    .ok_or_else(|| invalid("an estimate is a number of points"))?,
            ),
        };
        let shown = |value: Option<i64>| value.map(|n| n.to_string()).unwrap_or_default();
        Ok(self.set(
            "estimate",
            value,
            "estimate",
            shown(old.estimate),
            shown(value),
        )?)
    }

    fn assignee(
        &mut self,
        old: &Issue,
        actor: Option<i64>,
        who: &People,
        assignee: Option<&str>,
    ) -> Result<Option<i64>, Problem> {
        let Some(assignee) = assignee else {
            return Ok(None);
        };
        let value = match assignee.trim() {
            "" | "none" => None,
            "me" => actor,
            handle => Some(
                who.find(handle)
                    .ok_or_else(|| invalid(format!("nobody called {handle} here")))?
                    .id,
            ),
        };
        self.set(
            "assignee",
            value,
            "assignee",
            who.name(old.assignee),
            who.name(value),
        )?;
        Ok(value.filter(|_| value != old.assignee))
    }

    fn project(&mut self, old: &Issue, org_id: i64, changes: &Changes) -> Result<i64, Problem> {
        let Some(key) = changes
            .project
            .as_deref()
            .filter(|key| !key.trim().is_empty())
        else {
            return Ok(old.project_id);
        };
        let (new_project, prefix) = project_by_key(self.conn, org_id, key)?
            .ok_or_else(|| invalid(format!("no project {key}")))?;
        if new_project == old.project_id {
            return Ok(old.project_id);
        }
        let number = next_number(self.conn, new_project)?;
        self.conn.execute(
            "UPDATE issues SET project_id = ?2, number = ?3, milestone_id = NULL WHERE id = ?1",
            (self.id, new_project, number),
        )?;
        self.changes.push(Change {
            field: "project",
            old: old.identifier(),
            new: format!("{prefix}-{number}"),
        });
        if changes.milestone.is_none() && !old.milestone.is_empty() {
            self.changes.push(Change {
                field: "milestone",
                old: old.milestone.clone(),
                new: String::new(),
            });
        }
        Ok(new_project)
    }

    fn milestone(
        &mut self,
        old: &Issue,
        project_id: i64,
        name: Option<&str>,
    ) -> Result<(), Problem> {
        let Some(name) = name else {
            return Ok(());
        };
        let value: Option<(i64, String)> = match name.trim() {
            "" | "none" => None,
            name => Some(
                self.conn.query_row("SELECT id, name FROM milestones WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE", (project_id, name), |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?
                .ok_or_else(|| invalid(format!("the project has no milestone {name}")))?,
            ),
        };
        let current = if project_id == old.project_id {
            old.milestone.clone()
        } else {
            String::new()
        };
        let (milestone_id, new) =
            value.map_or((None, String::new()), |(id, name)| (Some(id), name));
        self.conn.execute(
            "UPDATE issues SET milestone_id = ?2 WHERE id = ?1",
            (self.id, milestone_id),
        )?;
        if current != new {
            self.changes.push(Change {
                field: "milestone",
                old: current,
                new,
            });
        }
        Ok(())
    }

    fn parent(&mut self, old: &Issue, org_id: i64, parent: Option<&str>) -> Result<(), Problem> {
        let Some(parent) = parent else {
            return Ok(());
        };
        let value = match parent.trim() {
            "" | "none" => None,
            key => {
                let parent = issue_by_key(self.conn, org_id, key)?
                    .ok_or_else(|| invalid(format!("no issue {key}")))?;
                let loops: bool = self.conn.query_row(
                    "WITH RECURSIVE up(id) AS (SELECT ?1 UNION SELECT i.parent_id FROM issues i JOIN up ON i.id = up.id WHERE i.parent_id IS NOT NULL)
                     SELECT EXISTS (SELECT 1 FROM up WHERE id = ?2)",
                    (parent.id, self.id),
                    |row| row.get(0),
                )?;
                if loops {
                    return Err(invalid("an issue cannot be its own ancestor"));
                }
                Some((parent.id, parent.identifier()))
            }
        };
        let (parent_id, new) = value.map_or((None, String::new()), |(id, key)| (Some(id), key));
        Ok(self.set("parent_id", parent_id, "parent", old.parent.clone(), new)?)
    }

    fn due(&mut self, old: &Issue, due: Option<&str>) -> Result<(), Problem> {
        let Some(due) = due.map(str::trim) else {
            return Ok(());
        };
        if !due.is_empty() && !valid_date(due) {
            return Err(invalid("a due date is YYYY-MM-DD"));
        }
        Ok(self.set("due_date", due, "due", old.due_date.clone(), due.to_owned())?)
    }

    fn labels(
        &mut self,
        old: &Issue,
        org_id: i64,
        labels: Option<&[String]>,
    ) -> Result<(), Problem> {
        let Some(labels) = labels else {
            return Ok(());
        };
        let labels = clean_labels(labels)?;
        let (before, after) = (old.labels.join(", "), labels.join(", "));
        if before.to_lowercase() == after.to_lowercase() {
            return Ok(());
        }
        self.conn
            .execute("DELETE FROM issue_labels WHERE issue_id = ?1", [self.id])?;
        for label in &labels {
            self.conn.execute(
                "INSERT INTO labels (org_id, name) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                (org_id, label),
            )?;
            self.conn.execute("INSERT INTO issue_labels SELECT ?1, id FROM labels WHERE org_id = ?2 AND name = ?3", (self.id, org_id, label))?;
        }
        self.changes.push(Change {
            field: "labels",
            old: before,
            new: after,
        });
        Ok(())
    }

    fn apply(
        &mut self,
        old: &Issue,
        org_id: i64,
        actor: Option<i64>,
        who: &People,
        changes: &Changes,
        created: bool,
    ) -> Result<Option<i64>, Problem> {
        self.title(old, changes.title.as_deref())?;
        self.description(old, changes.description.as_ref())?;
        self.status(old, changes.status.as_deref())?;
        self.priority(old, changes.priority.as_deref())?;
        self.estimate(old, changes.estimate.as_deref())?;
        let assigned = self.assignee(old, actor, who, changes.assignee.as_deref())?;
        let project_id = if created {
            old.project_id
        } else {
            self.project(old, org_id, changes)?
        };
        self.milestone(old, project_id, changes.milestone.as_deref())?;
        self.parent(old, org_id, changes.parent.as_deref())?;
        self.due(old, changes.due_date.as_deref())?;
        self.labels(old, org_id, changes.labels.as_deref())?;
        Ok(assigned)
    }

    fn record(&self, actor: Option<i64>, created: bool) -> Result<()> {
        let at = now();
        if !self.changes.is_empty() {
            self.conn.execute(
                "UPDATE issues SET updated_at = ?2 WHERE id = ?1",
                (self.id, at),
            )?;
        }
        if created {
            return Ok(());
        }
        for change in &self.changes {
            self.conn.execute(
                "INSERT INTO history (issue_id, actor, field, old, new, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (self.id, actor, change.field, &change.old, &change.new, at),
            )?;
        }
        Ok(())
    }
}

pub fn save(
    conn: &Connection,
    org_id: i64,
    actor: Option<i64>,
    who: &People,
    id: Option<i64>,
    changes: Changes,
    error_ref: Option<i64>,
) -> Result<Saved, Problem> {
    let created = id.is_none();
    let id = match id {
        Some(id) => id,
        None => create(conn, org_id, actor, &changes, error_ref)?,
    };
    let old = issue_by_id(conn, org_id, id)?.ok_or_else(|| invalid("no such issue"))?;
    let mut log = Log {
        conn,
        id,
        changes: Vec::new(),
    };
    let assigned = log.apply(&old, org_id, actor, who, &changes, created)?;
    if let Some(sort) = changes.sort.filter(|sort| sort.is_finite()) {
        conn.execute("UPDATE issues SET sort = ?2 WHERE id = ?1", (id, sort))?;
    }
    log.record(actor, created)?;
    for user in [created.then_some(actor).flatten(), assigned]
        .into_iter()
        .flatten()
    {
        subscribe(conn, id, user)?;
    }
    let issue = issue_by_id(conn, org_id, id)?.ok_or_else(|| invalid("no such issue"))?;
    Ok(Saved {
        issue,
        created,
        changes: log.changes,
        assigned,
    })
}

pub fn subscribe(conn: &Connection, issue_id: i64, user_id: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO subscriptions (issue_id, user_id) VALUES (?1, ?2)",
        (issue_id, user_id),
    )
    .map(drop)
}

pub fn subscribers(conn: &Connection, issue_id: i64) -> Result<Vec<i64>> {
    conn.prepare("SELECT user_id FROM subscriptions WHERE issue_id = ?1")?
        .query_map([issue_id], |row| row.get(0))?
        .collect()
}

pub fn notify(
    conn: &Connection,
    org_id: i64,
    users: &[i64],
    issue_id: i64,
    actor: Option<i64>,
    text: &str,
) -> Result<()> {
    for user in users {
        conn.execute(
            "INSERT INTO notifications (org_id, user_id, issue_id, actor, text, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (org_id, user, issue_id, actor, text, now()),
        )?;
    }
    Ok(())
}

pub struct Comment {
    pub id: i64,
    pub author: Option<i64>,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub fn comments(conn: &Connection, issue_id: i64) -> Result<Vec<Comment>> {
    conn.prepare("SELECT id, author, body, created_at, updated_at FROM comments WHERE issue_id = ?1 ORDER BY id")?
        .query_map([issue_id], |row| {
            Ok(Comment { id: row.get(0)?, author: row.get(1)?, body: row.get(2)?, created_at: row.get(3)?, updated_at: row.get(4)? })
        })?
        .collect()
}

pub fn add_comment(
    conn: &Connection,
    issue_id: i64,
    author: Option<i64>,
    body: &str,
) -> Result<i64> {
    let at = now();
    conn.execute("INSERT INTO comments (issue_id, author, body, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)", (issue_id, author, body, at))?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE issues SET updated_at = ?2 WHERE id = ?1",
        (issue_id, at),
    )?;
    Ok(id)
}

pub struct Event {
    pub actor: Option<i64>,
    pub field: String,
    pub old: String,
    pub new: String,
    pub at: i64,
}

pub fn history(conn: &Connection, issue_id: i64) -> Result<Vec<Event>> {
    conn.prepare("SELECT actor, field, old, new, at FROM history WHERE issue_id = ?1 ORDER BY id")?
        .query_map([issue_id], |row| {
            Ok(Event {
                actor: row.get(0)?,
                field: row.get(1)?,
                old: row.get(2)?,
                new: row.get(3)?,
                at: row.get(4)?,
            })
        })?
        .collect()
}

pub struct Relation {
    pub label: &'static str,
    pub other: String,
    pub title: String,
    pub status: String,
}

pub const RELATIONS: [&str; 3] = ["blocks", "related", "duplicate"];

pub fn relations(conn: &Connection, issue_id: i64) -> Result<Vec<Relation>> {
    conn.prepare(
        "SELECT r.id, r.kind, 1, p.prefix || '-' || o.number, o.title, o.status FROM relations r
           JOIN issues o ON o.id = r.other_id JOIN projects p ON p.id = o.project_id WHERE r.issue_id = ?1
         UNION ALL
         SELECT r.id, r.kind, 0, p.prefix || '-' || o.number, o.title, o.status FROM relations r
           JOIN issues o ON o.id = r.issue_id JOIN projects p ON p.id = o.project_id WHERE r.other_id = ?1",
    )?
    .query_map([issue_id], |row| {
        let (kind, outgoing): (String, bool) = (row.get(1)?, row.get(2)?);
        let label = match (kind.as_str(), outgoing) {
            ("blocks", true) => "blocks",
            ("blocks", false) => "blocked by",
            ("duplicate", true) => "duplicate of",
            ("duplicate", false) => "duplicated by",
            _ => "related to",
        };
        Ok(Relation { label, other: row.get(3)?, title: row.get(4)?, status: row.get(5)? })
    })?
    .collect()
}

pub struct Attachment {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub file: String,
    pub mime: String,
    pub size: i64,
    pub uploader: Option<i64>,
    pub created_at: i64,
}

pub fn attachments(conn: &Connection, issue_id: i64) -> Result<Vec<Attachment>> {
    conn.prepare("SELECT id, title, url, file, mime, size, uploader, created_at FROM attachments WHERE issue_id = ?1 ORDER BY id")?
        .query_map([issue_id], |row| {
            Ok(Attachment {
                id: row.get(0)?,
                title: row.get(1)?,
                url: row.get(2)?,
                file: row.get(3)?,
                mime: row.get(4)?,
                size: row.get(5)?,
                uploader: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?
        .collect()
}

pub struct Link {
    pub kind: String,
    pub url: String,
    pub title: String,
    pub state: String,
    pub at: i64,
}

pub fn links(conn: &Connection, issue_id: i64) -> Result<Vec<Link>> {
    conn.prepare(
        "SELECT kind, url, title, state, at FROM links WHERE issue_id = ?1 ORDER BY at DESC",
    )?
    .query_map([issue_id], |row| {
        Ok(Link {
            kind: row.get(0)?,
            url: row.get(1)?,
            title: row.get(2)?,
            state: row.get(3)?,
            at: row.get(4)?,
        })
    })?
    .collect()
}

pub const PROJECT_STATUSES: [&str; 5] = ["planned", "active", "paused", "completed", "cancelled"];

pub struct Project {
    pub id: i64,
    pub prefix: String,
    pub name: String,
    pub description: String,
    pub lead: Option<i64>,
    pub status: String,
    pub target_date: String,
    pub repo_id: Option<i64>,
    pub total: i64,
    pub done: i64,
}

const COUNTS: &str = "(SELECT count(*) FROM issues x WHERE x.{0} = {1}.id AND x.status NOT IN ('cancelled', 'duplicate')),
     (SELECT count(*) FROM issues x WHERE x.{0} = {1}.id AND x.status = 'done')";

fn counts(column: &str, table: &str) -> String {
    COUNTS.replace("{0}", column).replace("{1}", table)
}

pub fn projects(conn: &Connection, org_id: i64) -> Result<Vec<Project>> {
    conn.prepare(&format!(
        "SELECT p.id, p.prefix, p.name, p.description, p.lead, p.status, p.target_date, p.repo_id, {} FROM projects p
         WHERE p.org_id = ?1 ORDER BY p.status IN ('completed', 'cancelled'), p.name",
        counts("project_id", "p")
    ))?
    .query_map([org_id], |row| {
        Ok(Project {
            id: row.get(0)?,
            prefix: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            lead: row.get(4)?,
            status: row.get(5)?,
            target_date: row.get(6)?,
            repo_id: row.get(7)?,
            total: row.get(8)?,
            done: row.get(9)?,
        })
    })?
    .collect()
}

pub fn valid_prefix(prefix: &str) -> bool {
    prefix.starts_with(|c: char| c.is_ascii_uppercase())
        && shell::identifiers(&format!("{prefix}-1")) == [(0, prefix.len() + 2)]
}

pub struct ProjectFields {
    pub prefix: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub lead: Option<Option<i64>>,
    pub status: Option<String>,
    pub target_date: Option<String>,
    pub repo_id: Option<Option<i64>>,
}

fn create_project(conn: &Connection, org_id: i64, fields: &ProjectFields) -> Result<i64, Problem> {
    let name = fields.name.as_deref().unwrap_or("").trim();
    let prefix = fields.prefix.as_deref().unwrap_or("").trim().to_uppercase();
    if name.is_empty() || !valid_prefix(&prefix) {
        return Err(invalid(
            "a project needs a name and a prefix of up to 10 capital letters or digits, starting with a letter",
        ));
    }
    if conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE org_id = ?1 AND prefix = ?2)",
        (org_id, &prefix),
        |row| row.get(0),
    )? {
        return Err(invalid(format!("the prefix {prefix} is taken")));
    }
    conn.execute(
        "INSERT INTO projects (org_id, prefix, name, created_at) VALUES (?1, ?2, ?3, ?4)",
        (org_id, &prefix, name, now()),
    )?;
    Ok(conn.last_insert_rowid())
}

fn set_prefix(conn: &Connection, org_id: i64, id: i64, prefix: &str) -> Result<(), Problem> {
    let prefix = prefix.trim().to_uppercase();
    if !valid_prefix(&prefix) {
        return Err(invalid(
            "a prefix is up to 10 capital letters or digits, starting with a letter",
        ));
    }
    let taken: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE org_id = ?1 AND prefix = ?2 AND id != ?3)",
        (org_id, &prefix, id),
        |row| row.get(0),
    )?;
    if taken {
        return Err(invalid(format!("the prefix {prefix} is taken")));
    }
    conn.execute(
        "UPDATE projects SET prefix = ?2 WHERE id = ?1",
        (id, prefix),
    )?;
    Ok(())
}

fn update_project_text(conn: &Connection, id: i64, fields: &ProjectFields) -> Result<(), Problem> {
    if let Some(name) = fields.name.as_deref().map(str::trim) {
        if name.is_empty() {
            return Err(invalid("a project needs a name"));
        }
        conn.execute("UPDATE projects SET name = ?2 WHERE id = ?1", (id, name))?;
    }
    if let Some(description) = &fields.description {
        conn.execute(
            "UPDATE projects SET description = ?2 WHERE id = ?1",
            (id, description),
        )?;
    }
    if let Some(lead) = fields.lead {
        conn.execute("UPDATE projects SET lead = ?2 WHERE id = ?1", (id, lead))?;
    }
    Ok(())
}

fn update_project_plan(conn: &Connection, id: i64, fields: &ProjectFields) -> Result<(), Problem> {
    if let Some(status) = &fields.status {
        if !PROJECT_STATUSES.contains(&status.as_str()) {
            return Err(invalid(format!(
                "a project status is one of {}",
                PROJECT_STATUSES.join(", ")
            )));
        }
        conn.execute(
            "UPDATE projects SET status = ?2 WHERE id = ?1",
            (id, status),
        )?;
    }
    if let Some(date) = fields.target_date.as_deref().map(str::trim) {
        if !date.is_empty() && !valid_date(date) {
            return Err(invalid("a target date is YYYY-MM-DD"));
        }
        conn.execute(
            "UPDATE projects SET target_date = ?2 WHERE id = ?1",
            (id, date),
        )?;
    }
    if let Some(repo_id) = fields.repo_id {
        conn.execute(
            "UPDATE projects SET repo_id = ?2 WHERE id = ?1",
            (id, repo_id),
        )?;
    }
    Ok(())
}

pub fn save_project(
    conn: &Connection,
    org_id: i64,
    key: Option<&str>,
    fields: ProjectFields,
) -> Result<String, Problem> {
    let id = match key {
        Some(key) => {
            project_by_key(conn, org_id, key)?
                .ok_or_else(|| invalid(format!("no project {key}")))?
                .0
        }
        None => create_project(conn, org_id, &fields)?,
    };
    if let Some(prefix) = fields.prefix.as_deref().filter(|_| key.is_some()) {
        set_prefix(conn, org_id, id, prefix)?;
    }
    update_project_text(conn, id, &fields)?;
    update_project_plan(conn, id, &fields)?;
    Ok(
        conn.query_row("SELECT prefix FROM projects WHERE id = ?1", [id], |row| {
            row.get(0)
        })?,
    )
}

pub struct Milestone {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub target_date: String,
    pub total: i64,
    pub done: i64,
}

pub fn milestones(conn: &Connection, project_id: i64) -> Result<Vec<Milestone>> {
    conn.prepare(&format!(
        "SELECT m.id, m.name, m.description, m.target_date, {} FROM milestones m WHERE m.project_id = ?1 ORDER BY m.target_date = '', m.target_date, m.id",
        counts("milestone_id", "m")
    ))?
    .query_map([project_id], |row| {
        Ok(Milestone { id: row.get(0)?, name: row.get(1)?, description: row.get(2)?, target_date: row.get(3)?, total: row.get(4)?, done: row.get(5)? })
    })?
    .collect()
}

pub fn save_milestone(
    conn: &Connection,
    project_id: i64,
    id: Option<i64>,
    name: &str,
    description: &str,
    target_date: &str,
) -> Result<i64, Problem> {
    let (name, target_date) = (name.trim(), target_date.trim());
    if name.is_empty() || (!target_date.is_empty() && !valid_date(target_date)) {
        return Err(invalid(
            "a milestone needs a name, and a target date is YYYY-MM-DD",
        ));
    }
    let clash: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM milestones WHERE project_id = ?1 AND name = ?2 COLLATE NOCASE AND id IS NOT ?3)",
        (project_id, name, id),
        |row| row.get(0),
    )?;
    if clash {
        return Err(invalid(format!(
            "the project already has a milestone {name}"
        )));
    }
    match id {
        Some(id) => {
            let changed = conn.execute(
                "UPDATE milestones SET name = ?3, description = ?4, target_date = ?5 WHERE id = ?1 AND project_id = ?2",
                (id, project_id, name, description, target_date),
            )?;
            if changed == 0 {
                return Err(invalid("no such milestone"));
            }
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO milestones (project_id, name, description, target_date) VALUES (?1, ?2, ?3, ?4)",
                (project_id, name, description, target_date),
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

pub struct Document {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub body: String,
    pub author: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

const DOCUMENT_COLUMNS: &str = "d.id, coalesce(p.prefix, ''), d.title, d.body, d.author, d.created_at, d.updated_at FROM documents d LEFT JOIN projects p ON p.id = d.project_id";

fn document_row(row: &rusqlite::Row) -> Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        project: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        author: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

pub fn documents(
    conn: &Connection,
    org_id: i64,
    project: &str,
    text: &str,
    limit: i64,
) -> Result<Vec<Document>> {
    let terms = query::search_terms(text);
    conn.prepare(&format!(
        "SELECT {DOCUMENT_COLUMNS} WHERE d.org_id = ?1 AND (?2 = '' OR p.prefix = ?2 COLLATE NOCASE)
           AND (?3 = '' OR d.id IN (SELECT rowid FROM documents_fts WHERE documents_fts MATCH ?3))
         ORDER BY d.updated_at DESC LIMIT ?4"
    ))?
    .query_map((org_id, project, terms, limit), document_row)?
    .collect()
}

pub fn document(conn: &Connection, org_id: i64, id: i64) -> Result<Option<Document>> {
    conn.query_row(
        &format!("SELECT {DOCUMENT_COLUMNS} WHERE d.org_id = ?1 AND d.id = ?2"),
        (org_id, id),
        document_row,
    )
    .optional()
}

pub fn save_document(
    conn: &Connection,
    org_id: i64,
    author: i64,
    id: Option<i64>,
    title: &str,
    body: &str,
    project: Option<&str>,
) -> Result<i64, Problem> {
    let title = title.trim();
    if title.is_empty() || body.len() > 1_000_000 {
        return Err(invalid("a document needs a title and is up to 1 MB"));
    }
    let project_id = match project.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => Some(
            project_by_key(conn, org_id, key)?
                .ok_or_else(|| invalid(format!("no project {key}")))?
                .0,
        ),
        None => None,
    };
    let at = now();
    match id {
        Some(id) => {
            let changed = conn.execute(
                "UPDATE documents SET title = ?3, body = ?4, project_id = ?5, updated_at = ?6 WHERE id = ?1 AND org_id = ?2",
                (id, org_id, title, body, project_id, at),
            )?;
            if changed == 0 {
                return Err(invalid("no such document"));
            }
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO documents (org_id, project_id, title, body, author, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                (org_id, project_id, title, body, author, at),
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

pub struct Label {
    pub name: String,
    pub color: String,
    pub open: i64,
}

pub fn labels(conn: &Connection, org_id: i64) -> Result<Vec<Label>> {
    conn.prepare(&format!(
        "SELECT l.id, l.name, l.color, (SELECT count(*) FROM issue_labels il JOIN issues i ON i.id = il.issue_id
           WHERE il.label_id = l.id AND i.status IN ('{}'))
         FROM labels l WHERE l.org_id = ?1 ORDER BY l.name",
        OPEN.join("', '")
    ))?
    .query_map([org_id], |row| Ok(Label { name: row.get(1)?, color: row.get(2)?, open: row.get(3)? }))?
    .collect()
}

pub fn valid_color(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn save_label(
    conn: &Connection,
    org_id: i64,
    name: &str,
    new_name: Option<&str>,
    color: Option<&str>,
) -> Result<(), Problem> {
    let name = clean_labels(&[name.to_owned()])?
        .pop()
        .ok_or_else(|| invalid("a label needs a name"))?;
    if color.is_some_and(|color| !valid_color(color)) {
        return Err(invalid("a color is #rrggbb"));
    }
    conn.execute(
        "INSERT INTO labels (org_id, name) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
        (org_id, &name),
    )?;
    if let Some(color) = color {
        conn.execute(
            "UPDATE labels SET color = ?3 WHERE org_id = ?1 AND name = ?2",
            (org_id, &name, color),
        )?;
    }
    if let Some(new_name) = new_name.filter(|new_name| new_name.trim() != name) {
        let new_name = clean_labels(&[new_name.to_owned()])?
            .pop()
            .ok_or_else(|| invalid("a label needs a name"))?;
        let taken: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM labels WHERE org_id = ?1 AND name = ?2)",
            (org_id, &new_name),
            |row| row.get(0),
        )?;
        if taken && !new_name.eq_ignore_ascii_case(&name) {
            return Err(invalid(format!("the label {new_name} exists")));
        }
        conn.execute(
            "UPDATE labels SET name = ?3 WHERE org_id = ?1 AND name = ?2",
            (org_id, &name, new_name),
        )?;
    }
    Ok(())
}

pub struct Template {
    pub id: i64,
    pub name: String,
    pub title: String,
    pub description: String,
    pub fields: serde_json::Value,
}

pub fn templates(conn: &Connection, org_id: i64) -> Result<Vec<Template>> {
    conn.prepare("SELECT id, name, title, description, fields FROM templates WHERE org_id = ?1 ORDER BY name")?
        .query_map([org_id], |row| {
            let fields: String = row.get(4)?;
            Ok(Template {
                id: row.get(0)?,
                name: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                fields: serde_json::from_str(&fields).unwrap_or_default(),
            })
        })?
        .collect()
}

pub struct View {
    pub id: i64,
    pub owner: i64,
    pub name: String,
    pub query: String,
    pub layout: String,
    pub shared: bool,
}

pub fn views(conn: &Connection, org_id: i64, user_id: i64) -> Result<Vec<View>> {
    conn.prepare("SELECT id, owner, name, query, layout, shared FROM views WHERE org_id = ?1 AND (shared OR owner = ?2) ORDER BY name")?
        .query_map((org_id, user_id), |row| {
            Ok(View { id: row.get(0)?, owner: row.get(1)?, name: row.get(2)?, query: row.get(3)?, layout: row.get(4)?, shared: row.get(5)? })
        })?
        .collect()
}

pub struct Notification {
    pub id: i64,
    pub issue: String,
    pub title: String,
    pub actor: Option<i64>,
    pub text: String,
    pub created_at: i64,
    pub read: bool,
}

pub fn notifications(
    conn: &Connection,
    org_id: i64,
    user_id: i64,
    unread: bool,
    limit: i64,
) -> Result<Vec<Notification>> {
    conn.prepare(
        "SELECT n.id, coalesce(p.prefix || '-' || i.number, ''), coalesce(i.title, ''), n.actor, n.text, n.created_at, n.read_at IS NOT NULL
         FROM notifications n LEFT JOIN issues i ON i.id = n.issue_id LEFT JOIN projects p ON p.id = i.project_id
         WHERE n.org_id = ?1 AND n.user_id = ?2 AND (NOT ?3 OR n.read_at IS NULL) ORDER BY n.id DESC LIMIT ?4",
    )?
    .query_map((org_id, user_id, unread, limit), |row| {
        Ok(Notification {
            id: row.get(0)?,
            issue: row.get(1)?,
            title: row.get(2)?,
            actor: row.get(3)?,
            text: row.get(4)?,
            created_at: row.get(5)?,
            read: row.get(6)?,
        })
    })?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell::Person;

    fn changes(json: serde_json::Value) -> Changes {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn issues_number_per_project_and_log_what_changed() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        let who = People(vec![Person {
            id: 5,
            email: "ana@x.y".to_owned(),
            agent: false,
        }]);
        let save_ok = |conn: &mut Connection, id: Option<i64>, json: serde_json::Value| {
            atomic(conn, |tx| {
                save(tx, 1, Some(9), &who, id, changes(json), None)
            })
            .unwrap()
        };
        assert_eq!(
            save_ok(&mut conn, None, serde_json::json!({ "title": "x" }))
                .err()
                .unwrap(),
            "create a project first"
        );
        let fields = |prefix: &str| ProjectFields {
            prefix: Some(prefix.to_owned()),
            name: Some(prefix.to_owned()),
            description: None,
            lead: None,
            status: None,
            target_date: None,
            repo_id: None,
        };
        atomic(&mut conn, |tx| save_project(tx, 1, None, fields("chi")))
            .unwrap()
            .unwrap();
        atomic(&mut conn, |tx| save_project(tx, 1, None, fields("OPS")))
            .unwrap()
            .unwrap();
        assert!(
            atomic(&mut conn, |tx| save_project(tx, 1, None, fields("1X")))
                .unwrap()
                .is_err()
        );

        let first = save_ok(
            &mut conn,
            None,
            serde_json::json!({ "title": "first", "priority": 2, "labels": ["Bug", "bug", "ui"] }),
        )
        .unwrap();
        assert_eq!(
            (first.issue.identifier(), first.issue.labels.clone()),
            ("CHI-1".to_owned(), vec!["Bug".to_owned(), "ui".to_owned()])
        );
        let second = save_ok(
            &mut conn,
            None,
            serde_json::json!({ "title": "second", "parent": "chi-1" }),
        )
        .unwrap()
        .issue;
        assert_eq!(
            (second.identifier(), second.parent.as_str()),
            ("CHI-2".to_owned(), "CHI-1")
        );

        assert!(
            save_ok(
                &mut conn,
                Some(first.issue.id),
                serde_json::json!({ "parent": "CHI-2" })
            )
            .is_err(),
            "a loop is refused"
        );
        assert!(
            save_ok(
                &mut conn,
                Some(first.issue.id),
                serde_json::json!({ "status": "done", "assignee": "nobody" })
            )
            .is_err()
        );
        let done = save_ok(
            &mut conn,
            Some(first.issue.id),
            serde_json::json!({ "status": "done", "assignee": "ana", "labels": "ui" }),
        )
        .unwrap();
        assert_eq!(done.assigned, Some(5));
        assert!(done.issue.completed_at.is_some());
        let fields: Vec<_> = history(&conn, first.issue.id)
            .unwrap()
            .into_iter()
            .map(|e| (e.field, e.old, e.new))
            .collect();
        assert_eq!(
            fields,
            [
                ("status".to_owned(), "backlog".to_owned(), "done".to_owned()),
                ("assignee".to_owned(), String::new(), "ana".to_owned()),
                ("labels".to_owned(), "Bug, ui".to_owned(), "ui".to_owned()),
            ]
        );

        let moved = save_ok(
            &mut conn,
            Some(second.id),
            serde_json::json!({ "project": "ops" }),
        )
        .unwrap()
        .issue;
        assert_eq!(moved.identifier(), "OPS-1");
        let compiled = query::compile("label:UI is:closed", 9, &who).unwrap();
        let found = issues(&conn, 1, &compiled.condition, compiled.params, "i.id", 10).unwrap();
        assert_eq!(
            found.iter().map(Issue::identifier).collect::<Vec<_>>(),
            ["CHI-1"]
        );
        let compiled = query::compile("seco", 9, &who).unwrap();
        assert_eq!(
            issues(&conn, 1, &compiled.condition, compiled.params, "i.id", 10)
                .unwrap()
                .len(),
            1
        );
    }
}
