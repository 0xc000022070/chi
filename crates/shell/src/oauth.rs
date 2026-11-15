use axum::{
    Extension, Form, Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::html;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Ctx, Failure, Shell, client_ip, hold, now, secret, sha256_hex, sso, store, take,
    ui::hub_page,
    url_encode,
    web::{EXTERNAL_SESSION_TTL, SESSION_TTL, Visit, local_path},
};

const MAX_REDIRECT_URIS: usize = 5;

pub struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    user_id: i64,
    provider_id: Option<i64>,
}

pub fn routes() -> Router<Shell> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route("/register", post(register))
        .route("/authorize", get(authorize_page).post(authorize))
        .route("/token", post(token))
}

async fn server_metadata(State(shell): State<Shell>) -> Json<Value> {
    Json(json!({
        "issuer": shell.url(""),
        "authorization_endpoint": shell.url("/authorize"),
        "token_endpoint": shell.url("/token"),
        "registration_endpoint": shell.url("/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

async fn resource_metadata(State(shell): State<Shell>) -> Json<Value> {
    Json(json!({
        "resource": shell.url("/mcp"),
        "authorization_servers": [shell.url("")],
        "bearer_methods_supported": ["header"],
    }))
}

fn redirect_allowed(uri: &str) -> bool {
    let loopback = [
        "http://localhost:",
        "http://localhost/",
        "http://127.0.0.1:",
        "http://127.0.0.1/",
    ];
    uri.len() <= 500
        && !uri.contains('#')
        && !uri.chars().any(char::is_whitespace)
        && (uri.starts_with("https://") || loopback.iter().any(|prefix| uri.starts_with(prefix)))
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: String,
}

async fn register(
    State(shell): State<Shell>,
    headers: HeaderMap,
    Json(form): Json<Registration>,
) -> Response {
    if !shell
        .logins
        .allow(&format!("register {}", client_ip(&headers)))
    {
        return Failure::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many registrations, wait a few minutes",
        )
        .into_response();
    }
    let uris = form.redirect_uris;
    if uris.is_empty()
        || uris.len() > MAX_REDIRECT_URIS
        || !uris.iter().all(|uri| redirect_allowed(uri))
    {
        return oauth_error("invalid_redirect_uri");
    }
    let name: String = match form.client_name.trim() {
        "" => "an unnamed app".to_owned(),
        name => name.chars().take(100).collect(),
    };
    let id = secret();
    let (row_id, row_name, row_uris) = (id.clone(), name.clone(), uris.clone());
    if let Err(failure) = shell
        .db
        .call(move |conn| store::create_client(conn, &row_id, &row_name, &row_uris))
        .await
    {
        return failure.into_response();
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": id,
            "client_name": name,
            "redirect_uris": uris,
            "client_id_issued_at": now(),
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct AuthRequest {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    state: String,
}

impl AuthRequest {
    /// The client's name, once the client is known, the redirect uri is one it registered, and PKCE is S256.
    async fn checked(&self, shell: &Shell) -> Result<String, Failure> {
        let id = self.client_id.clone();
        let (name, uris) = shell
            .db
            .call(move |conn| store::client(conn, &id))
            .await?
            .ok_or_else(|| Failure::bad_request("unknown client, connect it again"))?;
        if !uris.contains(&self.redirect_uri) {
            return Err(Failure::bad_request(
                "redirect_uri is not registered for this client",
            ));
        }
        if self.response_type != "code"
            || self.code_challenge_method != "S256"
            || self.code_challenge.is_empty()
        {
            return Err(Failure::bad_request(
                "only the code flow with S256 PKCE is supported",
            ));
        }
        Ok(name)
    }

    fn back(&self, outcome: &str) -> Redirect {
        let join = if self.redirect_uri.contains('?') {
            '&'
        } else {
            '?'
        };
        Redirect::to(&format!(
            "{}{join}{outcome}&state={}",
            self.redirect_uri,
            url_encode(&self.state)
        ))
    }
}

fn host(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

async fn authorize_page(
    State(shell): State<Shell>,
    Extension(visit): Extension<Visit>,
    uri: Uri,
    Query(request): Query<AuthRequest>,
) -> Result<Response, Failure> {
    let name = request.checked(&shell).await?;
    let Some(user) = visit.user else {
        let here = uri.path_and_query().map_or("/", |pq| pq.as_str());
        return Ok(
            Redirect::to(&format!("/login?next={}", url_encode(local_path(here)))).into_response(),
        );
    };
    let page = hub_page(
        "connect",
        true,
        html! {
            h1 { "Connect " (name) }
            p { (name) " wants to act as " b { (user.email) } " in the orgs you can reach: read issues, errors, builds and docs, and change them with your role." }
            p.dim { "You will be sent back to " (host(&request.redirect_uri)) ". Viewers stay read-only." }
            form.stack method="post" action="/authorize" {
                input type="hidden" name="response_type" value=(request.response_type);
                input type="hidden" name="client_id" value=(request.client_id);
                input type="hidden" name="redirect_uri" value=(request.redirect_uri);
                input type="hidden" name="code_challenge" value=(request.code_challenge);
                input type="hidden" name="code_challenge_method" value=(request.code_challenge_method);
                input type="hidden" name="state" value=(request.state);
                div.actions {
                    button.primary name="decision" value="allow" { "Allow" }
                    button name="decision" value="deny" { "Deny" }
                }
            }
        },
    );
    Ok((
        [
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        ],
        page,
    )
        .into_response())
}

#[derive(Deserialize)]
struct Decision {
    #[serde(flatten)]
    request: AuthRequest,
    #[serde(default)]
    decision: String,
}

async fn authorize(
    State(shell): State<Shell>,
    Extension(visit): Extension<Visit>,
    Form(Decision { request, decision }): Form<Decision>,
) -> Result<Redirect, Failure> {
    request.checked(&shell).await?;
    let Some(user) = visit.user else {
        return Err(Failure::new(StatusCode::UNAUTHORIZED, "sign in first"));
    };
    if decision != "allow" {
        return Ok(request.back("error=access_denied"));
    }
    let code = hold(
        &shell.codes,
        Code {
            client_id: request.client_id.clone(),
            redirect_uri: request.redirect_uri.clone(),
            challenge: request.code_challenge.clone(),
            user_id: user.user_id,
            provider_id: user.provider.map(|(provider_id, ..)| provider_id),
        },
    );
    Ok(request.back(&format!("code={code}")))
}

#[derive(Deserialize)]
struct TokenRequest {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
}

fn oauth_error(error: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error })),
    )
        .into_response()
}

async fn token(State(shell): State<Shell>, Form(form): Form<TokenRequest>) -> Response {
    if form.grant_type != "authorization_code" {
        return oauth_error("unsupported_grant_type");
    }
    let Some(code) = take(&shell.codes, &form.code).filter(|code| {
        code.client_id == form.client_id
            && code.redirect_uri == form.redirect_uri
            && sso::challenge(&form.code_verifier) == code.challenge
    }) else {
        return oauth_error("invalid_grant");
    };
    let access = secret();
    let hash = sha256_hex(access.as_bytes());
    let ttl = if code.provider_id.is_some() {
        EXTERNAL_SESSION_TTL
    } else {
        SESSION_TTL
    };
    let granted = shell
        .db
        .call(move |conn| {
            store::create_grant(
                conn,
                &hash,
                code.user_id,
                code.provider_id,
                &code.client_id,
                now() + ttl,
            )
        })
        .await;
    if let Err(failure) = granted {
        return failure.into_response();
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "access_token": access, "token_type": "Bearer", "expires_in": ttl })),
    )
        .into_response()
}

