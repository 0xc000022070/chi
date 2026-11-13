use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Endpoints {
    pub authorization: String,
    pub token: String,
    pub userinfo: String,
    pub registration: String,
}

fn text(doc: &Value, key: &str) -> Result<String, String> {
    doc[key].as_str().map(str::to_owned).ok_or_else(|| format!("provider response lacks {key}"))
}

fn fail(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body: String = response.into_string().unwrap_or_default().chars().take(300).collect();
            format!("provider answered {code}: {body}")
        }
        other => format!("provider unreachable: {other}"),
    }
}

fn read(response: ureq::Response) -> Result<Value, String> {
    response.into_json().map_err(|e| format!("provider sent invalid json: {e}"))
}

pub fn discover(issuer: &str) -> Result<Endpoints, String> {
    let issuer = issuer.trim_end_matches('/');
    let doc = read(
        ureq::get(&format!("{issuer}/.well-known/oauth-authorization-server"))
            .timeout(TIMEOUT)
            .call()
            .map_err(fail)?,
    )?;
    let endpoints = Endpoints {
        authorization: text(&doc, "authorization_endpoint")?,
        token: text(&doc, "token_endpoint")?,
        userinfo: text(&doc, "userinfo_endpoint")?,
        registration: text(&doc, "registration_endpoint")?,
    };
    for url in [&endpoints.token, &endpoints.userinfo, &endpoints.registration] {
        if !url.starts_with(issuer) {
            return Err(format!("provider endpoint {url} is outside its issuer"));
        }
    }
    Ok(endpoints)
}

pub fn register(endpoints: &Endpoints, redirect_uri: &str) -> Result<String, String> {
    let doc = read(
        ureq::post(&endpoints.registration)
            .timeout(TIMEOUT)
            .send_json(json!({
                "client_name": "Chi",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            }))
            .map_err(fail)?,
    )?;
    text(&doc, "client_id")
}

pub fn identify(
    endpoints: &Endpoints,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<(String, String), String> {
    let token = read(
        ureq::post(&endpoints.token)
            .timeout(TIMEOUT)
            .send_form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ])
            .map_err(fail)?,
    )?;
    let access = text(&token, "access_token")?;
    let info = read(
        ureq::get(&endpoints.userinfo)
            .timeout(TIMEOUT)
            .set("authorization", &format!("Bearer {access}"))
            .call()
            .map_err(fail)?,
    )?;
    Ok((text(&info, "sub")?, text(&info, "email")?.trim().to_lowercase()))
}

pub fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| n << 8 | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc_4648_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
