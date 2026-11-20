use rusqlite::types::Value;

use crate::now;

pub struct Item {
    pub at: i64,
    pub kind: &'static str,
    pub title: String,
    pub url: String,
    pub badge: String,
    pub detail: String,
}

pub const TIME_HELP: &str = "since: and before: take 12h, 7d, 2w, today, yesterday or a date like 2026-11-01";

pub fn tokens(query: &str) -> Vec<String> {
    let (mut out, mut word, mut quoted) = (Vec::new(), String::new(), false);
    for c in query.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

pub fn join(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| {
            if token.contains(char::is_whitespace) {
                format!("\"{token}\"")
            } else {
                token.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Term {
    pub negated: bool,
    pub key: String,
    pub values: Vec<String>,
}

impl Term {
    pub fn lower(&self) -> Vec<String> {
        self.values.iter().map(|v| v.to_lowercase()).collect()
    }

    pub fn params(&self) -> Vec<Value> {
        self.values.iter().cloned().map(Value::from).collect()
    }
}

pub fn term(token: &str) -> Option<Term> {
    let (key, value) = token.split_once(':')?;
    let (negated, key) = match key.strip_prefix('-') {
        Some(key) => (true, key),
        None => (false, key),
    };
    if value.is_empty()
        || value.starts_with("//")
        || key.is_empty()
        || !key.chars().all(|c| c.is_ascii_lowercase())
    {
        return None;
    }
    Some(Term {
        negated,
        key: key.to_owned(),
        values: value
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

fn day_number(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

pub fn when(value: &str) -> Option<i64> {
    let value = value.trim().to_lowercase();
    let (now, day) = (now(), 86_400);
    let midnight = now - now.rem_euclid(day);
    if value == "today" {
        return Some(midnight);
    }
    if value == "yesterday" {
        return Some(midnight - day);
    }
    if let Some(unit) = value.chars().last().filter(|c| "hdw".contains(*c)) {
        let count: i64 = value[..value.len() - 1].parse().ok()?;
        let size = match unit {
            'h' => 3600,
            'd' => day,
            _ => 7 * day,
        };
        return Some(now - count.checked_mul(size)?);
    }
    let mut parts = value.splitn(3, '-').map(str::parse::<i64>);
    let (Some(Ok(year)), Some(Ok(month)), Some(Ok(date))) = (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    ((1..=12).contains(&month) && (1..=31).contains(&date))
        .then(|| day_number(year, month, date) * day)
}

#[derive(Default)]
pub struct Filter {
    conditions: Vec<String>,
    pub params: Vec<Value>,
}

impl Filter {
    /// `{}` in `sql` expands to one placeholder per value.
    pub fn add(&mut self, negated: bool, sql: &str, values: Vec<Value>) {
        let sql = sql.replace("{}", &vec!["?"; values.len()].join(", "));
        self.params.extend(values);
        self.conditions.push(if negated {
            format!("NOT coalesce(({sql}), 0)")
        } else {
            format!("({sql})")
        });
    }

    pub fn condition(&self) -> String {
        if self.conditions.is_empty() {
            "1".to_owned()
        } else {
            self.conditions.join(" AND ")
        }
    }
}

/// Compiles a query for one scope: `in:` is skipped, `since:`/`before:` filter `time`,
/// loose words must each appear in one of `text`, and `key` handles the scope's own filters.
pub fn compile(
    query: &str,
    time: &str,
    text: &[&str],
    mut key: impl FnMut(&Term, &mut Filter) -> Result<bool, String>,
) -> Result<Filter, String> {
    let mut filter = Filter::default();
    for token in tokens(query) {
        let Some(term) = term(&token) else {
            let word = token.to_lowercase();
            let sql: Vec<String> = text
                .iter()
                .map(|column| format!("instr(lower({column}), ?) > 0"))
                .collect();
            filter.add(false, &sql.join(" OR "), vec![Value::from(word); text.len()]);
            continue;
        };
        if term.values.is_empty() {
            return Err(format!("{}: needs a value", term.key));
        }
        match term.key.as_str() {
            "in" => {}
            "since" | "before" => {
                let at = when(&term.values[0]).ok_or(TIME_HELP)?;
                let op = if term.key == "since" { ">=" } else { "<" };
                filter.add(term.negated, &format!("{time} {op} ?"), vec![Value::from(at)]);
            }
            _ => {
                if !key(&term, &mut filter)? {
                    return Err(format!("no {}: filter here", term.key));
                }
            }
        }
    }
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moments_parse_relative_and_absolute() {
        assert_eq!(when("2026-11-13"), Some(1_794_528_000));
        assert_eq!(when("1970-01-01"), Some(0));
        let week = when("1w").unwrap();
        assert!((now() - week - 7 * 86_400).abs() <= 1);
        assert!(when("today").unwrap() % 86_400 == 0);
        for bad in ["", "7x", "d", "2026-13-01", "soon"] {
            assert_eq!(when(bad), None, "{bad}");
        }
    }

    #[test]
    fn queries_compile_to_conditions() {
        let compiled = compile("crash in:bugs -level:fatal,error since:2d", "t", &["a", "b"], |term, filter| {
            Ok(term.key == "level" && {
                filter.add(term.negated, "level IN ({})", term.params());
                true
            })
        })
        .unwrap();
        assert_eq!(
            compiled.condition(),
            "(instr(lower(a), ?) > 0 OR instr(lower(b), ?) > 0) AND NOT coalesce((level IN (?, ?)), 0) AND (t >= ?)"
        );
        assert_eq!(compiled.params.len(), 5);
        assert!(compile("color:red", "t", &["a"], |_, _| Ok(false)).is_err());
        assert!(compile("since:soon", "t", &["a"], |_, _| Ok(false)).is_err());
        assert_eq!(join(&tokens(r#"label:"needs review" x"#)), r#""label:needs review" x"#);
    }
}
