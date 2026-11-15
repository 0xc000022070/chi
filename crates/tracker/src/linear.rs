use std::{collections::HashMap, path::Path};

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::{Value, json};

const ENDPOINT: &str = "https://api.linear.app/graphql";

const TEAMS: &str =
    "query($after: String) { teams(first: 50, after: $after) { nodes { id key name description }
  pageInfo { hasNextPage endCursor } } }";
const ISSUES: &str =
    "query($after: String) { issues(first: 100, after: $after, includeArchived: true) { nodes {
  id number title description priority estimate dueDate createdAt updatedAt completedAt canceledAt
  team { id } state { name type } assignee { email } creator { email } parent { id }
  project { name } projectMilestone { name } labels { nodes { name color } } }
  pageInfo { hasNextPage endCursor } } }";
const RELATIONS: &str = "query($after: String) { issueRelations(first: 100, after: $after) { nodes { type issue { id } relatedIssue { id } }
  pageInfo { hasNextPage endCursor } } }";
const COMMENTS: &str = "query($after: String) { comments(first: 100, after: $after) { nodes { id body createdAt updatedAt user { email } issue { id } }
  pageInfo { hasNextPage endCursor } } }";
const DOCUMENTS: &str = "query($after: String) { documents(first: 50, after: $after) { nodes { id title content createdAt updatedAt creator { email } }
  pageInfo { hasNextPage endCursor } } }";

fn all(key: &str, query: &str, field: &str) -> Result<Vec<Value>, String> {
    let (mut nodes, mut after) = (Vec::new(), Value::Null);
    loop {
        let reply: Value = ureq::post(ENDPOINT)
            .set("Authorization", key)
            .send_json(json!({ "query": query, "variables": { "after": after } }))
            .map_err(|e| match e {
                ureq::Error::Status(code, response) => format!(
                    "linear answered {code}: {}",
                    response.into_string().unwrap_or_default()
                ),
                other => other.to_string(),
            })?
            .into_json()
            .map_err(|e| e.to_string())?;
        if let Some(errors) = reply.get("errors") {
            return Err(format!("linear: {errors}"));
        }
        let page = &reply["data"][field];
        nodes.extend(page["nodes"].as_array().cloned().unwrap_or_default());
        if page["pageInfo"]["hasNextPage"] != true {
            return Ok(nodes);
        }
        after = page["pageInfo"]["endCursor"].clone();
        eprint!("\r{field}: {}", nodes.len());
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn epoch(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    if text.len() < 19 || !text.ends_with('Z') {
        return None;
    }
    let days = days_from_civil(number(0..4)?, number(5..7)?, number(8..10)?);
    Some(days * 86_400 + number(11..13)? * 3600 + number(14..16)? * 60 + number(17..19)?)
}

fn status(state: &Value) -> &'static str {
    let name = state["name"].as_str().unwrap_or("").to_lowercase();
    match state["type"].as_str().unwrap_or("") {
        "unstarted" => "todo",
        "started" if name.contains("review") => "in_review",
        "started" => "in_progress",
        "completed" => "done",
        "canceled" if name.contains("duplicate") => "duplicate",
        "canceled" => "cancelled",
        _ => "backlog",
    }
}

struct Users(HashMap<String, i64>);

impl Users {
    fn get(&self, person: &Value) -> Option<i64> {
        self.0
            .get(&person["email"].as_str()?.to_lowercase())
            .copied()
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn milestone(tx: &Transaction, project_id: i64, issue: &Value) -> rusqlite::Result<Option<i64>> {
    let name = match (
        issue["project"]["name"].as_str(),
        issue["projectMilestone"]["name"].as_str(),
    ) {
        (Some(project), Some(milestone)) => format!("{project} / {milestone}"),
        (Some(project), None) => project.to_owned(),
        _ => return Ok(None),
    };
    tx.execute(
        "INSERT INTO milestones (project_id, name) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
        (project_id, &name),
    )?;
    tx.query_row(
        "SELECT id FROM milestones WHERE project_id = ?1 AND name = ?2",
        (project_id, &name),
        |row| row.get(0),
    )
    .map(Some)
}

fn free_number(tx: &Transaction, project_id: i64, wanted: i64) -> rusqlite::Result<i64> {
    let taken: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM issues WHERE project_id = ?1 AND number = ?2)",
        (project_id, wanted),
        |row| row.get(0),
    )?;
    if wanted > 0 && !taken {
        return Ok(wanted);
    }
    tx.query_row("UPDATE projects SET next_number = max(next_number, (SELECT coalesce(max(number), 0) + 1 FROM issues WHERE project_id = ?1)) + 1 WHERE id = ?1 RETURNING next_number - 1", [project_id], |row| row.get(0))
}

fn set_labels(tx: &Transaction, org_id: i64, id: i64, issue: &Value) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM issue_labels WHERE issue_id = ?1", [id])?;
    for label in issue["labels"]["nodes"].as_array().into_iter().flatten() {
        let (name, color) = (
            text(&label["name"]),
            label["color"]
                .as_str()
                .filter(|c| crate::store::valid_color(c))
                .unwrap_or("#8a8f98"),
        );
        if name.is_empty() || name.contains(',') {
            continue;
        }
        tx.execute(
            "INSERT INTO labels (org_id, name, color) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            (org_id, name, color),
        )?;
        tx.execute("INSERT OR IGNORE INTO issue_labels SELECT ?1, id FROM labels WHERE org_id = ?2 AND name = ?3", (id, org_id, name))?;
    }
    Ok(())
}

