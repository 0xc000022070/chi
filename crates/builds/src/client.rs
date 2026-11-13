use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use serde_json::{Map, Value, json};

use crate::file_sha256;

const CHUNK: usize = 32 * 1024 * 1024;
const USAGE: &str = "usage: chi push --url URL --version V [--commit SHA] [--branch B] [--tag prerelease|release] [--builder NAME] [--failed] FILE...
the upload token is read from CHI_TOKEN";

fn explain(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => format!("server answered {code}: {}", response.into_string().unwrap_or_default()),
        other => other.to_string(),
    }
}

fn upload(base: &str, auth: &str, build: i64, path: &Path, name: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    while offset < size {
        file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut filled = 0;
        while filled < CHUNK {
            let read = file.read(&mut buffer[filled..]).map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        let sent = ureq::put(&format!("{base}/api/builds/{build}/artifacts/{name}?offset={offset}"))
            .set("authorization", auth)
            .send_bytes(&buffer[..filled]);
        let answer: Value = match sent {
            Ok(response) => response.into_json().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(409, response)) => response.into_json().map_err(|e| e.to_string())?,
            Err(error) => return Err(explain(error)),
        };
        offset = answer["received"].as_u64().ok_or("server did not report the size received")?;
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
    let mut request = Request { base: String::new(), fields: Map::new(), files: Vec::new() };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--failed" => drop(request.fields.insert("failed".to_owned(), json!(true))),
            "--url" => request.base = args.next().ok_or(USAGE)?.trim_end_matches('/').to_owned(),
            "--version" | "--commit" | "--branch" | "--tag" | "--builder" => {
                request.fields.insert(arg[2..].to_owned(), json!(args.next().ok_or(USAGE)?));
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
    file.file_name().and_then(|name| name.to_str()).ok_or_else(|| format!("{}: file name is not valid utf-8", file.display()))
}

fn post(url: &str, auth: &str, body: Value) -> Result<Value, String> {
    ureq::post(url).set("authorization", auth).send_json(body).map_err(explain)?.into_json().map_err(|e| e.to_string())
}

pub fn push(args: &[String]) -> Result<(), String> {
    let Request { base, fields, files } = parse(args)?;
    let auth = format!("Bearer {}", std::env::var("CHI_TOKEN").map_err(|_| USAGE)?);

    let mut digests = Map::new();
    for file in &files {
        let digest = file_sha256(file).map_err(|e| format!("{}: {e}", file.display()))?;
        digests.insert(file_name(file)?.to_owned(), json!(digest));
    }

    let failed = fields.contains_key("failed");
    let created = post(&format!("{base}/api/builds"), &auth, Value::Object(fields))?;
    let build = created["id"].as_i64().ok_or("server did not return a build id")?;
    if failed {
        println!("reported failed build {build}");
        return Ok(());
    }
    for file in &files {
        upload(&base, &auth, build, file, file_name(file)?)?;
    }
    let finished = post(&format!("{base}/api/builds/{build}/finish"), &auth, json!({ "artifacts": digests }))?;
    println!("{}", finished["url"].as_str().unwrap_or_default());
    Ok(())
}