pub async fn bearer(State(shell): State<Shell>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| sha256_hex(token.trim().as_bytes()));
    let found = match token {
        Some(hash) => {
            shell
                .db
                .call(move |conn| {
                    let Some(user) = store::grant(conn, &hash, now())? else {
                        return Ok(None);
                    };
                    Ok(Some((store::orgs_of(conn, &user)?, user)))
                })
                .await
        }
        None => Ok(None),
    };
    let (orgs, user) = match found {
        Ok(Some(found)) => found,
        Ok(None) => {
            let challenge = format!(
                "Bearer resource_metadata=\"{}\"",
                shell.url("/.well-known/oauth-protected-resource/mcp")
            );
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, challenge)],
                "sign in required",
            )
                .into_response();
        }
        Err(failure) => return failure.into_response(),
    };
    let reach: Vec<Ctx> = orgs
        .into_iter()
        .map(|(org, role)| Ctx {
            org_id: org.id,
            org_name: org.name,
            slug: org.slug,
            user_id: user.user_id,
            email: user.email.clone(),
            role,
        })
        .collect();
    req.extensions_mut().insert(reach);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_are_https_or_loopback_only() {
        assert!(redirect_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_allowed("http://localhost:53682/callback"));
        assert!(redirect_allowed("http://127.0.0.1:9000/cb"));
        assert!(!redirect_allowed("http://evil.test/cb"));
        assert!(!redirect_allowed("http://localhost.evil.test/cb"));
        assert!(!redirect_allowed("https://a.test/cb\nhttps://b.test"));
        assert!(!redirect_allowed("https://a.test/cb#frag"));
        assert!(!redirect_allowed("javascript:alert(1)"));
    }

    #[test]
    fn host_is_shown_without_path_or_query() {
        assert_eq!(host("https://claude.ai/api/mcp/auth_callback"), "claude.ai");
        assert_eq!(host("http://localhost:8080?x=1"), "localhost:8080");
    }
}
