use axum::{
    Extension, Form, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;
use shell::{Ctx, Failure, Role, ago, autolink, markdown, org_page, url_encode};

use crate::{
    Changes, INLINE, Issue, MAX_UPLOAD, PRIORITIES, People, STATUSES, Tracker, describe, query,
    store::{self, Attachment, Comment, Event, Link, Milestone, Project, Relation},
};

const TABS: [(&str, &str); 4] = [
    ("open", "is:open"),
    ("mine", "assignee:me is:open"),
    ("created", "creator:me"),
    ("all", ""),
];

const BOARD: &str = r#"let drag;document.querySelectorAll('.card').forEach(c=>{c.draggable=true;c.addEventListener('dragstart',e=>{drag=c;e.dataTransfer.setData('text/plain',c.dataset.key)})});
document.querySelectorAll('.board section').forEach(s=>{s.addEventListener('dragover',e=>{e.preventDefault();s.classList.add('over')});s.addEventListener('dragleave',()=>s.classList.remove('over'));
s.addEventListener('drop',async e=>{e.preventDefault();s.classList.remove('over');if(!drag)return;const cs=[...s.querySelectorAll('.card')].filter(c=>c!==drag),i=cs.findIndex(c=>{const r=c.getBoundingClientRect();return e.clientY<r.top+r.height/2}),next=i<0?null:cs[i],prev=i<0?cs[cs.length-1]:cs[i-1];
const a=prev?+prev.dataset.sort:null,b=next?+next.dataset.sort:null,sort=a!=null&&b!=null?(a+b)/2:a!=null?a+1:b!=null?b-1:0;drag.dataset.sort=sort;next?s.insertBefore(drag,next):s.append(drag);
const r=await fetch('issues/'+drag.dataset.key+'/move',{method:'POST',body:new URLSearchParams({status:s.dataset.status,sort})});if(!r.ok)location.reload()})})"#;

const AUTOSUBMIT: &str = "document.querySelectorAll('form.auto').forEach(f=>f.addEventListener('change',()=>f.requestSubmit()))";

pub fn routes() -> Router<Tracker> {
    Router::new()
        .route("/issues", get(list).post(create))
        .route("/issues/new", get(new_issue))
        .route("/issues/from-error/{id}", post(from_error))
        .route("/issues/{key}", get(issue).post(update))
        .route("/issues/{key}/move", post(move_issue))
        .route("/issues/{key}/comments", post(comment))
        .route("/issues/{key}/comments/{id}", post(edit_comment))
        .route("/issues/{key}/relations", post(relate))
        .route("/issues/{key}/links", post(attach_link))
        .route(
            "/issues/{key}/files",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD + 1024 * 1024)),
        )
        .route("/issues/{key}/subscribe", post(subscribe))
        .route("/files/{id}", get(file))
}

pub fn status_name(status: &str) -> String {
    status.replace('_', " ")
}

pub fn badge(status: &str) -> Markup {
    html! { span class={ "badge " (status) } { (status_name(status)) } }
}

pub fn priority(value: i64) -> Markup {
    let name = PRIORITIES.get(value as usize).copied().unwrap_or("none");
    html! { span class={ "p" (value.min(2)) } { (name) } }
}

pub fn row(issue: &Issue, who: &People) -> Markup {
    html! {
        a.row href={ "issues/" (issue.identifier()) } {
            strong { span.dim { (issue.identifier()) } " " (issue.title) }
            span {
                (badge(&issue.status)) " "
                @if issue.priority > 0 { (priority(issue.priority)) " · " }
                @if issue.assignee.is_some() { "@" (who.name(issue.assignee)) " · " }
                @if !issue.milestone.is_empty() { (issue.milestone) " · " }
                @for label in &issue.labels { "#" (label) " " }
                @if !issue.due_date.is_empty() { "due " (issue.due_date) " · " }
                @if issue.error_ref.is_some() { "[!] " }
                (ago(issue.updated_at))
            }
        }
    }
}

#[derive(Deserialize, Default)]
struct Listing {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    layout: String,
}