fn upsert_issue(
    tx: &Transaction,
    org_id: i64,
    projects: &HashMap<String, i64>,
    users: &Users,
    issue: &Value,
) -> rusqlite::Result<Option<i64>> {
    let Some(&project_id) = projects.get(text(&issue["team"]["id"])) else {
        return Ok(None);
    };
    let linear_id = text(&issue["id"]);
    let created = epoch(text(&issue["createdAt"])).unwrap_or_else(shell::now);
    let updated = epoch(text(&issue["updatedAt"])).unwrap_or(created);
    let completed =
        epoch(text(&issue["completedAt"])).or_else(|| epoch(text(&issue["canceledAt"])));
    let milestone_id = milestone(tx, project_id, issue)?;
    let estimate = issue["estimate"].as_f64().map(|e| e.round() as i64);
    let fields = (
        text(&issue["title"]),
        text(&issue["description"]),
        status(&issue["state"]),
        issue["priority"].as_i64().unwrap_or(0).clamp(0, 4),
        estimate,
        users.get(&issue["assignee"]),
        users.get(&issue["creator"]),
        milestone_id,
        text(&issue["dueDate"]),
        updated,
        completed,
    );
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM issues WHERE linear_id = ?1",
            [linear_id],
            |row| row.get(0),
        )
        .optional()?;
    let id = match existing {
        Some(id) => {
            tx.execute(
                "UPDATE issues SET title = ?2, description = ?3, status = ?4, priority = ?5, estimate = ?6, assignee = ?7, creator = ?8,
                   milestone_id = ?9, due_date = ?10, updated_at = ?11, completed_at = ?12 WHERE id = ?1",
                (id, fields.0, fields.1, fields.2, fields.3, fields.4, fields.5, fields.6, fields.7, fields.8, fields.9, fields.10),
            )?;
            id
        }
        None => {
            let number = free_number(tx, project_id, issue["number"].as_i64().unwrap_or(0))?;
            tx.execute(
                "INSERT INTO issues (org_id, project_id, number, title, description, status, priority, estimate, assignee, creator, milestone_id,
                   due_date, created_at, updated_at, completed_at, sort, linear_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, -?13, ?16)",
                rusqlite::params![
                    org_id, project_id, number, fields.0, fields.1, fields.2, fields.3, fields.4, fields.5, fields.6, fields.7, fields.8, created, fields.9, fields.10,
                    linear_id
                ],
            )?;
            tx.last_insert_rowid()
        }
    };
    set_labels(tx, org_id, id, issue)?;
    for user in [fields.5, fields.6].into_iter().flatten() {
        tx.execute(
            "INSERT OR IGNORE INTO subscriptions (issue_id, user_id) VALUES (?1, ?2)",
            (id, user),
        )?;
    }
    Ok(Some(id))
}

