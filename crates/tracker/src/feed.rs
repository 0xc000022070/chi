use rusqlite::{params_from_iter, types::Value};
use shell::{Ctx, Failure, Item, find};

use crate::{Issue, People, Tracker, describe, problem};

pub const SCOPES: [(&str, &str); 4] = [
    ("issues", crate::query::HELP),
    ("docs", "project: by:me|handle"),
    ("projects", "status: lead:me|handle"),
    ("inbox", "is:unread|read"),
];

const DOCS: &str = "SELECT d.id, d.title, coalesce(p.prefix, ''), d.author, d.updated_at, d.body FROM documents d LEFT JOIN projects p ON p.id = d.project_id WHERE d.org_id = ?";
const PROJECTS: &str = "SELECT p.id, p.name, p.prefix, p.lead, p.created_at, p.status FROM projects p WHERE p.org_id = ?";
const INBOX: &str = "SELECT n.id, n.text, coalesce(p.prefix || '-' || i.number, ''), n.actor, n.created_at, CASE WHEN n.read_at IS NULL THEN 'unread' ELSE '' END
     FROM notifications n LEFT JOIN issues i ON i.id = n.issue_id LEFT JOIN projects p ON p.id = i.project_id WHERE n.org_id = ? AND n.user_id = ?";
const ACTIVITY: &str = "SELECT * FROM (
       SELECT i.created_at AS at, 'created' AS kind, p.prefix || '-' || i.number AS ref, i.title, i.creator, '', '', ''
         FROM issues i JOIN projects p ON p.id = i.project_id WHERE i.org_id = ?1 AND i.created_at < ?2
       UNION ALL
       SELECT h.at, 'changed', p.prefix || '-' || i.number, i.title, h.actor, h.field, h.old, h.new
         FROM history h JOIN issues i ON i.id = h.issue_id JOIN projects p ON p.id = i.project_id WHERE i.org_id = ?1 AND h.at < ?2
       UNION ALL
       SELECT c.created_at, 'commented', p.prefix || '-' || i.number, i.title, c.author, '', '', c.body
         FROM comments c JOIN issues i ON i.id = c.issue_id JOIN projects p ON p.id = i.project_id WHERE i.org_id = ?1 AND c.created_at < ?2
       UNION ALL
       SELECT d.updated_at, CASE WHEN d.updated_at = d.created_at THEN 'wrote' ELSE 'edited' END, CAST(d.id AS TEXT), d.title, d.author, '', '', ''
         FROM documents d WHERE d.org_id = ?1 AND d.updated_at < ?2
     ) ORDER BY at DESC LIMIT ?3";

type Row = (i64, String, String, Option<i64>, i64, String);
type Change = (i64, String, String, String, Option<i64>, String, String, String);

fn people(term: &find::Term, me: i64, who: &People) -> Vec<Value> {
    term.values
        .iter()
        .map(|value| {
            Value::from(if value == "me" {
                me
            } else {
                who.find(value).map_or(-1, |person| person.id)
            })
        })
        .collect()
}

fn excerpt(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    match line.char_indices().nth(140) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

fn handle(who: &People, id: Option<i64>) -> String {
    who.get(id).map(|person| format!("@{}", person.handle())).unwrap_or_default()
}

fn joined(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty() && part != "none")
        .collect::<Vec<_>>()
        .join(" · ")
}

fn lower(term: &find::Term) -> Vec<Value> {
    term.lower().into_iter().map(Value::from).collect()
}

fn issue_item(issue: Issue, who: &People) -> Item {
    Item {
        at: issue.updated_at,
        kind: "issue",
        title: format!("{} {}", issue.identifier(), issue.title),
        url: format!("issues/{}", issue.identifier()),
        badge: issue.status.replace('_', " "),
        detail: joined([issue.project.clone(), issue.priority_name().to_owned(), handle(who, issue.assignee)]),
    }
}

fn docs(query: &str, me: i64, who: &People) -> Result<find::Filter, String> {
    find::compile(query, "d.updated_at", &["d.title", "d.body"], |term, filter| {
        match term.key.as_str() {
            "project" => {
                let sql = vec!["? IN (lower(p.prefix), lower(p.name))"; term.values.len()].join(" OR ");
                filter.add(term.negated, &sql, lower(term));
            }
            "by" | "author" => filter.add(term.negated, "d.author IN ({})", people(term, me, who)),
            _ => return Ok(false),
        }
        Ok(true)
    })
}

