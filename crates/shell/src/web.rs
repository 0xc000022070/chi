use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::SaltString,
};
use axum::{
    Extension, Form, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{Markup, html};
use serde::Deserialize;
use tower::Layer;

use crate::{
    Ctx, Failure, Pending, Role, Shell, client_ip, now, secret, sha256_hex, sso,
    store::{self, Joined, Joiner, SessionUser},
    ui::{REPO_KINDS, glyph, hub_page, org_page, repo_glyph},
    url_encode,
};

const RESERVED: [&str; 7] = ["api", "login", "logout", "orgs", "invite", "sso", "callback"];

const COOKIE: &str = "chi_session";
const SESSION_TTL: i64 = 30 * 86_400;
const EXTERNAL_SESSION_TTL: i64 = 86_400;
const INVITE_TTL: i64 = 7 * 86_400;
pub(crate) const MIN_PASSWORD: usize = 10;

#[derive(Clone)]
struct Visit {
    user: Option<SessionUser>,
    token_hash: Option<String>,
}

impl Visit {
    fn native(&self) -> Option<&SessionUser> {
        self.user.as_ref().filter(|user| user.provider.is_none())
    }
}

fn local_path(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') { next } else { "/" }
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|pair| pair.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

pub(crate) fn hash_password(password: &str) -> Result<String, Failure> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("os entropy");
    let salt = SaltString::encode_b64(&salt).map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| Failure(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub(crate) fn password_matches(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

impl Shell {
    pub fn router(&self, rooms: Router, api: Router) -> Router {
        let org_side = Router::new()
            .route("/", get(org_home))
            .route("/settings", get(settings))
            .route("/settings/members", post(save_member))
            .route("/settings/invites", post(create_invite))
            .route("/settings/repos", post(create_repo))
            .route("/settings/repos/{id}", post(update_repo))
            .route("/settings/repos/{id}/token", post(rotate_repo_token))
            .route("/settings/providers", post(create_provider))
            .route("/settings/providers/{id}/delete", post(delete_provider))
            .with_state(self.clone())
            .merge(rooms);
        let pages = Router::new()
            .route("/", get(home))
            .route("/orgs", post(create_org))
            .route("/login", get(login_page).post(login))
            .route("/logout", post(logout))
            .route("/invite/{token}", get(invite_page).post(accept_invite))
            .route("/sso/{provider_id}", get(sso_start))
            .route("/callback", get(sso_callback))
            .route("/sans.woff2", get(|| async { font(include_bytes!("../fonts/sans.woff2")) }))
            .route("/mono.woff2", get(|| async { font(include_bytes!("../fonts/mono.woff2")) }))
            .with_state(self.clone())
            .fallback_service(middleware::from_fn_with_state(self.clone(), guard).layer(org_side))
            .layer(middleware::from_fn_with_state(self.clone(), visit));
        api.merge(pages)
    }

    async fn start_session(&self, user_id: i64, provider_id: Option<i64>, to: &str) -> Result<Response, Failure> {
        let token = secret();
        let hash = sha256_hex(token.as_bytes());
        let ttl = if provider_id.is_some() { EXTERNAL_SESSION_TTL } else { SESSION_TTL };
        self.db.call(move |conn| store::create_session(conn, &hash, user_id, provider_id, now() + ttl)).await?;
        let secure = if self.url.starts_with("https") { "; Secure" } else { "" };
        let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{secure}");
        Ok(([(header::SET_COOKIE, cookie)], Redirect::to(to)).into_response())
    }
}

fn font(bytes: &'static [u8]) -> Response {
    let headers = [(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=604800")];
    (headers, bytes).into_response()
}

const NO_PROXY_REWRITES: HeaderValue = HeaderValue::from_static("private, no-transform");

async fn visit(State(shell): State<Shell>, mut req: Request, next: Next) -> Response {
    let token_hash = session_token(req.headers()).map(|token| sha256_hex(token.as_bytes()));
    let user = match token_hash.clone() {
        Some(hash) => match shell.db.call(move |conn| store::session(conn, &hash, now())).await {
            Ok(user) => user,
            Err(failure) => return failure.into_response(),
        },
        None => None,
    };
    req.extensions_mut().insert(Visit { user, token_hash });
    let mut response = next.run(req).await;
    response.headers_mut().entry(header::CACHE_CONTROL).or_insert(NO_PROXY_REWRITES);
    response
}

fn split_org(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/');
    path.split_once('/').unwrap_or((path, ""))
}

/// Serves `/{org}/…`: resolves the org from the first segment and hands the rest to the org routes.
async fn guard(State(shell): State<Shell>, mut req: Request, next: Next) -> Response {
    let Some(Visit { user, .. }) = req.extensions().get::<Visit>().cloned() else {
        return Failure::not_found().into_response();
    };
    let (slug, rest) = split_org(req.uri().path());
    let inner = match req.uri().query() {
        Some(query) => format!("/{rest}?{query}"),
        None => format!("/{rest}"),
    };
    let Ok(inner) = inner.parse() else { return Failure::not_found().into_response() };
    let (slug, who) = (slug.to_owned(), user.clone());
    let found = shell
        .db
        .call(move |conn| {
            let Some(org) = store::org_by_slug(conn, &slug)? else { return Ok(None) };
            let role = match &who {
                Some(user) => store::role_in(conn, user, org.id)?,
                None => None,
            };
            Ok(Some((org, role)))
        })
        .await;
    let (org, role) = match found {
        Ok(Some(found)) => found,
        Ok(None) => return Failure::not_found().into_response(),
        Err(failure) => return failure.into_response(),
    };
    let Some(user) = user else {
        let path = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
        return Redirect::to(&format!("/login?next={}", url_encode(path))).into_response();
    };
    let Some(role) = role else { return Failure::forbidden().into_response() };
    *req.uri_mut() = inner;
    req.extensions_mut().insert(Ctx {
        org_id: org.id,
        org_name: org.name,
        slug: org.slug,
        user_id: user.user_id,
        email: user.email,
        role,
    });
    next.run(req).await
}

async fn org_home(Extension(ctx): Extension<Ctx>) -> Redirect {
    Redirect::to(&ctx.href("/bugs"))
}

async fn home(State(shell): State<Shell>, Extension(visit): Extension<Visit>) -> Result<Response, Failure> {
    let Some(user) = visit.user else { return Ok(Redirect::to("/login").into_response()) };
    let who = user.clone();
    let orgs = shell
        .db
        .call(move |conn| {
            let mut mine = Vec::new();
            for org in store::orgs(conn)? {
                if let Some(role) = store::role_in(conn, &who, org.id)? {
                    mine.push((org, role));
                }
            }
            Ok(mine)
        })
        .await?;
    let admin = user.admin && user.provider.is_none();
    Ok(hub_page(
        "orgs",
        true,
        html! {
            h1 { "Your orgs" }
            p.dim { (user.email) }
            @if orgs.is_empty() { p.dim { "You have no access to any org yet. Ask an owner for an invite." } }
            @for (org, role) in &orgs {
                a.row href={ "/" (org.slug) "/" } {
                    strong { (org.name) }
                    span { "/" (org.slug) " · " (role.as_str()) }
                }
            }
            @if admin {
                details.add {
                    summary { "New org" }
                    form.stack method="post" action="/orgs" {
                        label { "Name" input name="name" required; }
                        label { "Slug" input name="slug" required pattern="[a-z0-9-]+"; }
                        button { "create" }
                    }
                }
            }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
struct NewOrg {
    name: String,
    slug: String,
}

async fn create_org(
    State(shell): State<Shell>,
    Extension(visit): Extension<Visit>,
    Form(form): Form<NewOrg>,
) -> Result<Response, Failure> {
    if !visit.native().is_some_and(|user| user.admin) {
        return Err(Failure::forbidden());
    }
    let (name, slug) = (form.name.trim().to_owned(), form.slug.trim().to_owned());
    let slug_ok = !slug.is_empty()
        && !RESERVED.contains(&slug.as_str())
        && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if name.is_empty() || !slug_ok {
        return Err(Failure::bad_request("invalid name or slug"));
    }
    if !shell.db.call(move |conn| store::create_org(conn, &slug, &name)).await? {
        return Err(Failure::bad_request("slug already in use"));
    }
    Ok(Redirect::to("/").into_response())
}

#[derive(Deserialize, Default)]
struct Target {
    #[serde(default)]
    next: String,
}

async fn login_form(shell: &Shell, target: &Target, signed_in: bool, notice: &str) -> Result<Response, Failure> {
    let slug = split_org(&target.next).0.to_owned();
    let providers = shell
        .db
        .call(move |conn| {
            let org = store::org_by_slug(conn, &slug)?;
            Ok(store::providers(conn)?
                .into_iter()
                .filter(|provider| org.as_ref().is_none_or(|org| org.id == provider.org_id))
                .collect::<Vec<_>>())
        })
        .await?;
    let query = format!("next={}", url_encode(&target.next));
    Ok(hub_page(
        "sign in",
        signed_in,
        html! {
            h1 { "Sign in" }
            @if !notice.is_empty() { p.error { (notice) } }
            @for provider in &providers {
                a.button href=(format!("/sso/{}?{query}", provider.id)) { "Continue with " (provider.name) }
            }
            @if !providers.is_empty() { p.or { "or" } }
            form.stack method="post" action="/login" {
                input type="hidden" name="next" value=(target.next);
                label { "Email" input type="email" name="email" autocomplete="username" required; }
                label { "Password" input type="password" name="password" autocomplete="current-password" required; }
                button.primary { "Sign in" }
            }
        },
    )
    .into_response())
}

async fn login_page(
    State(shell): State<Shell>,
    Extension(visit): Extension<Visit>,
    Query(target): Query<Target>,
) -> Result<Response, Failure> {
    if visit.user.is_some() {
        return Ok(Redirect::to(local_path(&target.next)).into_response());
    }
    login_form(&shell, &target, false, "").await
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
    #[serde(flatten)]
    target: Target,
}

async fn login(
    State(shell): State<Shell>,
    headers: HeaderMap,
    Form(form): Form<Credentials>,
) -> Result<Response, Failure> {
    if !shell.logins.allow(&client_ip(&headers)) {
        return Err(Failure::new(StatusCode::TOO_MANY_REQUESTS, "too many attempts, wait a few minutes"));
    }
    let email = form.email.trim().to_lowercase();
    let known = shell.db.call(move |conn| store::password_hash(conn, &email)).await?;
    let password = form.password;
    let user_id = tokio::task::spawn_blocking(move || {
        known.and_then(|(user_id, hash)| password_matches(&password, &hash).then_some(user_id))
    })
    .await?;
    let Some(user_id) = user_id else {
        return login_form(&shell, &form.target, false, "Wrong email or password.").await;
    };
    shell.start_session(user_id, None, local_path(&form.target.next)).await
}

async fn logout(State(shell): State<Shell>, Extension(visit): Extension<Visit>) -> Result<Response, Failure> {
    if let Some(hash) = visit.token_hash {
        shell.db.call(move |conn| store::delete_session(conn, &hash)).await?;
    }
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response())
}

async fn invite_page(
    State(shell): State<Shell>,
    Extension(visit): Extension<Visit>,
    Path(token): Path<String>,
) -> Result<Response, Failure> {
    let hash = sha256_hex(token.as_bytes());
    if !shell.db.call(move |conn| store::invite_exists(conn, &hash, now())).await? {
        return Err(Failure::new(StatusCode::GONE, "this invite is used or expired"));
    }
    Ok(hub_page(
        "invite",
        visit.user.is_some(),
        html! {
            h1 { "You are invited" }
            form.stack method="post" {
                @if let Some(user) = visit.native() {
                    p { "Accept as " b { (user.email) } "?" }
                } @else {
                    label { "Email" input type="email" name="email" autocomplete="username" required; }
                    label {
                        "Password, " (MIN_PASSWORD) "+ characters"
                        input type="password" name="password" autocomplete="new-password" minlength=(MIN_PASSWORD) required;
                    }
                }
                button.primary { "Accept invite" }
            }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
struct Signup {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
}

async fn accept_invite(
    State(shell): State<Shell>,
    Extension(visit): Extension<Visit>,
    Path(token): Path<String>,
    Form(form): Form<Signup>,
) -> Result<Response, Failure> {
    let joiner = match visit.native() {
        Some(user) => Joiner::Existing(user.user_id),
        None => {
            let email = form.email.trim().to_lowercase();
            if !email.contains('@') || form.password.len() < MIN_PASSWORD {
                return Err(Failure::bad_request("a valid email and a longer password are required"));
            }
            let password = form.password;
            let password_hash = tokio::task::spawn_blocking(move || hash_password(&password)).await??;
            Joiner::New { email, password_hash }
        }
    };
    let hash = sha256_hex(token.as_bytes());
    match shell.db.call(move |conn| store::accept_invite(conn, &hash, now(), joiner)).await? {
        Joined::User(user_id) => shell.start_session(user_id, None, "/").await,
        Joined::BadInvite => Err(Failure::new(StatusCode::GONE, "this invite is used or expired")),
        Joined::EmailTaken => Err(Failure::bad_request("that email already has an account: sign in first, then open the invite again")),
    }
}

async fn sso_start(
    State(shell): State<Shell>,
    Path(provider_id): Path<i64>,
    Query(target): Query<Target>,
) -> Result<Response, Failure> {
    let provider = shell
        .db
        .call(move |conn| Ok(store::providers(conn)?.into_iter().find(|p| p.id == provider_id)))
        .await?
        .ok_or_else(Failure::not_found)?;
    let issuer = provider.issuer.clone();
    let endpoints = tokio::task::spawn_blocking(move || sso::discover(&issuer))
        .await?
        .map_err(|e| Failure(StatusCode::BAD_GATEWAY, e))?;
    let verifier = secret();
    let challenge = sso::challenge(&verifier);
    let state = shell.hold(Pending { provider_id, verifier, next: target.next });
    Ok(Redirect::to(&format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope=identity&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        endpoints.authorization,
        url_encode(&provider.client_id),
        url_encode(&shell.url("/callback")),
    ))
    .into_response())
}

#[derive(Deserialize)]
struct Callback {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: String,
}

async fn sso_callback(
    State(shell): State<Shell>,
    Query(query): Query<Callback>,
) -> Result<Response, Failure> {
    let Some(Pending { provider_id, verifier, next }) = shell.take(&query.state) else {
        return Err(Failure::bad_request("sign-in expired, start again"));
    };
    if query.code.is_empty() {
        let target = Target { next };
        return login_form(&shell, &target, false, &format!("Sign-in was not completed: {}", query.error)).await;
    }
    let provider = shell
        .db
        .call(move |conn| Ok(store::providers(conn)?.into_iter().find(|p| p.id == provider_id)))
        .await?
        .ok_or_else(Failure::not_found)?;
    let redirect_uri = shell.url("/callback");
    let (issuer, client_id, code) = (provider.issuer.clone(), provider.client_id.clone(), query.code);
    let (subject, email) = tokio::task::spawn_blocking(move || {
        let endpoints = sso::discover(&issuer)?;
        sso::identify(&endpoints, &client_id, &code, &verifier, &redirect_uri)
    })
    .await?
    .map_err(|e| Failure(StatusCode::BAD_GATEWAY, e))?;
    let org_id = provider.org_id;
    let (user_id, slug) = shell
        .db
        .call(move |conn| {
            let user_id = store::sign_in_external(conn, provider_id, org_id, &subject, &email)?;
            Ok((user_id, store::org(conn, org_id)?.map(|org| org.slug).unwrap_or_default()))
        })
        .await?;
    let to = if next.is_empty() { format!("/{slug}") } else { local_path(&next).to_owned() };
    shell.start_session(user_id, Some(provider_id), &to).await
}

fn role_options(current: &str) -> Markup {
    html! {
        @for role in ["viewer", "maintainer", "owner"] {
            option value=(role) selected[role == current] { (role) }
        }
    }
}

fn kind_options(current: &str) -> Markup {
    html! {
        option value="" { "other" }
        @for (kind, _) in REPO_KINDS {
            option value=(kind) selected[kind == current] { (kind) }
        }
    }
}

fn repos_section(repos: &[store::Repo], maintainer: bool) -> Markup {
    html! {
        h2 { "Repos" }
        p.dim { "Codebases that upload builds. Each one has its own upload token." }
        @if repos.is_empty() { p.dim { "None yet." } }
        @for repo in repos {
            @let about = if repo.url.is_empty() { repo.kind.as_str() } else { repo.url.as_str() };
            @if maintainer {
                details.row {
                    summary {
                        (glyph(repo_glyph(&repo.kind)))
                        div { strong { (repo.name) } span { (about) } }
                        span.dim { "edit" }
                    }
                    form.stack method="post" action=(format!("settings/repos/{}", repo.id)) {
                        label { "Type" select name="kind" { (kind_options(&repo.kind)) } }
                        label { "URL" input name="url" value=(repo.url) placeholder="https://github.com/org/repo"; }
                        div.actions {
                            button { "save" }
                            button formaction=(format!("settings/repos/{}/token", repo.id)) { "new upload token" }
                        }
                        p.dim { "A new upload token replaces the current one." }
                    }
                }
            } @else {
                div.row { strong { (glyph(repo_glyph(&repo.kind))) (repo.name) } span { (about) } }
            }
        }
        @if maintainer {
            details.add {
                summary { "Add a repo" }
                form.stack method="post" action="settings/repos" {
                    label { "Name" input name="name" required; }
                    label { "Type" select name="kind" { (kind_options("")) } }
                    label { "URL" input name="url" placeholder="https://github.com/org/repo"; }
                    button { "add repo" }
                }
            }
        }
    }
}

fn members_section(members: &[store::Member], owner: bool) -> Markup {
    html! {
        h2 { "Members" }
        p.dim { "Suite admins reach every org without being listed here." }
        @for member in members {
            div.row {
                strong { (member.email) }
                @if owner {
                    form.inline method="post" action="settings/members" {
                        input type="hidden" name="user_id" value=(member.user_id);
                        select name="role" aria-label="Role" { (role_options(&member.role)) option value="" { "remove" } }
                        " " button { "save" }
                    }
                } @else { span { (member.role) } }
            }
        }
        @if owner {
            details.add {
                summary { "Invite someone" }
                form.stack method="post" action="settings/invites" {
                    label { "Role" select name="role" { (role_options("viewer")) } }
                    button { "create invite link" }
                }
            }
        }
    }
}

fn providers_section(providers: &[store::Provider]) -> Markup {
    html! {
        h2 { "Sign-in providers" }
        p.dim { "Other services whose accounts can sign in to this org." }
        @for provider in providers {
            div.row {
                strong { (provider.name) }
                span { (provider.issuer) } " "
                form.inline method="post" action=(format!("settings/providers/{}/delete", provider.id)) {
                    button { "remove" }
                }
            }
        }
        details.add {
            summary { "Add a provider" }
            form.stack method="post" action="settings/providers" {
                label { "Name shown on the sign-in page" input name="name" required; }
                label { "Issuer URL" input name="issuer" placeholder="https://…" required; }
                label {
                    "Roles"
                    select name="trusted" {
                        option value="0" { "its users are always viewers" }
                        option value="1" { "its users keep the role they are given here" }
                    }
                }
                button { "add provider" }
            }
        }
    }
}

async fn settings(State(shell): State<Shell>, Extension(ctx): Extension<Ctx>) -> Result<Response, Failure> {
    let org_id = ctx.org_id;
    let (members, repos, providers) = shell
        .db
        .call(move |conn| {
            let providers: Vec<_> = store::providers(conn)?.into_iter().filter(|p| p.org_id == org_id).collect();
            Ok((store::members(conn, org_id)?, store::repos(conn, org_id)?, providers))
        })
        .await?;
    let owner = ctx.role >= Role::Owner;
    Ok(org_page(
        &ctx,
        "settings",
        html! {
            h1 { "Settings" }
            p.dim { (ctx.email) " · " (ctx.role.as_str()) }
            (repos_section(&repos, ctx.role >= Role::Maintainer))
            (members_section(&members, owner))
            @if owner { (providers_section(&providers)) }
        },
    )
    .into_response())
}

#[derive(Deserialize)]
struct MemberForm {
    user_id: i64,
    role: String,
}

async fn save_member(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<MemberForm>,
) -> Result<Response, Failure> {
    ctx.require(Role::Owner)?;
    let role = match form.role.as_str() {
        "" => None,
        other => Some(Role::parse(other).ok_or_else(|| Failure::bad_request("unknown role"))?),
    };
    let org_id = ctx.org_id;
    shell.db.call(move |conn| store::set_role(conn, org_id, form.user_id, role)).await?;
    Ok(Redirect::to(&ctx.href("/settings")).into_response())
}

fn once(ctx: &Ctx, title: &str, label: &str, value: &str) -> Response {
    org_page(
        ctx,
        title,
        html! {
            h1 { (title) }
            p { (label) }
            pre { (value) }
            p.dim { "Shown once. Copy it now." }
            a href="settings" { "back to settings" }
        },
    )
    .into_response()
}

#[derive(Deserialize)]
struct InviteForm {
    role: String,
}

async fn create_invite(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<InviteForm>,
) -> Result<Response, Failure> {
    ctx.require(Role::Owner)?;
    let role = Role::parse(&form.role).ok_or_else(|| Failure::bad_request("unknown role"))?;
    let token = secret();
    let (hash, org_id) = (sha256_hex(token.as_bytes()), ctx.org_id);
    shell
        .db
        .call(move |conn| store::create_invite(conn, &hash, Some(org_id), role, now() + INVITE_TTL))
        .await?;
    let link = shell.url(&format!("/invite/{token}"));
    Ok(once(&ctx, "Invite link", &format!("Valid for 7 days, one use, role {}.", role.as_str()), &link))
}

#[derive(Deserialize)]
struct RepoForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    kind: String,
}

impl RepoForm {
    fn checked(self) -> Result<(String, String, String), Failure> {
        let url = self.url.trim().trim_end_matches('/').to_owned();
        if !(url.is_empty() || url.starts_with("https://")) {
            return Err(Failure::bad_request("the url must be https"));
        }
        if !(self.kind.is_empty() || REPO_KINDS.iter().any(|(kind, _)| *kind == self.kind)) {
            return Err(Failure::bad_request("unknown repo type"));
        }
        Ok((self.name.trim().to_owned(), url, self.kind))
    }
}

async fn create_repo(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<RepoForm>,
) -> Result<Response, Failure> {
    ctx.require(Role::Maintainer)?;
    let (name, url, kind) = form.checked()?;
    if name.is_empty() {
        return Err(Failure::bad_request("a name is required"));
    }
    let token = secret();
    let (hash, org_id, label) = (sha256_hex(token.as_bytes()), ctx.org_id, format!("Upload token for {name}."));
    if !shell.db.call(move |conn| store::create_repo(conn, org_id, &name, &url, &kind, &hash)).await? {
        return Err(Failure::bad_request("a repo with that name already exists"));
    }
    Ok(once(&ctx, "Upload token", &label, &token))
}

async fn update_repo(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
    Form(form): Form<RepoForm>,
) -> Result<Response, Failure> {
    ctx.require(Role::Maintainer)?;
    let (_, url, kind) = form.checked()?;
    let org_id = ctx.org_id;
    shell.db.call(move |conn| store::update_repo(conn, org_id, id, &url, &kind)).await?;
    Ok(Redirect::to(&ctx.href("/settings")).into_response())
}

async fn rotate_repo_token(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Response, Failure> {
    ctx.require(Role::Maintainer)?;
    let token = secret();
    let (hash, org_id) = (sha256_hex(token.as_bytes()), ctx.org_id);
    let name = shell
        .db
        .call(move |conn| store::rotate_repo_token(conn, org_id, id, &hash))
        .await?
        .ok_or_else(Failure::not_found)?;
    Ok(once(&ctx, "Upload token", &format!("New upload token for {name}. The old one stopped working."), &token))
}

#[derive(Deserialize)]
struct ProviderForm {
    name: String,
    issuer: String,
    trusted: String,
}

async fn create_provider(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<ProviderForm>,
) -> Result<Response, Failure> {
    ctx.require(Role::Owner)?;
    let (name, issuer) = (form.name.trim().to_owned(), form.issuer.trim().trim_end_matches('/').to_owned());
    if name.is_empty() || !(issuer.starts_with("https://") || issuer.starts_with("http://localhost:")) {
        return Err(Failure::bad_request("a name and an https issuer are required"));
    }
    let (probe, redirect_uri) = (issuer.clone(), shell.url("/callback"));
    let client_id = tokio::task::spawn_blocking(move || sso::register(&sso::discover(&probe)?, &redirect_uri))
        .await?
        .map_err(|e| Failure(StatusCode::BAD_GATEWAY, e))?;
    let (org_id, trusted) = (ctx.org_id, form.trusted == "1");
    shell
        .db
        .call(move |conn| store::create_provider(conn, org_id, &name, &issuer, &client_id, trusted))
        .await?;
    Ok(Redirect::to(&ctx.href("/settings")).into_response())
}

async fn delete_provider(
    State(shell): State<Shell>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Response, Failure> {
    ctx.require(Role::Owner)?;
    let org_id = ctx.org_id;
    shell.db.call(move |conn| store::delete_provider(conn, org_id, id)).await?;
    Ok(Redirect::to(&ctx.href("/settings")).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_only_ever_a_local_path() {
        assert_eq!(local_path("/bugs?x=1"), "/bugs?x=1");
        assert_eq!(local_path("//evil.test"), "/");
        assert_eq!(local_path("https://evil.test"), "/");
        assert_eq!(local_path("/\\evil.test"), "/");
        assert_eq!(local_path(""), "/");
    }

    #[test]
    fn org_is_the_first_path_segment() {
        assert_eq!(split_org("/cc/bugs/7"), ("cc", "bugs/7"));
        assert_eq!(split_org("/cc"), ("cc", ""));
        assert_eq!(split_org("/"), ("", ""));
    }

    #[test]
    fn session_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "a=1; chi_session=abc; chi_session_x=no".parse().unwrap());
        assert_eq!(session_token(&headers), Some("abc"));
    }

    #[test]
    fn passwords_round_trip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(password_matches("correct horse", &hash));
        assert!(!password_matches("wrong", &hash));
    }
}
