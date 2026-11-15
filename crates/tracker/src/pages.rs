use axum::{
    Extension, Form, Json, Router,
    extract::{Path, Query, State},
    response::{Html, Redirect},
    routing::{get, post},
};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::{Value, json};
use shell::{Ctx, Failure, Role, ago, markdown, org_page, url_encode};

use crate::{
    Tracker,
    store::{self, Project, ProjectFields},
    ui::row,
};

pub fn routes() -> Router<Tracker> {
    Router::new()
        .route("/projects", get(projects).post(create_project))
        .route("/projects/{key}", get(project).post(update_project))
        .route("/projects/{key}/milestones", post(save_milestone))
        .route("/docs", get(docs).post(create_doc))
        .route("/docs/new", get(new_doc))
        .route("/docs/{id}", get(doc).post(update_doc))
        .route("/inbox", get(inbox))
        .route("/inbox/read", post(read))
        .route("/labels", get(labels).post(save_label))
        .route("/templates", get(templates).post(save_template))
        .route("/views", get(views).post(save_view))
        .route("/views/{id}/delete", post(delete_view))
        .route("/search.json", get(search))
}

fn progress(done: i64, total: i64) -> Markup {
    let percent = if total == 0 { 0 } else { done * 100 / total };
    html! { div.bar title={ (done) " of " (total) " done" } { i style={ "width:" (percent) "%" } {} } span.dim { (done) "/" (total) " done" } }
}

fn project_form(project: Option<&Project>, who: &crate::People, repos: &[shell::Repo]) -> Markup {
    let text = |get: fn(&Project) -> &str| project.map(get).unwrap_or("");
    html! {
        label { "name " input name="name" required value=(text(|p| &p.name)); }
        label { "issue prefix, e.g. CHI " input name="prefix" required pattern="[A-Za-z][A-Za-z0-9]{0,9}" value=(text(|p| &p.prefix)); }
        label { "description " textarea name="description" rows="4" { (text(|p| &p.description)) } }
        label { "status " select name="status" { @for status in store::PROJECT_STATUSES { option selected[project.is_some_and(|p| p.status == status)] { (status) } } } }
        label { "lead " select name="lead" {
            option value="" { "nobody" }
            @for person in who.0.iter().filter(|person| !person.agent) { option value=(person.handle()) selected[project.is_some_and(|p| p.lead == Some(person.id))] { (person.handle()) } }
        } }
        label { "target date " input name="target_date" type="date" value=(text(|p| &p.target_date)); }
        label { "repository " select name="repo" {
            option value="" { "none" }
            @for repo in repos { option value=(repo.id) selected[project.is_some_and(|p| p.repo_id == Some(repo.id))] { (repo.name) } }
        } }
    }
}

async fn projects(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let projects = tracker
        .db
        .call(move |conn| store::projects(conn, org_id))
        .await?;
    let who = tracker.people(org_id).await?;
    let repos = tracker.shell.repos(org_id).await?;
    Ok(org_page(
        &ctx,
        "projects",
        html! {
            h1 { "Projects" }
            div data-live="issues" {
                @if projects.is_empty() { p.dim { "No projects yet. A project owns its issues and their prefix." } }
                @for p in &projects {
                    a.row href={ "projects/" (p.prefix) } {
                        strong { span.dim { (p.prefix) } " " (p.name) }
                        span { span class={ "badge " (p.status) } { (p.status) } @if p.lead.is_some() { " · lead @" (who.name(p.lead)) } @if !p.target_date.is_empty() { " · target " (p.target_date) } }
                        (progress(p.done, p.total))
                    }
                }
            }
            @if ctx.role >= Role::Maintainer {
                details.add open[projects.is_empty()] {
                    summary { "new project" }
                    form.stack method="post" action="projects" { (project_form(None, &who, &repos)) button.primary { "create project" } }
                }
            }
        },
    ))
}