fn projects(query: &str, me: i64, who: &People) -> Result<find::Filter, String> {
    find::compile(query, "p.created_at", &["p.name", "p.prefix", "p.description"], |term, filter| {
        match term.key.as_str() {
            "status" => filter.add(term.negated, "lower(p.status) IN ({})", lower(term)),
            "lead" => filter.add(term.negated, "p.lead IN ({})", people(term, me, who)),
            _ => return Ok(false),
        }
        Ok(true)
    })
}

fn inbox(query: &str) -> Result<find::Filter, String> {
    find::compile(query, "n.created_at", &["n.text"], |term, filter| {
        if term.key != "is" {
            return Ok(false);
        }
        let sql = match term.lower().first().map(String::as_str) {
            Some("unread") => "n.read_at IS NULL",
            Some("read") => "n.read_at IS NOT NULL",
            _ => return Err("is: takes unread or read".to_owned()),
        };
        filter.add(term.negated, sql, Vec::new());
        Ok(true)
    })
}

fn row_item(scope: &str, (id, title, key, person, at, extra): Row, who: &People) -> Item {
    match scope {
        "docs" => Item {
            at,
            kind: "doc",
            title,
            url: format!("docs/{id}"),
            badge: key,
            detail: joined([handle(who, person), excerpt(&extra)]),
        },
        "projects" => Item {
            at,
            kind: "project",
            title: format!("{key} {title}"),
            url: format!("projects/{key}"),
            badge: extra,
            detail: handle(who, person),
        },
        _ => Item {
            at,
            kind: "notification",
            url: if key.is_empty() { "inbox".to_owned() } else { format!("issues/{key}") },
            title,
            badge: extra,
            detail: joined([key, handle(who, person)]),
        },
    }
}

fn change_item((at, kind, key, title, actor, field, old, new): Change, who: &People) -> Item {
    let actor = who.get(actor).map_or_else(|| "someone".to_owned(), |person| format!("@{}", person.handle()));
    let detail = match kind.as_str() {
        "changed" => format!("{actor} {}", describe(&field, &old, &new)),
        "commented" => format!("{actor}: {}", excerpt(&new)),
        "created" => format!("{actor} opened it"),
        kind => format!("{actor} {kind} it"),
    };
    if kind == "wrote" || kind == "edited" {
        return Item { at, kind: "doc", title, url: format!("docs/{key}"), badge: "doc".to_owned(), detail };
    }
    Item { at, kind: "issue", title: format!("{key} {title}"), url: format!("issues/{key}"), badge: kind, detail }
}

impl Tracker {
    pub async fn find(
        &self,
        ctx: &Ctx,
        scope: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Item>, Failure> {
        let who = self.people(ctx.org_id).await?;
        let me = ctx.user_id;
        let (sql, filter) = match scope {
            "issues" => {
                let issues = self.list(ctx, query, "updated", limit).await?;
                return Ok(issues.into_iter().map(|issue| issue_item(issue, &who)).collect());
            }
            "docs" => (DOCS, docs(query, me, &who)),
            "projects" => (PROJECTS, projects(query, me, &who)),
            "inbox" => (INBOX, inbox(query)),
            _ => return Err(Failure::not_found()),
        };
        let filter = filter.map_err(problem)?;
        let mut params = vec![Value::from(ctx.org_id)];
        if scope == "inbox" {
            params.push(Value::from(me));
        }
        params.extend(filter.params.iter().cloned());
        params.push(Value::from(limit));
        let sql = format!("{sql} AND {} ORDER BY 5 DESC LIMIT ?", filter.condition());
        let rows = self.rows(sql, params).await?;
        Ok(rows.into_iter().map(|row| row_item(scope, row, &who)).collect())
    }

    async fn rows(&self, sql: String, params: Vec<Value>) -> Result<Vec<Row>, Failure> {
        self.db
            .call(move |conn| {
                conn.prepare(&sql)?
                    .query_map(params_from_iter(params), |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
                    })?
                    .collect()
            })
            .await
    }

    pub async fn activity(&self, org_id: i64, before: i64, limit: i64) -> Result<Vec<Item>, Failure> {
        let who = self.people(org_id).await?;
        let rows: Vec<Change> = self
            .db
            .call(move |conn| {
                conn.prepare(ACTIVITY)?
                    .query_map((org_id, before, limit), |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?))
                    })?
                    .collect()
            })
            .await?;
        Ok(rows.into_iter().map(|row| change_item(row, &who)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::excerpt;

    #[test]
    fn excerpts_take_the_first_line_and_cut_long_ones() {
        assert_eq!(excerpt("\n  \nhello\nworld"), "hello");
        assert_eq!(excerpt(&"é".repeat(200)).chars().count(), 141);
    }
}
