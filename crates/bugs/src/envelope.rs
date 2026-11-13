use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct Envelope {
    pub header: Value,
    pub events: Vec<Value>,
}

fn split_line(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|b| *b == b'\n') {
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (bytes, &[]),
    }
}

pub fn parse(body: &[u8], max_events: usize) -> Option<Envelope> {
    let (header, mut rest) = split_line(body);
    let header = serde_json::from_slice(header).ok()?;
    let mut events = Vec::new();
    while !rest.is_empty() {
        let (item_header, after) = split_line(rest);
        if item_header.is_empty() {
            rest = after;
            continue;
        }
        let item: Value = serde_json::from_slice(item_header).ok()?;
        let (payload, after) = match item["length"].as_u64() {
            Some(length) => {
                let (payload, after) = after.split_at_checked(length as usize)?;
                (payload, after.strip_prefix(b"\n").unwrap_or(after))
            }
            None => split_line(after),
        };
        if item["type"] == "event"
            && events.len() < max_events
            && let Ok(event) = serde_json::from_slice(payload)
        {
            events.push(event);
        }
        rest = after;
    }
    Some(Envelope { header, events })
}

pub fn exception(event: &Value) -> Option<&Value> {
    let values = &event["exception"]["values"];
    values.as_array().or(event["exception"].as_array())?.last()
}

pub fn frames(event: &Value) -> &[Value] {
    exception(event)
        .and_then(|e| e["stacktrace"]["frames"].as_array())
        .map_or(&[], Vec::as_slice)
}

pub struct Summary {
    pub fingerprint: String,
    pub title: String,
    pub culprit: String,
    pub level: String,
}

fn message(event: &Value) -> &str {
    event["logentry"]["formatted"]
        .as_str()
        .or(event["logentry"]["message"].as_str())
        .or(event["message"]["formatted"].as_str())
        .or(event["message"].as_str())
        .unwrap_or("")
}

fn level(event: &Value) -> &'static str {
    match event["level"].as_str() {
        Some("fatal") => "fatal",
        Some("warning") => "warning",
        Some("info") => "info",
        Some("debug") => "debug",
        _ => "error",
    }
}

pub fn summarize(event: &Value) -> Summary {
    let thrown = exception(event);
    let kind = thrown.and_then(|e| e["type"].as_str()).unwrap_or("");
    let value = thrown.and_then(|e| e["value"].as_str()).unwrap_or_else(|| message(event));
    let frames = frames(event);
    let in_app: Vec<&Value> = frames.iter().filter(|f| f["in_app"] == true).collect();
    let grouping: Vec<&Value> = if in_app.is_empty() { frames.iter().collect() } else { in_app };

    let custom: Vec<&str> = event["fingerprint"]
        .as_array()
        .map(|parts| parts.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    if custom.is_empty() || custom.contains(&"{{ default }}") {
        hasher.update(kind);
        for frame in &grouping {
            hasher.update(frame["filename"].as_str().unwrap_or(""));
            hasher.update(frame["function"].as_str().unwrap_or(""));
        }
        if grouping.is_empty() {
            hasher.update(value);
        }
    }
    for part in custom.iter().filter(|p| **p != "{{ default }}") {
        hasher.update(part);
        hasher.update([0]);
    }

    let title = match (kind, value) {
        ("", "") => "<no message>".to_owned(),
        ("", value) => value.to_owned(),
        (kind, "") => kind.to_owned(),
        (kind, value) => format!("{kind}: {value}"),
    };
    let culprit = grouping
        .last()
        .map(|f| {
            format!(
                "{} ({})",
                f["function"].as_str().unwrap_or("?"),
                f["filename"].as_str().unwrap_or("?")
            )
        })
        .or(event["transaction"].as_str().map(str::to_owned))
        .unwrap_or_default();

    Summary {
        fingerprint: shell::hex(&hasher.finalize()[..16]),
        title: title.chars().take(200).collect(),
        culprit: culprit.chars().take(200).collect(),
        level: level(event).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_items_with_and_without_length() {
        let event = r#"{"message":"a\nb"}"#;
        let body = format!(
            "{{\"event_id\":\"abc\"}}\n{{\"type\":\"session\"}}\n{{}}\n{{\"type\":\"event\",\"length\":{}}}\n{event}\n{{\"type\":\"event\"}}\n{{\"level\":\"info\"}}\n",
            event.len()
        );
        let envelope = parse(body.as_bytes(), 8).unwrap();
        assert_eq!(envelope.header["event_id"], "abc");
        assert_eq!(envelope.events.len(), 2);
        assert_eq!(envelope.events[0]["message"], "a\nb");
        assert_eq!(envelope.events[1]["level"], "info");
        assert!(parse(b"{}\n{\"type\":\"event\",\"length\":99}\nshort", 8).is_none());
        assert_eq!(parse(body.as_bytes(), 1).unwrap().events.len(), 1);
    }

    fn thrown(line: u64, function: &str) -> Value {
        json!({"exception": {"values": [{"type": "TypeError", "value": "boom", "stacktrace": {"frames": [
            {"filename": "node_modules/x.js", "function": "run", "in_app": false},
            {"filename": "/workspace/a.ts", "function": function, "lineno": line, "in_app": true},
        ]}}]}})
    }

    #[test]
    fn groups_by_in_app_frames_not_line_numbers() {
        let base = summarize(&thrown(10, "load"));
        assert_eq!(base.title, "TypeError: boom");
        assert_eq!(base.culprit, "load (/workspace/a.ts)");
        assert_eq!(base.fingerprint, summarize(&thrown(42, "load")).fingerprint);
        assert_ne!(base.fingerprint, summarize(&thrown(10, "save")).fingerprint);

        let mut custom = thrown(10, "save");
        custom["fingerprint"] = json!(["auth", "deleteAccount"]);
        let mut same_custom = thrown(99, "load");
        same_custom["fingerprint"] = json!(["auth", "deleteAccount"]);
        assert_eq!(summarize(&custom).fingerprint, summarize(&same_custom).fingerprint);
    }
}