async fn list(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Query(listing): Query<Listing>,
) -> Result<Html<String>, Failure> {
    let q = listing.q.unwrap_or_else(|| "is:open".to_owned());
    let board = listing.layout == "board";
    let who = tracker.people(ctx.org_id).await?;
    let (issues, problem) = match tracker
        .list(
            &ctx,
            &q,
            if board { "manual" } else { "priority" },
            if board { 500 } else { 300 },
        )
        .await
    {
        Ok(issues) => (issues, None),
        Err(Failure(_, message)) => (Vec::new(), Some(message)),
    };
    let (org_id, user) = (ctx.org_id, ctx.user_id);
    let views = tracker
        .db
        .call(move |conn| store::views(conn, org_id, user))
        .await?;
    let layout = if board { "board" } else { "list" };
    let here =
        |query: &str, layout: &str| format!("issues?q={}&layout={layout}", url_encode(query));
    Ok(org_page(
        &ctx,
        "issues",
        html! {
            div.tabs data-live="issues" {
                @for (name, query) in TABS { a href=(here(query, layout)) aria-current=[(query == q).then_some("page")] { (name) } }
                @for view in &views { a href=(here(&view.query, &view.layout)) aria-current=[(view.query == q).then_some("page")] { (view.name) } }
                a.end href=(here(&q, if board { "list" } else { "board" })) { (if board { "list" } else { "board" }) }
            }
            form method="get" action="issues" {
                input type="search" name="q" value=(q) placeholder="status:todo assignee:me label:bug crash" aria-label="Filter issues";
                input type="hidden" name="layout" value=(layout);
            }
            @if let Some(problem) = problem { p.error { (problem) } } @else { p.dim { (issues.len()) " issues · " a href="issues/new" data-key="c" { "new issue" } " · " a href="views" { "views" } } }
            details.add {
                summary { "save as view" }
                form method="post" action="views" {
                    input type="hidden" name="query" value=(q);
                    input type="hidden" name="layout" value=(layout);
                    label { "name " input name="name" required; }
                    label { input type="checkbox" name="shared" value="1" checked; " visible to the whole org" }
                    button { "save view" }
                }
            }
            @if board {
                div.board {
                    @for status in STATUSES.iter().filter(|status| **status != "duplicate") {
                        section data-status=(status) {
                            h3 { (status_name(status)) " " span.dim { (issues.iter().filter(|issue| issue.status == *status).count()) } }
                            @for issue in issues.iter().filter(|issue| issue.status == *status) {
                                a.card href={ "issues/" (issue.identifier()) } data-key=(issue.identifier()) data-sort=(issue.sort) {
                                    span { (issue.identifier()) @if issue.priority > 0 { " · " (priority(issue.priority)) } @if issue.assignee.is_some() { " · @" (who.name(issue.assignee)) } }
                                    (issue.title)
                                }
                            }
                        }
                    }
                }
                @if ctx.role >= Role::Maintainer { script { (PreEscaped(BOARD)) } }
            } @else {
                @for issue in &issues { (row(issue, &who)) }
            }
        },
    ))
}

#[derive(Deserialize)]
struct Prefill {
    #[serde(default)]
    template: String,
    #[serde(default)]
    parent: String,
    #[serde(default)]
    project: String,
}

fn people_select(name: &str, who: &People, current: Option<i64>, key: Option<&str>) -> Markup {
    html! {
        select name=(name) data-key=[key] {
            option value="" { "nobody" }
            @for person in &who.0 {
                option value=(person.email) selected[current == Some(person.id)] { (person.handle()) @if person.agent { " (agent)" } }
            }
        }
    }
}

fn select(name: &str, options: &[&str], current: &str, key: Option<&str>) -> Markup {
    html! { select name=(name) data-key=[key] { @for option in options { option value=(option) selected[*option == current] { (status_name(option)) } } } }
}

