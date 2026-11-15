use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::Command,
};

use serde_json::{Map, Value, json};

use crate::file_sha256;

const CHUNK: usize = 32 * 1024 * 1024;
pub(crate) const MAX_DIFF: usize = 300_000;
const MAX_COMMITS: &str = "200";
const USAGE: &str = "usage: chi push --url URL --version V [--commit SHA] [--branch B] [--tag prerelease|release] [--builder NAME] [--failed] FILE...
the upload token is read from CHI_TOKEN. With --commit inside a git checkout, the commits, diffstat and diff
since the previous build of the same tag go along, for the repo's pipeline to read";

fn explain(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => format!(
            "server answered {code}: {}",
            response.into_string().unwrap_or_default()
        ),
        other => other.to_string(),
    }
}

fn upload(base: &str, auth: &str, build: i64, path: &Path, name: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    while offset < size {
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut filled = 0;
        while filled < CHUNK {
            let read = file
                .read(&mut buffer[filled..])
                .map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        let sent = ureq::put(&format!(
            "{base}/api/builds/{build}/artifacts/{name}?offset={offset}"
        ))
        .set("authorization", auth)
        .send_bytes(&buffer[..filled]);
        let answer: Value = match sent {
            Ok(response) => response.into_json().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(409, response)) => {
                response.into_json().map_err(|e| e.to_string())?
            }
            Err(error) => return Err(explain(error)),
        };
        offset = answer["received"]
            .as_u64()
            .ok_or("server did not report the size received")?;
        eprintln!("{name}: {offset}/{size} bytes");
    }
    Ok(())
}

struct Request<'a> {
    base: String,
    fields: Map<String, Value>,
    files: Vec<&'a Path>,
}

fn parse(args: &[String]) -> Result<Request<'_>, String> {
    let mut request = Request {
        base: String::new(),
        fields: Map::new(),
        files: Vec::new(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--failed" => drop(request.fields.insert("failed".to_owned(), json!(true))),
            "--url" => request.base = args.next().ok_or(USAGE)?.trim_end_matches('/').to_owned(),
            "--version" | "--commit" | "--branch" | "--tag" | "--builder" => {
                request
                    .fields
                    .insert(arg[2..].to_owned(), json!(args.next().ok_or(USAGE)?));
            }
            flag if flag.starts_with("--") => return Err(USAGE.to_owned()),
            file => request.files.push(Path::new(file)),
        }
    }
    if request.base.is_empty() || !request.fields.contains_key("version") {
        return Err(USAGE.to_owned());
    }
    Ok(request)
}

fn file_name(file: &Path) -> Result<&str, String> {
    file.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{}: file name is not valid utf-8", file.display()))
}

fn post(url: &str, auth: &str, body: Value) -> Result<Value, String> {
    ureq::post(url)
        .set("authorization", auth)
        .send_json(body)
        .map_err(explain)?
        .into_json()
        .map_err(|e| e.to_string())
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_log(log: &str) -> Vec<Value> {
    log.split('\x1e')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').splitn(5, '\x1f');
            let sha = fields.next().filter(|sha| !sha.is_empty())?;
            Some(json!({
                "sha": sha,
                "author": fields.next()?,
                "date": fields.next()?,
                "subject": fields.next()?,
                "body": fields.next().unwrap_or("").trim(),
            }))
        })
        .collect()
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[truncated]\n");
    }
    text
}

fn context(base_url: &str, auth: &str, fields: &mut Map<String, Value>) {
    let Some(commit) = fields
        .get("commit")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    if git(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")]).is_none() {
        return;
    }
    let tag = fields
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or("prerelease");
    let base = ureq::get(&format!("{base_url}/api/builds/base?tag={tag}"))
        .set("authorization", auth)
        .call()
        .ok()
        .and_then(|response| response.into_json::<Value>().ok())
        .and_then(|answer| answer["commit"].as_str().map(str::to_owned))
        .filter(|base| {
            !base.is_empty()
                && *base != commit
                && git(&["cat-file", "-e", &format!("{base}^{{commit}}")]).is_some()
        });
    let format = "--format=%H%x1f%an%x1f%aI%x1f%s%x1f%b%x1e";
    let (log, diffstat, diff) = match &base {
        Some(base) => {
            let range = format!("{base}..{commit}");
            (
                git(&["log", "-n", MAX_COMMITS, format, &range]),
                git(&["diff", "--stat", base, &commit]),
                git(&["diff", base, &commit]),
            )
        }
        None => (git(&["log", "-n", "20", format, &commit]), None, None),
    };
    fields.insert("base".to_owned(), json!(base.unwrap_or_default()));
    fields.insert(
        "commits".to_owned(),
        json!(parse_log(&log.unwrap_or_default())),
    );
    fields.insert(
        "diffstat".to_owned(),
        json!(truncate(diffstat.unwrap_or_default(), MAX_DIFF / 3)),
    );
    fields.insert(
        "diff".to_owned(),
        json!(truncate(diff.unwrap_or_default(), MAX_DIFF)),
    );
}

pub fn push(args: &[String]) -> Result<(), String> {
    let Request {
        base,
        mut fields,
        files,
    } = parse(args)?;
    let auth = format!("Bearer {}", std::env::var("CHI_TOKEN").map_err(|_| USAGE)?);
    context(&base, &auth, &mut fields);

    let mut digests = Map::new();
    for file in &files {
        let digest = file_sha256(file).map_err(|e| format!("{}: {e}", file.display()))?;
        digests.insert(file_name(file)?.to_owned(), json!(digest));
    }

    let failed = fields.contains_key("failed");
    let created = post(&format!("{base}/api/builds"), &auth, Value::Object(fields))?;
    let build = created["id"]
        .as_i64()
        .ok_or("server did not return a build id")?;
    if failed {
        println!("reported failed build {build}");
        return Ok(());
    }
    for file in &files {
        upload(&base, &auth, build, file, file_name(file)?)?;
    }
    let finished = post(
        &format!("{base}/api/builds/{build}/finish"),
        &auth,
        json!({ "artifacts": digests }),
    )?;
    println!("{}", finished["url"].as_str().unwrap_or_default());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_log_splits_into_commits() {
        let log = "a1\x1fAna\x1f2026-01-01T00:00:00Z\x1ffix ABC-1\x1fFixes ABC-1\n\x1e\nb2\x1fBo\x1f2026-01-02T00:00:00Z\x1fadd x\x1f\x1e\n";
        let commits = parse_log(log);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["body"], "Fixes ABC-1");
        assert_eq!(commits[1]["sha"], "b2");
    }

    #[test]
    fn truncation_keeps_whole_characters() {
        assert_eq!(truncate("ñandú".to_owned(), 2), "ñ\n[truncated]\n");
        assert_eq!(truncate("short".to_owned(), 10), "short");
    }
}
