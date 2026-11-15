use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shell::{Failure, identifiers, now};

use crate::{Changes, OPEN, Tracker, store};

const CLOSING: [&str; 9] = [
    "FIX", "FIXES", "FIXED", "CLOSE", "CLOSES", "CLOSED", "RESOLVE", "RESOLVES", "RESOLVED",
];

pub fn routes() -> Router<Tracker> {
    Router::new().route("/api/github", post(webhook))
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner = Sha256::new()
        .chain_update(block.map(|b| b ^ 0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(block.map(|b| b ^ 0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn signed(secret: &str, body: &[u8], signature: &str) -> bool {
    let expected = format!("sha256={}", shell::hex(&hmac(secret.as_bytes(), body)));
    expected.len() == signature.len()
        && expected
            .bytes()
            .zip(signature.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn mentions(text: &str) -> Vec<(String, bool)> {
    let upper = text.to_ascii_uppercase();
    let mut found: Vec<(String, bool)> = Vec::new();
    for (start, end) in identifiers(&upper) {
        let word = upper[..start]
            .trim_end_matches([' ', ':', '\t'])
            .rsplit(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        let (key, closes) = (upper[start..end].to_owned(), CLOSING.contains(&word));
        match found.iter_mut().find(|(seen, _)| *seen == key) {
            Some(seen) => seen.1 |= closes,
            None => found.push((key, closes)),
        }
    }
    found
}

async fn webhook(
    State(tracker): State<Tracker>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Failure> {
    let secret = tracker
        .github_secret
        .clone()
        .ok_or_else(Failure::not_found)?;
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    };
    if !signed(&secret, &body, header("x-hub-signature-256")) {
        return Err(Failure::new(StatusCode::UNAUTHORIZED, "bad signature"));
    }
    let event: Value =
        serde_json::from_slice(&body).map_err(|e| Failure::bad_request(&e.to_string()))?;
    let url = event["repository"]["html_url"].as_str().unwrap_or("");
    let orgs: Vec<i64> = tracker
        .shell
        .repos_by_url(url)
        .await?
        .into_iter()
        .map(|repo| repo.org_id)
        .collect();
    let linked = match header("x-github-event") {
        "pull_request" => pull_request(&tracker, &orgs, &event, url).await?,
        "push" => push(&tracker, &orgs, &event, url).await?,
        // ignored: other events link nothing
        _ => 0,
    };
    Ok(Json(json!({ "linked": linked })))
}

async fn pull_request(
    tracker: &Tracker,
    orgs: &[i64],
    event: &Value,
    url: &str,
) -> Result<usize, Failure> {
    let pr = &event["pull_request"];
    let (action, merged, draft) = (
        event["action"].as_str().unwrap_or(""),
        pr["merged"] == true,
        pr["draft"] == true,
    );
    let state = match (pr["state"].as_str(), merged, draft) {
        (_, true, _) => "merged",
        (Some("closed"), _, _) => "closed",
        (_, _, true) => "draft",
        _ => "open",
    };
    let text = format!(
        "{}\n{}\n{}",
        pr["title"].as_str().unwrap_or(""),
        pr["body"].as_str().unwrap_or(""),
        pr["head"]["ref"].as_str().unwrap_or("")
    );
    let title = format!("#{} {}", pr["number"], pr["title"].as_str().unwrap_or(""));
    let target = match (action, merged, draft) {
        ("closed", true, _) => Some("done"),
        ("opened" | "reopened" | "ready_for_review", false, false) => Some("in_review"),
        _ => None,
    };
    let branch = mentions(pr["head"]["ref"].as_str().unwrap_or(""));
    let mut linked = 0;
    for org_id in orgs {
        for (key, closes) in mentions(&text) {
            let moves = closes || branch.iter().any(|(named, _)| *named == key);
            linked += tracker
                .link(
                    *org_id,
                    &key,
                    "pull request",
                    pr["html_url"].as_str().unwrap_or(url),
                    &title,
                    state,
                    target.filter(|_| moves),
                )
                .await?;
        }
    }
    Ok(linked)
}

async fn push(tracker: &Tracker, orgs: &[i64], event: &Value, url: &str) -> Result<usize, Failure> {
    let branch = event["ref"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("refs/heads/");
    let default = branch
        == event["repository"]["default_branch"]
            .as_str()
            .unwrap_or("main");
    let mut linked = 0;
    for commit in event["commits"].as_array().into_iter().flatten() {
        let message = commit["message"].as_str().unwrap_or("");
        let title: String = message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        let short: String = commit["id"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(7)
            .collect();
        for org_id in orgs {
            for (key, closes) in mentions(message) {
                let target = (closes && default).then_some("done");
                linked += tracker
                    .link(
                        *org_id,
                        &key,
                        "commit",
                        commit["url"].as_str().unwrap_or(url),
                        &format!("{short} {title}"),
                        branch,
                        target,
                    )
                    .await?;
            }
        }
    }
    Ok(linked)
}

impl Tracker {
    #[expect(
        clippy::too_many_arguments,
        reason = "the link's facts arrive as separate webhook fields"
    )]
    async fn link(
        &self,
        org_id: i64,
        key: &str,
        kind: &str,
        url: &str,
        title: &str,
        state: &str,
        target: Option<&str>,
    ) -> Result<usize, Failure> {
        if !url.starts_with("https://") {
            return Ok(0);
        }
        let (lookup, kind_owned, url_owned, title_owned, state_owned) = (
            key.to_owned(),
            kind.to_owned(),
            url.to_owned(),
            title.to_owned(),
            state.to_owned(),
        );
        let issue = self
            .db
            .call(move |conn| {
                let Some(issue) = store::issue_by_key(conn, org_id, &lookup)? else { return Ok(None) };
                let at = now();
                let added = conn.execute(
                    "INSERT INTO links (issue_id, kind, url, title, state, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (issue_id, url) DO NOTHING",
                    (issue.id, &kind_owned, &url_owned, &title_owned, &state_owned, at),
                )?;
                if added > 0 {
                    conn.execute("INSERT INTO history (issue_id, actor, field, old, new, at) VALUES (?1, NULL, 'link', '', ?2, ?3)", (issue.id, format!("{kind_owned} {title_owned}"), at))?;
                } else {
                    conn.execute("UPDATE links SET title = ?3, state = ?4, at = ?5 WHERE issue_id = ?1 AND url = ?2", (issue.id, &url_owned, &title_owned, &state_owned, at))?;
                }
                Ok(Some(issue))
            })
            .await?;
        let Some(issue) = issue else { return Ok(0) };
        let order = |status: &str| OPEN.iter().position(|open| *open == status);
        let forward = target.is_some_and(|target| match (order(&issue.status), order(target)) {
            (Some(now), Some(next)) => next > now,
            (Some(_), None) => true,
            _ => false,
        });
        if forward {
            let changes = Changes {
                status: target.map(str::to_owned),
                ..Changes::default()
            };
            self.write(org_id, None, "github", Some(issue.id), changes, None)
                .await?;
        } else {
            self.shell.publish(org_id, "issues");
        }
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_follow_githubs_example() {
        let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(signed(
            "It's a Secret to Everybody",
            b"Hello, World!",
            signature
        ));
        assert!(!signed(
            "It's a Secret to Everybody",
            b"Hello, World?",
            signature
        ));
        assert!(!signed("other", b"Hello, World!", signature));
    }

    #[test]
    fn closing_words_only_count_right_before_the_id() {
        let found = mentions("Fixes chi-1, refs CHI-2 and closes: ops-3\nfeature/chi-4-x CHI-1");
        assert_eq!(
            found,
            [
                ("CHI-1".to_owned(), true),
                ("CHI-2".to_owned(), false),
                ("OPS-3".to_owned(), true),
                ("CHI-4".to_owned(), false)
            ]
        );
    }
}