fn org_users(data: &Path, slug: &str) -> Result<(i64, Users), String> {
    let chi = Connection::open(data.join("chi.db")).map_err(|e| e.to_string())?;
    let org_id: i64 = chi
        .query_row("SELECT id FROM orgs WHERE slug = ?1", [slug], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no org {slug}"))?;
    let users = chi.prepare("SELECT lower(u.email), u.id FROM users u JOIN memberships m ON m.user_id = u.id WHERE m.org_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map([org_id], |row| <(String, i64)>::try_from(row))?
                .collect()
        })
        .map_err(|e| e.to_string())?;
    Ok((org_id, Users(users)))
}

fn open_tracker(data: &Path, org_id: i64, teams: &[Value]) -> Result<Connection, String> {
    let conn = Connection::open(data.join("tracker.db")).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    conn.execute_batch(include_str!("schema.sql"))
        .map_err(|e| e.to_string())?;
    for team in teams {
        let elsewhere: Option<i64> = conn
            .query_row(
                "SELECT org_id FROM projects WHERE linear_id = ?1 AND org_id != ?2",
                (text(&team["id"]), org_id),
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if elsewhere.is_some() {
            return Err(format!(
                "team {} was imported into another org; a Linear workspace goes into one org",
                text(&team["key"])
            ));
        }
    }
    Ok(conn)
}

pub fn import(data: &Path, slug: &str, key: &str) -> Result<String, String> {
    let (org_id, users) = org_users(data, slug)?;

    let teams = all(key, TEAMS, "teams")?;
    let issues = all(key, ISSUES, "issues")?;
    let relations = all(key, RELATIONS, "issueRelations")?;
    let comments = all(key, COMMENTS, "comments")?;
    let documents = all(key, DOCUMENTS, "documents")?;
    eprintln!();

    let mut conn = open_tracker(data, org_id, &teams)?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let summary = copy(
        &tx, org_id, &users, &teams, &issues, &relations, &comments, &documents,
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(summary)
}

fn copy_teams(
    tx: &Transaction,
    org_id: i64,
    teams: &[Value],
) -> rusqlite::Result<HashMap<String, i64>> {
    let mut projects = HashMap::new();
    for team in teams {
        let (linear_id, prefix) = (text(&team["id"]), text(&team["key"]).to_uppercase());
        if !crate::store::valid_prefix(&prefix) {
            eprintln!("skipping team {prefix}: not a usable prefix");
            continue;
        }
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM projects WHERE linear_id = ?1 OR (org_id = ?2 AND prefix = ?3)",
                (linear_id, org_id, &prefix),
                |row| row.get(0),
            )
            .optional()?;
        let id = match id {
            Some(id) => {
                tx.execute(
                    "UPDATE projects SET linear_id = ?2 WHERE id = ?1",
                    (id, linear_id),
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO projects (org_id, prefix, name, description, linear_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (org_id, &prefix, text(&team["name"]), text(&team["description"]), linear_id, shell::now()),
                )?;
                tx.last_insert_rowid()
            }
        };
        projects.insert(linear_id.to_owned(), id);
    }
    Ok(projects)
}

fn copy_issues<'a>(
    tx: &Transaction,
    org_id: i64,
    projects: &HashMap<String, i64>,
    users: &Users,
    issues: &'a [Value],
) -> rusqlite::Result<HashMap<&'a str, i64>> {
    let mut ids: HashMap<&str, i64> = HashMap::new();
    for issue in issues {
        if let Some(id) = upsert_issue(tx, org_id, projects, users, issue)? {
            ids.insert(text(&issue["id"]), id);
        }
    }
    for issue in issues {
        if let (Some(id), Some(parent)) = (
            ids.get(text(&issue["id"])),
            issue["parent"]["id"]
                .as_str()
                .and_then(|parent| ids.get(parent)),
        ) {
            tx.execute(
                "UPDATE issues SET parent_id = ?2 WHERE id = ?1",
                (id, parent),
            )?;
        }
    }
    tx.execute("UPDATE projects SET next_number = max(next_number, (SELECT coalesce(max(number), 0) + 1 FROM issues WHERE project_id = projects.id)) WHERE org_id = ?1", [org_id])?;
    Ok(ids)
}

fn copy_relations(
    tx: &Transaction,
    ids: &HashMap<&str, i64>,
    relations: &[Value],
) -> rusqlite::Result<usize> {
    let mut related = 0;
    for relation in relations {
        let kind = match text(&relation["type"]) {
            "blocks" => "blocks",
            "duplicate" => "duplicate",
            _ => "related",
        };
        if let (Some(a), Some(b)) = (
            ids.get(text(&relation["issue"]["id"])),
            ids.get(text(&relation["relatedIssue"]["id"])),
        ) {
            related += tx.execute(
                "INSERT OR IGNORE INTO relations (issue_id, kind, other_id) VALUES (?1, ?2, ?3)",
                (a, kind, b),
            )?;
        }
    }
    Ok(related)
}