#[derive(Deserialize, Default)]
pub(crate) struct ProjectInput {
    pub prefix: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub lead: Option<String>,
    pub target_date: Option<String>,
    pub repo: Option<String>,
}

impl Tracker {
    pub(crate) async fn project_fields(
        &self,
        ctx: &Ctx,
        input: ProjectInput,
    ) -> Result<ProjectFields, Failure> {
        let ProjectInput {
            prefix,
            name,
            description,
            status,
            lead,
            target_date,
            repo,
        } = input;
        let who = self.people(ctx.org_id).await?;
        let lead = match lead.as_deref().map(str::trim) {
            None => None,
            Some("" | "none") => Some(None),
            Some(handle) => Some(Some(
                who.find(handle)
                    .ok_or_else(|| Failure::bad_request(&format!("nobody called {handle} here")))?
                    .id,
            )),
        };
        let repos = self.shell.repos(ctx.org_id).await?;
        let repo_id = match repo.as_deref().map(str::trim) {
            None => None,
            Some("" | "none") => Some(None),
            Some(repo) => Some(Some(
                repos
                    .iter()
                    .find(|r| {
                        r.id.to_string() == repo
                            || r.name.eq_ignore_ascii_case(repo)
                            || r.url == repo
                    })
                    .ok_or_else(|| Failure::bad_request(&format!("no repository {repo}")))?
                    .id,
            )),
        };
        Ok(ProjectFields {
            prefix,
            name,
            description,
            lead,
            status: status.filter(|status| !status.is_empty()),
            target_date,
            repo_id,
        })
    }

    pub(crate) async fn save_project(
        &self,
        ctx: &Ctx,
        key: Option<String>,
        fields: ProjectFields,
    ) -> Result<String, Failure> {
        let org_id = ctx.org_id;
        self.change(ctx, "issues", move |conn| {
            store::save_project(conn, org_id, key.as_deref(), fields)
        })
        .await
    }
}

async fn create_project(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<ProjectInput>,
) -> Result<Redirect, Failure> {
    let fields = tracker.project_fields(&ctx, form).await?;
    let prefix = tracker.save_project(&ctx, None, fields).await?;
    Ok(Redirect::to(&ctx.href(&format!("/projects/{prefix}"))))
}

async fn update_project(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(form): Form<ProjectInput>,
) -> Result<Redirect, Failure> {
    let fields = tracker.project_fields(&ctx, form).await?;
    let prefix = tracker.save_project(&ctx, Some(key), fields).await?;
    Ok(Redirect::to(&ctx.href(&format!("/projects/{prefix}"))))
}