async fn new_issue(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Query(prefill): Query<Prefill>,
) -> Result<Html<String>, Failure> {
    ctx.require(Role::Maintainer)?;
    let org_id = ctx.org_id;
    let (projects, templates) = tracker
        .db
        .call(move |conn| {
            Ok((
                store::projects(conn, org_id)?,
                store::templates(conn, org_id)?,
            ))
        })
        .await?;
    let who = tracker.people(ctx.org_id).await?;
    let template = templates
        .iter()
        .find(|template| template.name == prefill.template);
    let field = |key: &str| {
        template
            .and_then(|template| template.fields[key].as_str())
            .unwrap_or("")
            .to_owned()
    };
    let project = if prefill.project.is_empty() {
        field("project")
    } else {
        prefill.project.clone()
    };
    let labels = template.map(|template| match &template.fields["labels"] {
        serde_json::Value::Array(list) => list
            .iter()
            .filter_map(|label| label.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        other => other.as_str().unwrap_or("").to_owned(),
    });
    let status = Some(field("status"))
        .filter(|status| STATUSES.contains(&status.as_str()))
        .unwrap_or_else(|| "backlog".to_owned());
    let current_priority = query::priority(&field("priority")).unwrap_or(0) as usize;
    let assignee = who.find(&field("assignee")).map(|person| person.id);
    Ok(org_page(
        &ctx,
        "new issue",
        html! {
            h1 { "New issue" }
            @if projects.is_empty() { p.error { "Create a project first: " a href="projects" { "projects" } } }
            @if !templates.is_empty() {
                p.dim { "templates: " @for template in &templates { a href={ "issues/new?template=" (url_encode(&template.name)) "&parent=" (prefill.parent) "&project=" (url_encode(&prefill.project)) } { (template.name) } " " } }
            }
            form method="post" action="issues" {
                label { "title " input name="title" required autofocus value=(template.map(|t| t.title.as_str()).unwrap_or("")); }
                label { "description (Markdown, @mention people, CHI-12 links issues) " textarea name="description" rows="10" { (template.map(|t| t.description.as_str()).unwrap_or("")) } }
                label { "project " select name="project" { @for p in &projects { option value=(p.prefix) selected[p.prefix.eq_ignore_ascii_case(&project) || p.name.eq_ignore_ascii_case(&project)] { (p.prefix) " · " (p.name) } } } }
                label { "status " (select("status", &STATUSES, &status, None)) }
                label { "priority " (select("priority", &PRIORITIES, PRIORITIES[current_priority.min(4)], None)) }
                label { "assignee " (people_select("assignee", &who, assignee, None)) }
                label { "labels, comma separated " input name="labels" value=(labels.unwrap_or_default()); }
                label { "estimate " input name="estimate" type="number" min="0" max="1000" value=(field("estimate")); }
                label { "due " input name="due_date" type="date"; }
                label { "parent issue " input name="parent" value=(prefill.parent) placeholder="CHI-12"; }
                button.primary { "create issue" }
            }
        },
    ))
}

async fn create(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(changes): Form<Changes>,
) -> Result<Redirect, Failure> {
    let issue = tracker.save(&ctx, None, changes).await?;
    Ok(Redirect::to(
        &ctx.href(&format!("/issues/{}", issue.identifier())),
    ))
}

async fn from_error(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    let issue = tracker.from_error(&ctx, id).await?;
    Ok(Redirect::to(
        &ctx.href(&format!("/issues/{}", issue.identifier())),
    ))
}

struct Detail {
    comments: Vec<Comment>,
    history: Vec<Event>,
    relations: Vec<Relation>,
    attachments: Vec<Attachment>,
    links: Vec<Link>,
    children: Vec<Issue>,
    subscribed: bool,
    milestones: Vec<Milestone>,
    projects: Vec<Project>,
}

fn activity(detail: &Detail, who: &People, ctx: &Ctx, key: &str) -> Markup {
    enum Item<'a> {
        Said(&'a Comment),
        Did(&'a Event),
    }
    let mut items: Vec<(i64, Item)> = detail
        .comments
        .iter()
        .map(|c| (c.created_at, Item::Said(c)))
        .collect();
    items.extend(detail.history.iter().map(|e| (e.at, Item::Did(e))));
    items.sort_by_key(|(at, _)| *at);
    let name = |id: Option<i64>| {
        if id.is_none() {
            "chi".to_owned()
        } else {
            who.name(id)
        }
    };
    html! {
        @for (_, item) in &items {
            @match item {
                Item::Did(event) => div.frame.dim { (name(event.actor)) " " (describe(&event.field, &event.old, &event.new)) " · " (ago(event.at)) },
                Item::Said(comment) => div.frame id={ "c" (comment.id) } {
                    span.dim { strong { "@" (name(comment.author)) } " · " (ago(comment.created_at)) @if comment.updated_at > comment.created_at { " · edited" } }
                    div.md { (markdown(&comment.body)) }
                    @if comment.author == Some(ctx.user_id) || ctx.role == Role::Owner {
                        details {
                            summary.dim { "edit" }
                            form method="post" action={ "issues/" (key) "/comments/" (comment.id) } {
                                textarea name="body" rows="5" { (comment.body) }
                                div.actions { button { "save" } button name="delete" value="1" { "delete" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn properties(issue: &Issue, detail: &Detail, who: &People, can: bool) -> Markup {
    html! {
        form.auto method="post" action={ "issues/" (issue.identifier()) } {
            fieldset disabled[!can] style="border:0;padding:0;margin:0" {
                dl {
                    dt { "status" } dd { (select("status", &STATUSES, &issue.status, Some("s"))) }
                    dt { "priority" } dd { (select("priority", &PRIORITIES, issue.priority_name(), Some("p"))) }
                    dt { "assignee" } dd { (people_select("assignee", who, issue.assignee, Some("a"))) }
                    dt { "project" } dd { select name="project" { @for p in &detail.projects { option value=(p.prefix) selected[p.id == issue.project_id] { (p.prefix) " · " (p.name) } } } }
                    dt { "milestone" } dd {
                        select name="milestone" {
                            option value="" { "none" }
                            @for m in &detail.milestones { option value=(m.name) selected[Some(m.id) == issue.milestone_id] { (m.name) } }
                        }
                    }
                    dt { "labels" } dd { input name="labels" value=(issue.labels.join(", ")) placeholder="bug, ui"; }
                    dt { "estimate" } dd { input name="estimate" type="number" min="0" max="1000" value=[issue.estimate]; }
                    dt { "due" } dd { input name="due_date" type="date" value=(issue.due_date); }
                    dt { "parent" } dd { input name="parent" value=(issue.parent) placeholder="CHI-12"; }
                }
            }
            noscript { button { "save" } }
        }
    }
}

async fn issue(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
) -> Result<Response, Failure> {
    let issue = tracker.issue(&ctx, &key).await?;
    if issue.identifier() != key {
        return Ok(
            Redirect::to(&ctx.href(&format!("/issues/{}", issue.identifier()))).into_response(),
        );
    }
    let who = tracker.people(ctx.org_id).await?;
    let (org_id, id, project_id, user) = (ctx.org_id, issue.id, issue.project_id, ctx.user_id);
    let detail = tracker
        .db
        .call(move |conn| {
            Ok(Detail {
                comments: store::comments(conn, id)?,
                history: store::history(conn, id)?,
                relations: store::relations(conn, id)?,
                attachments: store::attachments(conn, id)?,
                links: store::links(conn, id)?,
                children: store::issues(conn, org_id, "i.parent_id = ?", vec![id.into()], "i.sort, i.id", 200)?,
                subscribed: conn.query_row("SELECT EXISTS (SELECT 1 FROM subscriptions WHERE issue_id = ?1 AND user_id = ?2)", (id, user), |row| row.get(0))?,
                milestones: store::milestones(conn, project_id)?,
                projects: store::projects(conn, org_id)?,
            })
        })
        .await?;
    let can = ctx.role >= Role::Maintainer;
    let base = format!("issues/{key}");
    Ok(org_page(
        &ctx,
        &format!("{key} {}", issue.title),
        html! {
            div data-live="issues" {
                p.dim {
                    a href={ "projects/" (issue.prefix) } { (issue.project) } " · " (key)
                    @if !issue.parent.is_empty() { " · sub-issue of " (autolink(&issue.parent)) }
                    @if let Some(error) = issue.error_ref { " · from " a href={ "bugs/" (error) } { "error #" (error) } }
                    " · opened by @" (who.name(issue.creator)) " " (ago(issue.created_at))
                }
                h1 { (issue.title) }
                div.split {
                    div {
                        @if issue.description.is_empty() { p.dim { "No description." } } @else { div.md { (markdown(&issue.description)) } }
                        @if can {
                            details {
                                summary.dim data-key="e" { "edit title and description" }
                                form method="post" action=(base) {
                                    label { "title " input name="title" value=(issue.title) required; }
                                    label { "description " textarea name="description" rows="14" { (issue.description) } }
                                    button { "save" }
                                }
                            }
                        }

                        h2 { "Sub-issues" }
                        @for child in &detail.children { (row(child, &who)) }
                        @if can { p { a href={ "issues/new?parent=" (key) "&project=" (issue.prefix) } { "add a sub-issue" } } }

                        h2 { "Relations" }
                        @for relation in &detail.relations {
                            div.frame {
                                span.dim { (relation.label) " " } (autolink(&relation.other)) " " (relation.title) " " (badge(&relation.status))
                            }
                        }
                        @if can {
                            form.inline method="post" action={ (base) "/relations" } {
                                select name="kind" { option value="blocks" { "blocks" } option value="related" { "related to" } option value="duplicate" { "duplicate of" } }
                                " " input name="other" placeholder="CHI-12" required size="10";
                                " " button { "relate" } " " button name="remove" value="1" { "unrelate" }
                            }
                        }

                        h2 { "Attachments" }
                        @for file in &detail.attachments {
                            div.frame {
                                @if file.url.is_empty() {
                                    a href={ "files/" (file.id) } { (file.title) } span.dim { " " (file.size / 1024) " KB" }
                                    @if INLINE.contains(&file.mime.as_str()) { div { img src={ "files/" (file.id) } alt=(file.title) style="max-width:100%;max-height:320px;border-radius:6px"; } }
                                } @else {
                                    a href=(file.url) rel="noreferrer" { (file.title) }
                                }
                                span.dim { " · @" (who.name(file.uploader)) " " (ago(file.created_at)) }
                            }
                        }
                        @for link in &detail.links {
                            div.frame {
                                span.dim { (link.kind) " " } a href=(link.url) rel="noreferrer" { (link.title) } " " span class={ "badge " (link.state) } { (link.state) } span.dim { " " (ago(link.at)) }
                            }
                        }
                        @if can {
                            details.add {
                                summary { "attach" }
                                form method="post" action={ (base) "/files" } enctype="multipart/form-data" {
                                    label { "file, up to 25 MB " input type="file" name="file" required; }
                                    button { "upload" }
                                }
                                form method="post" action={ (base) "/links" } {
                                    label { "link " input name="url" type="url" required placeholder="https://"; }
                                    label { "title " input name="title"; }
                                    button { "add link" }
                                }
                            }
                        }

                        h2 { "Activity" }
                        (activity(&detail, &who, &ctx, &key))
                        @if can {
                            form method="post" action={ (base) "/comments" } {
                                label { "comment (mention @claude or @codex to ask an agent) " textarea name="body" rows="5" required {} }
                                button.primary { "comment" }
                            }
                        }
                    }
                    aside {
                        (properties(&issue, &detail, &who, can))
                        form method="post" action={ (base) "/subscribe" } {
                            @if detail.subscribed { button name="on" value="0" { "unsubscribe" } } @else { button name="on" value="1" { "subscribe" } }
                        }
                        p.dim { "updated " (ago(issue.updated_at)) @if let Some(at) = issue.completed_at { br; "closed " (ago(at)) } }
                    }
                }
            }
            script { (PreEscaped(AUTOSUBMIT)) }
        },
    )
    .into_response())
}

async fn update(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(mut changes): Form<Changes>,
) -> Result<Redirect, Failure> {
    let issue = tracker.issue(&ctx, &key).await?;
    if changes
        .project
        .as_deref()
        .is_some_and(|project| !project.eq_ignore_ascii_case(&issue.prefix))
    {
        changes.milestone = None;
    }
    let issue = tracker.save(&ctx, Some(&key), changes).await?;
    Ok(Redirect::to(
        &ctx.href(&format!("/issues/{}", issue.identifier())),
    ))
}

#[derive(Deserialize)]
struct Move {
    status: String,
    sort: f64,
}

async fn move_issue(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(to): Form<Move>,
) -> Result<StatusCode, Failure> {
    let changes = Changes {
        status: Some(to.status),
        sort: Some(to.sort),
        ..Changes::default()
    };
    tracker.save(&ctx, Some(&key), changes).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Said {
    #[serde(default)]
    body: String,
    #[serde(default)]
    delete: Option<String>,
}

async fn comment(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(said): Form<Said>,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let issue = tracker.issue(&ctx, &key).await?;
    let id = tracker
        .comment(
            ctx.org_id,
            &issue,
            Some(ctx.user_id),
            &ctx.email,
            &said.body,
        )
        .await?;
    Ok(Redirect::to(
        &ctx.href(&format!("/issues/{}#c{id}", issue.identifier())),
    ))
}

async fn edit_comment(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path((_, id)): Path<(String, i64)>,
    Form(said): Form<Said>,
) -> Result<Redirect, Failure> {
    let key = tracker
        .edit_comment(
            &ctx,
            id,
            if said.delete.is_some() {
                None
            } else {
                Some(said.body)
            },
        )
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/issues/{key}"))))
}

#[derive(Deserialize)]
struct Relate {
    kind: String,
    other: String,
    #[serde(default)]
    remove: Option<String>,
}

async fn relate(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(form): Form<Relate>,
) -> Result<Redirect, Failure> {
    let issue = tracker.issue(&ctx, &key).await?;
    tracker
        .relate(&ctx, &issue, &form.kind, &form.other, form.remove.is_some())
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/issues/{key}"))))
}

#[derive(Deserialize)]
struct LinkForm {
    url: String,
    #[serde(default)]
    title: String,
}

async fn attach_link(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(form): Form<LinkForm>,
) -> Result<Redirect, Failure> {
    let issue = tracker.issue(&ctx, &key).await?;
    tracker
        .attach_link(&ctx, &issue, &form.title, &form.url)
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/issues/{key}"))))
}

async fn upload(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    mut form: Multipart,
) -> Result<Redirect, Failure> {
    ctx.require(Role::Maintainer)?;
    let issue = tracker.issue(&ctx, &key).await?;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| Failure::bad_request(&e.body_text()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("file").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| Failure::bad_request(&e.body_text()))?;
        tracker
            .upload(&ctx, &issue, &name, &mime, bytes.to_vec())
            .await?;
    }
    Ok(Redirect::to(&ctx.href(&format!("/issues/{key}"))))
}

#[derive(Deserialize)]
struct On {
    on: String,
}

async fn subscribe(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(form): Form<On>,
) -> Result<Redirect, Failure> {
    let issue = tracker.issue(&ctx, &key).await?;
    tracker.set_subscribed(&ctx, &issue, form.on == "1").await?;
    Ok(Redirect::to(&ctx.href(&format!("/issues/{key}"))))
}

async fn file(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Response, Failure> {
    let org_id = ctx.org_id;
    let found = tracker
        .db
        .call(move |conn| {
            let row = conn.query_row("SELECT title, file, mime FROM attachments WHERE id = ?1 AND org_id = ?2 AND file != ''", (id, org_id), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            });
            rusqlite::OptionalExtension::optional(row)
        })
        .await?;
    let (title, file, mime) = found.ok_or_else(Failure::not_found)?;
    let bytes = tokio::fs::read(tracker.files.join(&file))
        .await
        .map_err(|_| Failure::not_found())?;
    let inline = INLINE.contains(&mime.as_str());
    let name: String = title
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let disposition = format!(
        "{}; filename=\"{name}\"; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        url_encode(&title)
    );
    let headers = [
        (
            header::CONTENT_TYPE,
            if inline {
                mime
            } else {
                "application/octet-stream".to_owned()
            },
        ),
        (header::CONTENT_DISPOSITION, disposition),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        (
            header::CONTENT_SECURITY_POLICY,
            "sandbox; default-src 'none'; img-src 'self'".to_owned(),
        ),
        (header::CACHE_CONTROL, "private, max-age=86400".to_owned()),
    ];
    Ok((headers, bytes).into_response())
}