fn copy_comments(
    tx: &Transaction,
    ids: &HashMap<&str, i64>,
    users: &Users,
    comments: &[Value],
) -> rusqlite::Result<usize> {
    let mut said = 0;
    for comment in comments {
        let Some(issue_id) = ids.get(text(&comment["issue"]["id"])) else {
            continue;
        };
        let created = epoch(text(&comment["createdAt"])).unwrap_or_else(shell::now);
        said += tx.execute(
            "INSERT INTO comments (issue_id, author, body, created_at, updated_at, linear_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (linear_id) DO UPDATE SET body = excluded.body, updated_at = excluded.updated_at",
            (issue_id, users.get(&comment["user"]), text(&comment["body"]), created, epoch(text(&comment["updatedAt"])).unwrap_or(created), text(&comment["id"])),
        )?;
    }
    Ok(said)
}

fn copy_documents(
    tx: &Transaction,
    org_id: i64,
    users: &Users,
    documents: &[Value],
) -> rusqlite::Result<usize> {
    let mut written = 0;
    for document in documents {
        let created = epoch(text(&document["createdAt"])).unwrap_or_else(shell::now);
        let title = Some(text(&document["title"]))
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Untitled");
        written += tx.execute(
            "INSERT INTO documents (org_id, title, body, author, created_at, updated_at, linear_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (linear_id) DO UPDATE SET title = excluded.title, body = excluded.body, updated_at = excluded.updated_at WHERE documents.org_id = excluded.org_id",
            (org_id, title, text(&document["content"]), users.get(&document["creator"]), created, epoch(text(&document["updatedAt"])).unwrap_or(created), text(&document["id"])),
        )?;
    }
    Ok(written)
}

#[expect(
    clippy::too_many_arguments,
    reason = "one slice per fetched Linear connection"
)]
fn copy(
    tx: &Transaction,
    org_id: i64,
    users: &Users,
    teams: &[Value],
    issues: &[Value],
    relations: &[Value],
    comments: &[Value],
    documents: &[Value],
) -> rusqlite::Result<String> {
    let projects = copy_teams(tx, org_id, teams)?;
    let ids = copy_issues(tx, org_id, &projects, users, issues)?;
    let related = copy_relations(tx, &ids, relations)?;
    let said = copy_comments(tx, &ids, users, comments)?;
    let written = copy_documents(tx, org_id, users, documents)?;
    Ok(format!(
        "imported {} projects, {} issues, {related} relations, {said} comments, {written} documents",
        projects.len(),
        ids.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_timestamps_become_epoch_seconds() {
        assert_eq!(epoch("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(epoch("2026-11-13T14:34:00.123Z"), Some(1_794_580_440));
        assert_eq!(epoch("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(epoch("yesterday"), None);
    }

    #[test]
    fn a_rerun_updates_instead_of_duplicating() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        let users = Users(HashMap::from([("ana@x.y".to_owned(), 5)]));
        let teams = [json!({ "id": "t", "key": "eng", "name": "Engineering" })];
        let issue = |title: &str| {
            json!({ "id": "i1", "number": 7, "title": title, "team": { "id": "t" }, "state": { "type": "started", "name": "In Review" },
                    "assignee": { "email": "ANA@x.y" }, "createdAt": "2026-01-01T00:00:00.000Z", "project": { "name": "Launch" },
                    "labels": { "nodes": [{ "name": "bug", "color": "#ff0000" }] } })
        };
        for title in ["first", "second"] {
            let tx = conn.transaction().unwrap();
            copy(&tx, 1, &users, &teams, &[issue(title)], &[], &[], &[]).unwrap();
            tx.commit().unwrap();
        }
        let row: (String, i64, String, i64, i64) = conn
            .query_row("SELECT title, number, status, assignee, (SELECT next_number FROM projects) FROM issues", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap();
        assert_eq!(row, ("second".to_owned(), 7, "in_review".to_owned(), 5, 8));
    }
}