async fn project(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let lookup = key.clone();
    let (project, milestones, docs) = tracker
        .db
        .call(move |conn| {
            let Some(project) = store::projects(conn, org_id)?.into_iter().find(|p| {
                p.prefix.eq_ignore_ascii_case(&lookup) || p.name.eq_ignore_ascii_case(&lookup)
            }) else {
                return Ok((None, Vec::new(), Vec::new()));
            };
            let milestones = store::milestones(conn, project.id)?;
            let docs = store::documents(conn, org_id, &project.prefix, "", 50)?;
            Ok((Some(project), milestones, docs))
        })
        .await?;
    let project = project.ok_or_else(Failure::not_found)?;
    let who = tracker.people(org_id).await?;
    let repos = tracker.shell.repos(org_id).await?;
    let open = tracker
        .list(
            &ctx,
            &format!("project:{} is:open", project.prefix),
            "priority",
            200,
        )
        .await?;
    let can = ctx.role >= Role::Maintainer;
    Ok(org_page(
        &ctx,
        &project.name,
        html! {
            div data-live="issues" {
                p.dim { a href="projects" { "projects" } " · " (project.prefix) }
                h1 { (project.name) }
                p { span class={ "badge " (project.status) } { (project.status) }
                    @if project.lead.is_some() { " lead @" (who.name(project.lead)) }
                    @if !project.target_date.is_empty() { " · target " (project.target_date) }
                    @if let Some(repo) = repos.iter().find(|repo| Some(repo.id) == project.repo_id) { " · " a href=(repo.url) { (repo.name) } } }
                (progress(project.done, project.total))
                @if !project.description.is_empty() { div.md { (markdown(&project.description)) } }
                p { a href={ "issues?q=" (url_encode(&format!("project:{}", project.prefix))) "&layout=board" } { "board" } " · "
                    a href={ "issues/new?project=" (project.prefix) } { "new issue" } " · "
                    a href={ "docs/new?project=" (project.prefix) } { "new document" } }

                h2 { "Milestones" }
                @for m in &milestones {
                    div.frame {
                        a href={ "issues?q=" (url_encode(&format!("project:{} milestone:\"{}\"", project.prefix, m.name))) } { strong { (m.name) } }
                        @if !m.target_date.is_empty() { span.dim { " · target " (m.target_date) } }
                        (progress(m.done, m.total))
                        @if !m.description.is_empty() { div.md { (markdown(&m.description)) } }
                        @if can {
                            details { summary.dim { "edit" }
                                form method="post" action={ "projects/" (project.prefix) "/milestones" } {
                                    input type="hidden" name="id" value=(m.id);
                                    label { "name " input name="name" value=(m.name) required; }
                                    label { "target date " input name="target_date" type="date" value=(m.target_date); }
                                    label { "description " textarea name="description" rows="3" { (m.description) } }
                                    button { "save" }
                                }
                            }
                        }
                    }
                }
                @if can {
                    details.add { summary { "new milestone" }
                        form method="post" action={ "projects/" (project.prefix) "/milestones" } {
                            label { "name " input name="name" required; }
                            label { "target date " input name="target_date" type="date"; }
                            label { "description " textarea name="description" rows="3" {} }
                            button { "add milestone" }
                        }
                    }
                }

                h2 { "Documents" }
                @for d in &docs { a.row href={ "docs/" (d.id) } { strong { (d.title) } span { "@" (who.name(d.author)) " · " (ago(d.updated_at)) } } }

                h2 { "Open issues" }
                @for issue in &open { (row(issue, &who)) }

                @if can {
                    details.add { summary { "edit project" }
                        form.stack method="post" action={ "projects/" (project.prefix) } { (project_form(Some(&project), &who, &repos)) button.primary { "save" } }
                    }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
struct MilestoneForm {
    #[serde(default)]
    id: Option<i64>,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    target_date: String,
}

impl Tracker {
    pub(crate) async fn save_milestone(
        &self,
        ctx: &Ctx,
        project: String,
        id: Option<i64>,
        name: String,
        description: String,
        target_date: String,
    ) -> Result<i64, Failure> {
        let org_id = ctx.org_id;
        self.change(ctx, "issues", move |conn| {
            let (project_id, _) = store::project_by_key(conn, org_id, &project)?
                .ok_or_else(|| store::Problem::Invalid(format!("no project {project}")))?;
            store::save_milestone(conn, project_id, id, &name, &description, &target_date)
        })
        .await
    }
}

async fn save_milestone(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(key): Path<String>,
    Form(form): Form<MilestoneForm>,
) -> Result<Redirect, Failure> {
    tracker
        .save_milestone(
            &ctx,
            key.clone(),
            form.id,
            form.name,
            form.description,
            form.target_date,
        )
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/projects/{key}"))))
}

#[derive(Deserialize, Default)]
struct DocQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    project: String,
}

async fn docs(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<DocQuery>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let (project, text) = (query.project.clone(), query.q.clone());
    let docs = tracker
        .db
        .call(move |conn| store::documents(conn, org_id, &project, &text, 200))
        .await?;
    let who = tracker.people(org_id).await?;
    Ok(org_page(
        &ctx,
        "docs",
        html! {
            h1 { "Documents" }
            form method="get" action="docs" { input type="search" name="q" value=(query.q) placeholder="Search documents" aria-label="Search documents"; input type="hidden" name="project" value=(query.project); }
            @if ctx.role >= Role::Maintainer { p { a href="docs/new" data-key="c" { "new document" } } }
            div data-live="docs" {
                @if docs.is_empty() { p.dim { "No documents." } }
                @for d in &docs {
                    a.row href={ "docs/" (d.id) } { strong { (d.title) } span { @if !d.project.is_empty() { (d.project) " · " } "@" (who.name(d.author)) " · " (ago(d.updated_at)) } }
                }
            }
        },
    ))
}

fn doc_form(action: &str, title: &str, body: &str, project: &str, projects: &[Project]) -> Markup {
    html! {
        form method="post" action=(action) {
            label { "title " input name="title" required value=(title); }
            label { "project " select name="project" { option value="" { "none" } @for p in projects { option value=(p.prefix) selected[p.prefix.eq_ignore_ascii_case(project)] { (p.prefix) " · " (p.name) } } } }
            label { "body (Markdown) " textarea.code name="body" rows="24" { (body) } }
            button.primary { "save" }
        }
    }
}

async fn new_doc(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<DocQuery>,
) -> Result<Html<String>, Failure> {
    ctx.require(Role::Maintainer)?;
    let org_id = ctx.org_id;
    let projects = tracker
        .db
        .call(move |conn| store::projects(conn, org_id))
        .await?;
    Ok(org_page(
        &ctx,
        "new document",
        html! { h1 { "New document" } (doc_form("docs", "", "", &query.project, &projects)) },
    ))
}

#[derive(Deserialize)]
struct DocForm {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    project: String,
}

impl Tracker {
    pub(crate) async fn save_document(
        &self,
        ctx: &Ctx,
        id: Option<i64>,
        title: String,
        body: String,
        project: Option<String>,
    ) -> Result<i64, Failure> {
        let (org_id, author) = (ctx.org_id, ctx.user_id);
        let id = self
            .change(ctx, "docs", move |conn| {
                store::save_document(conn, org_id, author, id, &title, &body, project.as_deref())
            })
            .await?;
        Ok(id)
    }
}

async fn create_doc(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<DocForm>,
) -> Result<Redirect, Failure> {
    let id = tracker
        .save_document(&ctx, None, form.title, form.body, Some(form.project))
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/docs/{id}"))))
}

async fn update_doc(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
    Form(form): Form<DocForm>,
) -> Result<Redirect, Failure> {
    tracker
        .save_document(&ctx, Some(id), form.title, form.body, Some(form.project))
        .await?;
    Ok(Redirect::to(&ctx.href(&format!("/docs/{id}"))))
}

async fn doc(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let (doc, projects) = tracker
        .db
        .call(move |conn| {
            Ok((
                store::document(conn, org_id, id)?,
                store::projects(conn, org_id)?,
            ))
        })
        .await?;
    let doc = doc.ok_or_else(Failure::not_found)?;
    let who = tracker.people(org_id).await?;
    Ok(org_page(
        &ctx,
        &doc.title,
        html! {
            p.dim { a href="docs" { "docs" } @if !doc.project.is_empty() { " · " a href={ "projects/" (doc.project) } { (doc.project) } } " · @" (who.name(doc.author)) " · edited " (ago(doc.updated_at)) }
            h1 { (doc.title) }
            div.md data-live="docs" { (markdown(&doc.body)) }
            @if ctx.role >= Role::Maintainer {
                details { summary.dim data-key="e" { "edit" } (doc_form(&format!("docs/{id}"), &doc.title, &doc.body, &doc.project, &projects)) }
            }
        },
    ))
}

#[derive(Deserialize)]
struct InboxQuery {
    #[serde(default)]
    all: Option<String>,
}

async fn inbox(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<InboxQuery>,
) -> Result<Html<String>, Failure> {
    let (org_id, user, unread) = (ctx.org_id, ctx.user_id, query.all.is_none());
    let items = tracker
        .db
        .call(move |conn| store::notifications(conn, org_id, user, unread, 200))
        .await?;
    let who = tracker.people(org_id).await?;
    Ok(org_page(
        &ctx,
        "inbox",
        html! {
            div.tabs data-live="issues inbox" {
                a href="inbox" aria-current=[unread.then_some("page")] { "unread" }
                a href="inbox?all=1" aria-current=[(!unread).then_some("page")] { "all" }
            }
            @if items.is_empty() { p.dim { "Nothing new." } }
            @else { form method="post" action="inbox/read" { button { "mark all read" } } }
            @for item in &items {
                div.row {
                    strong { a href={ "issues/" (item.issue) } { (item.issue) } " " (item.title) }
                    span { "@" (if item.actor.is_none() { "chi".to_owned() } else { who.name(item.actor) }) " " (item.text) " · " (ago(item.created_at)) }
                    form.inline method="post" action="inbox/read" {
                        input type="hidden" name="id" value=(item.id);
                        @if item.read { button name="unread" value="1" { "mark unread" } } @else { button { "mark read" } }
                    }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
struct ReadForm {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    unread: Option<String>,
}

async fn read(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<ReadForm>,
) -> Result<Redirect, Failure> {
    tracker.mark(&ctx, form.id, form.unread.is_none()).await?;
    Ok(Redirect::to(&ctx.href("/inbox")))
}

async fn labels(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let labels = tracker
        .db
        .call(move |conn| store::labels(conn, org_id))
        .await?;
    Ok(org_page(
        &ctx,
        "labels",
        html! {
            h1 { "Labels" }
            @for label in &labels {
                div.frame {
                    span style={ "color:" (label.color) } { "●" } " "
                    a href={ "issues?q=" (url_encode(&format!("label:\"{}\" is:open", label.name))) } { (label.name) }
                    span.dim { " " (label.open) " open" }
                    @if ctx.role >= Role::Maintainer {
                        form.inline method="post" action="labels" {
                            input type="hidden" name="name" value=(label.name);
                            " " input name="new_name" value=(label.name) size="16" aria-label="name";
                            " " input name="color" type="color" value=(label.color) aria-label="color";
                            " " button { "save" }
                        }
                    }
                }
            }
            @if ctx.role >= Role::Maintainer {
                form.inline method="post" action="labels" { input name="name" placeholder="new label" required; " " input name="color" type="color" value="#8a8f98"; " " button { "add" } }
            }
        },
    ))
}

#[derive(Deserialize)]
struct LabelForm {
    name: String,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    color: Option<String>,
}

async fn save_label(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<LabelForm>,
) -> Result<Redirect, Failure> {
    let org_id = ctx.org_id;
    tracker
        .change(&ctx, "issues", move |conn| {
            store::save_label(
                conn,
                org_id,
                &form.name,
                form.new_name.as_deref(),
                form.color.as_deref(),
            )
        })
        .await?;
    Ok(Redirect::to(&ctx.href("/labels")))
}

async fn templates(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
) -> Result<Html<String>, Failure> {
    let org_id = ctx.org_id;
    let templates = tracker
        .db
        .call(move |conn| store::templates(conn, org_id))
        .await?;
    let can = ctx.role >= Role::Maintainer;
    let form = |name: &str, title: &str, description: &str, fields: &Value| {
        html! {
            form method="post" action="templates" {
                label { "name " input name="name" required value=(name); }
                label { "issue title " input name="title" value=(title); }
                label { "description " textarea name="description" rows="8" { (description) } }
                label { "fields as JSON: project, status, priority, assignee, labels, estimate " textarea.code name="fields" rows="3" { (if fields.is_null() { "{}".to_owned() } else { fields.to_string() }) } }
                div.actions { button { "save" } @if !name.is_empty() { button name="delete" value="1" { "delete" } } }
            }
        }
    };
    Ok(org_page(
        &ctx,
        "templates",
        html! {
            h1 { "Issue templates" }
            @for t in &templates {
                details.frame {
                    summary { strong { (t.name) } " " a href={ "issues/new?template=" (url_encode(&t.name)) } { "use" } }
                    @if can { (form(&t.name, &t.title, &t.description, &t.fields)) } @else { div.md { (markdown(&t.description)) } }
                }
            }
            @if can { details.add { summary { "new template" } (form("", "", "", &Value::Null)) } }
        },
    ))
}

#[derive(Deserialize)]
struct TemplateForm {
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    fields: String,
    #[serde(default)]
    delete: Option<String>,
}

impl Tracker {
    pub(crate) async fn save_template(
        &self,
        ctx: &Ctx,
        name: String,
        title: String,
        description: String,
        fields: Value,
        delete: bool,
    ) -> Result<(), Failure> {
        let org_id = ctx.org_id;
        let name = name.trim().to_owned();
        if name.is_empty() || !fields.is_object() {
            return Err(Failure::bad_request(
                "a template needs a name, and its fields are a JSON object",
            ));
        }
        self.change(ctx, "issues", move |conn| {
            if delete {
                conn.execute("DELETE FROM templates WHERE org_id = ?1 AND name = ?2", (org_id, &name))?;
            } else {
                conn.execute(
                    "INSERT INTO templates (org_id, name, title, description, fields) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (org_id, name) DO UPDATE SET title = excluded.title, description = excluded.description, fields = excluded.fields",
                    (org_id, &name, title, description, fields.to_string()),
                )?;
            }
            Ok(())
        })
        .await
    }
}

async fn save_template(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<TemplateForm>,
) -> Result<Redirect, Failure> {
    let fields: Value = serde_json::from_str(if form.fields.trim().is_empty() {
        "{}"
    } else {
        &form.fields
    })
    .map_err(|e| Failure::bad_request(&format!("fields: {e}")))?;
    tracker
        .save_template(
            &ctx,
            form.name,
            form.title,
            form.description,
            fields,
            form.delete.is_some(),
        )
        .await?;
    Ok(Redirect::to(&ctx.href("/templates")))
}

async fn views(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
) -> Result<Html<String>, Failure> {
    let (org_id, user) = (ctx.org_id, ctx.user_id);
    let views = tracker
        .db
        .call(move |conn| store::views(conn, org_id, user))
        .await?;
    let who = tracker.people(org_id).await?;
    Ok(org_page(
        &ctx,
        "views",
        html! {
            h1 { "Views" }
            p.dim { "A view is a saved issue filter: " code { (crate::query::HELP) } }
            @if views.is_empty() { p.dim { "No views. Filter the issues, then save the filter as a view." } }
            @for view in &views {
                div.row {
                    strong { a href={ "issues?q=" (url_encode(&view.query)) "&layout=" (view.layout) } { (view.name) } }
                    span { code { (view.query) } " · " (view.layout) " · " (if view.shared { "shared" } else { "private" }) " · @" (who.name(Some(view.owner))) }
                    @if view.owner == ctx.user_id || ctx.role == Role::Owner {
                        form.inline method="post" action={ "views/" (view.id) "/delete" } { button { "delete" } }
                    }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
struct ViewForm {
    name: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    layout: String,
    #[serde(default)]
    shared: Option<String>,
}

impl Tracker {
    pub(crate) async fn save_view(
        &self,
        ctx: &Ctx,
        name: String,
        query: String,
        layout: String,
        shared: bool,
    ) -> Result<i64, Failure> {
        let who = self.people(ctx.org_id).await?;
        crate::query::compile(&query, ctx.user_id, &who).map_err(|e| Failure::bad_request(&e))?;
        let name = name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 80 {
            return Err(Failure::bad_request(
                "a view needs a name of up to 80 characters",
            ));
        }
        let layout = if layout == "board" { "board" } else { "list" };
        let (org_id, user) = (ctx.org_id, ctx.user_id);
        self.change(ctx, "issues", move |conn| {
            conn.execute("INSERT INTO views (org_id, owner, name, query, layout, shared) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", (org_id, user, name, query, layout, shared))?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }
}

async fn save_view(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Form(form): Form<ViewForm>,
) -> Result<Redirect, Failure> {
    let layout = form.layout.clone();
    let query = form.query.clone();
    tracker
        .save_view(
            &ctx,
            form.name,
            form.query,
            form.layout,
            form.shared.is_some(),
        )
        .await?;
    Ok(Redirect::to(&ctx.href(&format!(
        "/issues?q={}&layout={layout}",
        url_encode(&query)
    ))))
}

async fn delete_view(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Path(id): Path<i64>,
) -> Result<Redirect, Failure> {
    let (org_id, user, owner) = (ctx.org_id, ctx.user_id, ctx.role == Role::Owner);
    let deleted = tracker
        .db
        .call(move |conn| {
            conn.execute(
                "DELETE FROM views WHERE id = ?1 AND org_id = ?2 AND (owner = ?3 OR ?4)",
                (id, org_id, user, owner),
            )
        })
        .await?;
    if deleted == 0 {
        return Err(Failure::not_found());
    }
    tracker.shell.publish(org_id, "issues");
    Ok(Redirect::to(&ctx.href("/views")))
}

const PAGES: [(&str, &str); 12] = [
    ("issues", "issues"),
    ("my issues", "issues?q=assignee:me%20is:open"),
    ("board", "issues?layout=board"),
    ("projects", "projects"),
    ("docs", "docs"),
    ("inbox", "inbox"),
    ("labels", "labels"),
    ("templates", "templates"),
    ("views", "views"),
    ("bugs", "bugs"),
    ("builds", "builds"),
    ("agents", "agents"),
];

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

async fn search(
    State(tracker): State<Tracker>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, Failure> {
    Ok(Json(Value::Array(tracker.search(&ctx, &query.q, 8).await?)))
}

impl Tracker {
    pub(crate) async fn search(
        &self,
        ctx: &Ctx,
        text: &str,
        limit: i64,
    ) -> Result<Vec<Value>, Failure> {
        let text = text.trim().to_owned();
        let lower = text.to_lowercase();
        let mut found: Vec<Value> = PAGES
            .iter()
            .filter(|(name, _)| !lower.is_empty() && name.contains(&lower) || lower.is_empty())
            .map(|(name, url)| json!({ "title": name, "url": url, "hint": "page" }))
            .collect();
        let issues = match self.list(ctx, &text, "updated", limit).await {
            Ok(issues) => issues,
            Err(_) => self
                .list(
                    ctx,
                    &format!("\"{}\"", text.replace('"', "")),
                    "updated",
                    limit,
                )
                .await
                .unwrap_or_default(),
        };
        found.extend(issues.iter().map(|i| json!({ "title": format!("{} {}", i.identifier(), i.title), "url": format!("issues/{}", i.identifier()), "hint": crate::ui::status_name(&i.status) })));
        let org_id = ctx.org_id;
        let words = text.clone();
        let (projects, docs) = self
            .db
            .call(move |conn| {
                let projects = store::projects(conn, org_id)?;
                let docs = if words.is_empty() {
                    Vec::new()
                } else {
                    store::documents(conn, org_id, "", &words, limit)?
                };
                Ok((projects, docs))
            })
            .await?;
        found.extend(
            projects
                .iter()
                .filter(|p| p.name.to_lowercase().contains(&lower) || p.prefix.to_lowercase().contains(&lower))
                .take(limit as usize)
                .map(|p| json!({ "title": p.name, "url": format!("projects/{}", p.prefix), "hint": "project" })),
        );
        found.extend(docs.iter().map(
            |d| json!({ "title": d.title, "url": format!("docs/{}", d.id), "hint": "document" }),
        ));
        Ok(found)
    }
}
