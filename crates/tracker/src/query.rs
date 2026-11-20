use rusqlite::types::Value;

use shell::find::tokens;

use crate::{OPEN, PRIORITIES, People, STATUSES, split_identifier};

pub const HELP: &str = "status: assignee:me|none|handle creator: label:|none project: priority: milestone:|none parent:ID|none \
has:error|parent|children|estimate|due|links is:open|closed|blocked due:overdue since:7d before:2026-11-01 sort:updated|created|priority|due|manual, -key: negates, other words search the text";

pub struct Compiled {
    pub condition: String,
    pub params: Vec<Value>,
    pub order: Option<&'static str>,
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn fts(words: &[String]) -> String {
    words
        .iter()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn order(name: &str) -> Option<&'static str> {
    Some(match name {
        "updated" => "i.updated_at DESC",
        "created" => "i.created_at DESC",
        "priority" => "i.priority = 0, i.priority, i.updated_at DESC",
        "due" => "i.due_date = '', i.due_date, i.priority = 0, i.priority",
        "manual" => "i.sort, i.id DESC",
        _ => return None,
    })
}

fn people(
    values: &[&str],
    me: i64,
    people: &People,
    params: &mut Vec<Value>,
    column: &str,
) -> String {
    let none = values.contains(&"none");
    let ids: Vec<i64> = values
        .iter()
        .filter(|value| **value != "none")
        .map(|value| {
            if *value == "me" {
                me
            } else {
                people.find(value).map_or(-1, |person| person.id)
            }
        })
        .collect();
    let mut parts = Vec::new();
    if !ids.is_empty() {
        parts.push(format!("{column} IN ({})", placeholders(ids.len())));
        params.extend(ids.into_iter().map(Value::from));
    }
    if none {
        parts.push(format!("{column} IS NULL"));
    }
    parts.join(" OR ")
}

fn filter(token: &str) -> Option<(&str, &str)> {
    token.split_once(':').filter(|(key, value)| {
        !value.is_empty()
            && !value.starts_with("//")
            && key
                .trim_start_matches('-')
                .chars()
                .all(|c| c.is_ascii_lowercase())
            && !key.is_empty()
    })
}

fn statuses(lower: &[String], params: &mut Vec<Value>) -> Result<String, String> {
    let mut wanted = Vec::new();
    for value in lower {
        match value.as_str() {
            "open" => wanted.extend(OPEN),
            "closed" => wanted.extend(STATUSES.iter().filter(|status| !OPEN.contains(status))),
            value => wanted.push(
                *STATUSES
                    .iter()
                    .find(|status| **status == value)
                    .ok_or(format!("unknown status {value}"))?,
            ),
        }
    }
    params.extend(
        wanted
            .iter()
            .map(|status| Value::from((*status).to_owned())),
    );
    Ok(format!("i.status IN ({})", placeholders(wanted.len())))
}

fn is(lower: &[String]) -> Result<String, String> {
    Ok(match lower.first().map(String::as_str) {
        Some("open") => format!("i.status IN ('{}')", OPEN.join("', '")),
        Some("closed") => format!("i.status NOT IN ('{}')", OPEN.join("', '")),
        Some("blocked") => format!(
            "EXISTS (SELECT 1 FROM relations r JOIN issues b ON b.id = r.issue_id WHERE r.other_id = i.id AND r.kind = 'blocks' AND b.status IN ('{}'))",
            OPEN.join("', '")
        ),
        _ => return Err("is: takes open, closed or blocked".to_owned()),
    })
}

fn has(lower: &[String]) -> Result<String, String> {
    Ok(match lower.first().map(String::as_str) {
        Some("error") => "i.error_ref IS NOT NULL",
        Some("parent") => "i.parent_id IS NOT NULL",
        Some("children") => "EXISTS (SELECT 1 FROM issues c WHERE c.parent_id = i.id)",
        Some("estimate") => "i.estimate IS NOT NULL",
        Some("due") => "i.due_date != ''",
        Some("links") => "EXISTS (SELECT 1 FROM links k WHERE k.issue_id = i.id)",
        _ => return Err("has: takes error, parent, children, estimate, due or links".to_owned()),
    }
    .to_owned())
}

fn priorities(lower: &[String], params: &mut Vec<Value>) -> Result<String, String> {
    let mut wanted = Vec::new();
    for value in lower {
        wanted.push(priority(value).ok_or(format!("unknown priority {value}"))?);
    }
    params.extend(wanted.iter().map(|p| Value::from(*p)));
    Ok(format!("i.priority IN ({})", placeholders(wanted.len())))
}

fn projects(values: &[&str], params: &mut Vec<Value>) -> String {
    for value in values {
        params.extend([
            Value::from((*value).to_owned()),
            Value::from((*value).to_owned()),
        ]);
    }
    vec!["(p.prefix = ? COLLATE NOCASE OR p.name = ? COLLATE NOCASE)"; values.len()].join(" OR ")
}

fn named(values: &[&str], params: &mut Vec<Value>, sql: &str) -> String {
    params.extend(values.iter().map(|value| Value::from((*value).to_owned())));
    sql.replace("{}", &placeholders(values.len()))
}

fn parent(value: &str, params: &mut Vec<Value>) -> Result<String, String> {
    let (prefix, number) =
        split_identifier(value).ok_or("parent: takes an issue id such as CHI-12")?;
    params.extend([Value::from(prefix), Value::from(number)]);
    Ok("(pp.prefix = ? AND pi.number = ?)".to_owned())
}

fn moment(key: &str, lower: &[String], params: &mut Vec<Value>) -> Result<String, String> {
    let op = match key {
        "in" => return Ok("1".to_owned()),
        "since" => ">=",
        "before" => "<",
        _ => return Err(format!("unknown filter {key}: — {HELP}")),
    };
    params.push(Value::from(
        shell::find::when(&lower[0]).ok_or(shell::find::TIME_HELP)?,
    ));
    Ok(format!("i.updated_at {op} ?"))
}

fn condition(
    key: &str,
    value: &str,
    values: &[&str],
    lower: &[String],
    me: i64,
    who: &People,
    params: &mut Vec<Value>,
) -> Result<String, String> {
    Ok(match key {
        "status" => statuses(lower, params)?,
        "is" => is(lower)?,
        "assignee" => people(values, me, who, params, "i.assignee"),
        "creator" => people(values, me, who, params, "i.creator"),
        "project" => projects(values, params),
        "priority" => priorities(lower, params)?,
        "has" => has(lower)?,
        _ => link(key, value, values, lower, params)?,
    })
}

fn link(
    key: &str,
    value: &str,
    values: &[&str],
    lower: &[String],
    params: &mut Vec<Value>,
) -> Result<String, String> {
    let none = lower == ["none"];
    Ok(match key {
        "label" if none => {
            "NOT EXISTS (SELECT 1 FROM issue_labels il WHERE il.issue_id = i.id)".to_owned()
        }
        "label" => named(
            values,
            params,
            "EXISTS (SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id WHERE il.issue_id = i.id AND l.name COLLATE NOCASE IN ({}))",
        ),
        "milestone" if none => "i.milestone_id IS NULL".to_owned(),
        "milestone" => named(values, params, "m.name COLLATE NOCASE IN ({})"),
        "parent" if none => "i.parent_id IS NULL".to_owned(),
        "parent" => parent(value, params)?,
        "due" if lower == ["overdue"] => format!(
            "i.due_date != '' AND i.due_date < date('now') AND i.status IN ('{}')",
            OPEN.join("', '")
        ),
        _ => moment(key, lower, params)?,
    })
}

fn search(text: &[String], conditions: &mut Vec<String>, params: &mut Vec<Value>) {
    match text {
        [] => {}
        [word] if split_identifier(word).is_some() => {
            let (prefix, number) = split_identifier(word).expect("checked above");
            conditions.push("(p.prefix = ? AND i.number = ?)".to_owned());
            params.extend([Value::from(prefix), Value::from(number)]);
        }
        words => {
            conditions
                .push("i.id IN (SELECT rowid FROM issues_fts WHERE issues_fts MATCH ?)".to_owned());
            params.push(Value::from(fts(words)));
        }
    }
}

pub fn compile(query: &str, me: i64, who: &People) -> Result<Compiled, String> {
    let (mut conditions, mut params, mut text, mut sort) =
        (Vec::new(), Vec::new(), Vec::new(), None);
    for token in tokens(query) {
        let Some((key, value)) = filter(&token) else {
            text.push(token);
            continue;
        };
        let (negated, key) = match key.strip_prefix('-') {
            Some(key) => (true, key),
            None => (false, key),
        };
        let values: Vec<&str> = value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect();
        if values.is_empty() {
            return Err(format!("{key}: needs a value"));
        }
        let lower: Vec<String> = values.iter().map(|value| value.to_lowercase()).collect();
        if key == "sort" {
            sort = Some(order(&lower[0]).ok_or(format!("unknown sort {}", lower[0]))?);
            continue;
        }
        let condition = condition(key, value, &values, &lower, me, who, &mut params)?;
        conditions.push(if negated {
            format!("NOT coalesce(({condition}), 0)")
        } else {
            format!("({condition})")
        });
    }
    search(&text, &mut conditions, &mut params);
    Ok(Compiled {
        condition: if conditions.is_empty() {
            "1".to_owned()
        } else {
            conditions.join(" AND ")
        },
        params,
        order: sort,
    })
}

pub fn priority(value: &str) -> Option<i64> {
    let value = value.trim().to_lowercase();
    PRIORITIES
        .iter()
        .position(|name| *name == value)
        .map(|p| p as i64)
        .or_else(|| value.parse().ok().filter(|p| (0..=4).contains(p)))
}

pub fn search_terms(text: &str) -> String {
    fts(&tokens(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_words_stay_together_and_keys_compile() {
        assert_eq!(
            tokens(r#"label:"needs review" crash  on"#),
            ["label:needs review", "crash", "on"]
        );
        let who = People(Vec::new());
        let compiled = compile(
            "status:todo,open -assignee:me,none sort:priority crash",
            7,
            &who,
        )
        .unwrap();
        assert!(
            compiled
                .condition
                .contains("NOT coalesce((i.assignee IN (?) OR i.assignee IS NULL), 0)"),
            "{}",
            compiled.condition
        );
        assert_eq!(compiled.order, order("priority"));
        assert_eq!(compiled.params.len(), 5 + 1 + 1);
        assert_eq!(
            compiled.params.last(),
            Some(&Value::from("\"crash\"*".to_owned()))
        );
        assert!(compile("color:red", 1, &who).is_err());
        assert_eq!(compile("in:issues since:2d", 1, &who).unwrap().params.len(), 1);
        assert!(compile("status:nope", 1, &who).is_err());
        assert!(compile("sort:,", 1, &who).is_err());
        assert!(compile("-project:,", 1, &who).is_err());
        let exact = compile("CHI-12", 1, &who).unwrap();
        assert_eq!(
            exact.params,
            [Value::from("CHI".to_owned()), Value::from(12)]
        );
        assert_eq!(compile("http://x.y/a:b", 1, &who).unwrap().params.len(), 1);
    }
}
