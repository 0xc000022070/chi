use std::{collections::HashMap, io::Write};

use flate2::{Compression, write::GzEncoder};
use rusqlite::{Connection, OptionalExtension, Result};

use crate::{Incoming, envelope};

const DAY: i64 = 86_400;
const MAX_NEW_ISSUES_PER_DAY: i64 = 200;
const MAX_STORED_PER_ISSUE_PER_HOUR: i64 = 20;
const MAX_STORED_PER_ISSUE: i64 = 100;
const MAX_STORED_PER_PROJECT: i64 = 50_000;
const MAX_STORED_PAYLOAD: usize = 256 * 1024;
const RETENTION: i64 = 90 * DAY;
const OVERFLOW: &str = "overflow";

pub struct Alert {
    pub kind: &'static str,
    pub org_id: i64,
    pub project_id: i64,
    pub project: String,
    pub issue_id: i64,
    pub title: String,
    pub culprit: String,
    pub level: String,
}

pub fn add_issue_environment(conn: &Connection) -> Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('issues') WHERE name = 'environment')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        conn.execute_batch(
            "ALTER TABLE issues ADD COLUMN environment TEXT NOT NULL DEFAULT '';
             UPDATE issues SET environment = coalesce(
               (SELECT environment FROM events WHERE issue_id = issues.id ORDER BY id DESC LIMIT 1), '');",
        )?;
    }
    Ok(())
}

pub fn keys(conn: &Connection) -> Result<HashMap<String, i64>> {
    conn.prepare("SELECT public_key, project_id FROM keys")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let _ = encoder.write_all(text.as_bytes());
    encoder.finish().unwrap_or_default()
}

pub fn write(conn: &mut Connection, batch: &[Incoming], now: i64) -> Result<Vec<Alert>> {
    let tx = conn.transaction()?;
    let mut alerts = Vec::new();
    for incoming in batch {
        alerts.extend(write_one(&tx, incoming, now)?);
    }
    tx.commit()?;
    Ok(alerts)
}

fn find(conn: &Connection, project_id: i64, fingerprint: &str) -> Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT id, status FROM issues WHERE project_id = ?1 AND fingerprint = ?2",
        (project_id, fingerprint),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

fn locate(conn: &Connection, project_id: i64, summary: envelope::Summary, now: i64) -> Result<(envelope::Summary, Option<(i64, String)>)> {
    let found = find(conn, project_id, &summary.fingerprint)?;
    if found.is_some() {
        return Ok((summary, found));
    }
    let fresh: i64 = conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE project_id = ?1 AND first_seen > ?2",
        (project_id, now - DAY),
        |row| row.get(0),
    )?;
    if fresh < MAX_NEW_ISSUES_PER_DAY {
        return Ok((summary, None));
    }
    let overflow = envelope::Summary {
        fingerprint: OVERFLOW.to_owned(),
        title: "Too many new issues today: the rest are collected here".to_owned(),
        culprit: String::new(),
        level: "error".to_owned(),
    };
    let found = find(conn, project_id, OVERFLOW)?;
    Ok((overflow, found))
}

fn count(
    conn: &Connection,
    project_id: i64,
    summary: &envelope::Summary,
    found: Option<(i64, String)>,
    environment: &str,
    now: i64,
) -> Result<(i64, Option<&'static str>)> {
    let Some((id, status)) = found else {
        conn.execute(
            "INSERT INTO issues (project_id, fingerprint, title, culprit, level, first_seen, last_seen, environment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            (project_id, &summary.fingerprint, &summary.title, &summary.culprit, &summary.level, now, environment),
        )?;
        return Ok((conn.last_insert_rowid(), Some("issue.new")));
    };
    conn.execute(
        "UPDATE issues SET title = ?2, culprit = ?3, level = ?4, last_seen = ?5, count = count + 1, environment = ?6,
           status = CASE status WHEN 'resolved' THEN 'open' ELSE status END
         WHERE id = ?1",
        (id, &summary.title, &summary.culprit, &summary.level, now, environment),
    )?;
    Ok((id, (status == "resolved").then_some("issue.regressed")))
}

fn store(conn: &Connection, issue_id: i64, event: &serde_json::Value, now: i64) -> Result<()> {
    let stored_this_hour: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE issue_id = ?1 AND received_at > ?2",
        (issue_id, now - 3600),
        |row| row.get(0),
    )?;
    let payload = gzip(&event.to_string());
    if stored_this_hour >= MAX_STORED_PER_ISSUE_PER_HOUR || payload.len() > MAX_STORED_PAYLOAD {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO events (issue_id, event_id, received_at, release, environment, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (issue_id, event["event_id"].as_str(), now, event["release"].as_str(), event["environment"].as_str(), payload),
    )?;
    conn.execute(
        "DELETE FROM events WHERE issue_id = ?1 AND id NOT IN
           (SELECT id FROM events WHERE issue_id = ?1 ORDER BY id DESC LIMIT ?2)",
        (issue_id, MAX_STORED_PER_ISSUE),
    )?;
    Ok(())
}

fn write_one(conn: &Connection, Incoming { project_id, event }: &Incoming, now: i64) -> Result<Option<Alert>> {
    let project: Option<(i64, String)> = conn
        .query_row("SELECT org_id, name FROM projects WHERE id = ?1", [project_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((org_id, project)) = project else { return Ok(None) };

    let (summary, found) = locate(conn, *project_id, envelope::summarize(event), now)?;
    let environment = event["environment"].as_str().unwrap_or("");
    let (issue_id, kind) = count(conn, *project_id, &summary, found, environment, now)?;
    store(conn, issue_id, event, now)?;

    Ok(kind.filter(|_| matches!(summary.level.as_str(), "error" | "fatal")).map(|kind| Alert {
        kind,
        org_id,
        project_id: *project_id,
        project,
        issue_id,
        title: summary.title,
        culprit: summary.culprit,
        level: summary.level,
    }))
}

pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute("DELETE FROM events WHERE received_at < ?1", [now - RETENTION])?;
    conn.execute("DELETE FROM issues WHERE last_seen < ?1", [now - RETENTION])?;
    let projects: Vec<i64> = conn.prepare("SELECT id FROM projects")?.query_map([], |row| row.get(0))?.collect::<Result<_>>()?;
    for project_id in projects {
        conn.execute(
            "DELETE FROM events WHERE id IN
               (SELECT e.id FROM events e JOIN issues i ON i.id = e.issue_id
                WHERE i.project_id = ?1 ORDER BY e.id DESC LIMIT -1 OFFSET ?2)",
            (project_id, MAX_STORED_PER_PROJECT),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute("INSERT INTO projects (id, org_id, name) VALUES (1, 7, 'api')", []).unwrap();
        conn
    }

    fn event(kind: &str, level: &str) -> Incoming {
        Incoming { project_id: 1, event: json!({"level": level, "exception": {"values": [{"type": kind, "value": "x"}]}}) }
    }

    fn scalar(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn alerts_once_per_new_issue_and_again_on_regression() {
        let mut conn = seeded();
        let alerts = write(&mut conn, &[event("A", "error"), event("A", "error")], 1000).unwrap();
        assert_eq!(alerts.iter().map(|a| a.kind).collect::<Vec<_>>(), ["issue.new"]);
        assert_eq!((alerts[0].org_id, alerts[0].project.as_str()), (7, "api"));
        assert_eq!(scalar(&conn, "SELECT count FROM issues"), 2);

        conn.execute("UPDATE issues SET status = 'resolved'", []).unwrap();
        let alerts = write(&mut conn, &[event("A", "error")], 2000).unwrap();
        assert_eq!(alerts.iter().map(|a| a.kind).collect::<Vec<_>>(), ["issue.regressed"]);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM issues WHERE status = 'open'"), 1);

        conn.execute("UPDATE issues SET status = 'ignored'", []).unwrap();
        assert!(write(&mut conn, &[event("A", "error")], 3000).unwrap().is_empty());
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM issues WHERE status = 'ignored'"), 1);
    }

    #[test]
    fn issues_take_the_latest_environment_and_old_rows_are_backfilled() {
        let mut conn = seeded();
        let mut dev = event("A", "error");
        dev.event["environment"] = json!("development");
        write(&mut conn, &[dev], 1000).unwrap();
        assert_eq!(conn.query_row("SELECT environment FROM issues", [], |row| row.get::<_, String>(0)).unwrap(), "development");

        conn.execute_batch("ALTER TABLE issues DROP COLUMN environment").unwrap();
        add_issue_environment(&conn).unwrap();
        add_issue_environment(&conn).unwrap();
        assert_eq!(conn.query_row("SELECT environment FROM issues", [], |row| row.get::<_, String>(0)).unwrap(), "development");
    }

    #[test]
    fn info_events_are_counted_but_never_alert() {
        let mut conn = seeded();
        assert!(write(&mut conn, &[event("Expected", "info"), event("Expected", "info")], 1000).unwrap().is_empty());
        assert_eq!(scalar(&conn, "SELECT count FROM issues"), 2);
    }

    #[test]
    fn repeats_past_the_hourly_cap_are_counted_not_stored() {
        let mut conn = seeded();
        let burst: Vec<Incoming> = (0..MAX_STORED_PER_ISSUE_PER_HOUR + 30).map(|_| event("A", "error")).collect();
        write(&mut conn, &burst, 1000).unwrap();
        assert_eq!(scalar(&conn, "SELECT count FROM issues"), MAX_STORED_PER_ISSUE_PER_HOUR + 30);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM events"), MAX_STORED_PER_ISSUE_PER_HOUR);
        write(&mut conn, &[event("A", "error")], 1000 + 3601).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM events"), MAX_STORED_PER_ISSUE_PER_HOUR + 1);
    }

    #[test]
    fn new_issues_past_the_daily_cap_share_one_overflow_issue() {
        let mut conn = seeded();
        let flood: Vec<Incoming> = (0..MAX_NEW_ISSUES_PER_DAY + 50).map(|i| event(&format!("E{i}"), "error")).collect();
        let alerts = write(&mut conn, &flood, 1000).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM issues"), MAX_NEW_ISSUES_PER_DAY + 1);
        assert_eq!(scalar(&conn, "SELECT count FROM issues WHERE fingerprint = 'overflow'"), 50);
        assert_eq!(alerts.len() as i64, MAX_NEW_ISSUES_PER_DAY + 1);
        let next_day = write(&mut conn, &[event("Fresh", "error")], 1000 + DAY + 1).unwrap();
        assert_eq!(next_day[0].title, "Fresh: x");
    }

    #[test]
    fn prune_drops_what_is_past_retention() {
        let mut conn = seeded();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        write(&mut conn, &[event("Old", "error")], 1000).unwrap();
        write(&mut conn, &[event("New", "error")], 1000 + RETENTION).unwrap();
        prune(&conn, 1001 + RETENTION).unwrap();
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM issues"), 1);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM events"), 1);
    }
}
