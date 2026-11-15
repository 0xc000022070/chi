use rusqlite::types::Value as Sql;
use serde_json::{Value, json};
use shell::{Ctx, Failure, Role, utc};

use crate::{Changes, OPEN, PRIORITIES, People, STATUSES, Tracker, pages::ProjectInput, store};

type Tool = (
    &'static str,
    &'static str,
    Value,
    &'static [&'static str],
    bool,
);

pub fn tools() -> Vec<Tool> {
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": 250, "default": 50 });
    let key = json!({ "type": "string", "description": "issue id such as CHI-12" });
    let text = json!({ "type": "string" });
    let id = json!({ "type": "integer" });
    let issue_fields = json!({
        "id": { "type": "string", "description": "issue to update, such as CHI-12; omit to create" },
        "title": text,
        "description": { "type": "string", "description": "Markdown; @handle mentions notify people and ask agents" },
        "status": { "type": "string", "enum": STATUSES },
        "priority": { "type": "string", "enum": PRIORITIES },
        "estimate": { "type": "integer", "minimum": 0 },
        "assignee": { "type": "string", "description": "handle, email, me, claude, codex, or empty to clear" },
        "project": { "type": "string", "description": "project prefix or name; required on create" },
        "milestone": { "type": "string", "description": "milestone name in the issue's project, empty to clear" },
        "parent": { "type": "string", "description": "parent issue id, empty to clear" },
        "due_date": { "type": "string", "description": "YYYY-MM-DD, empty to clear" },
        "labels": { "type": "array", "items": { "type": "string" }, "description": "the whole label set; new labels are created" },
    });
    vec![
        (
            "list_issues",
            "Issues matching a filter such as `is:open assignee:me label:bug project:CHI priority:high status:todo parent:CHI-3 sort:priority` plus free text. Viewers too.",
            json!({ "query": { "type": "string", "default": "is:open" }, "order": { "type": "string", "enum": ["updated", "created", "priority", "due", "manual"] }, "limit": limit }),
            &[],
            true,
        ),
        (
            "get_issue",
            "One issue with its sub-issues, relations, attachments, pull requests and commits. Comments come from list_comments.",
            json!({ "id": key }),
            &["id"],
            true,
        ),
        (
            "save_issue",
            "Creates an issue, or updates the given fields of one when id is set. Maintainers only.",
            issue_fields,
            &[],
            false,
        ),
        (
            "list_statuses",
            "Issue statuses in workflow order and priorities from none to low.",
            json!({}),
            &[],
            true,
        ),
        (
            "list_comments",
            "Comments of an issue, oldest first.",
            json!({ "issue": key }),
            &["issue"],
            true,
        ),
        (
            "save_comment",
            "Posts a comment, or edits (body) or deletes (delete) comment id. Mentioning @claude or @codex asks that agent to answer.",
            json!({ "issue": key, "id": id, "body": text, "delete": { "type": "boolean" } }),
            &[],
            false,
        ),
        (
            "save_relation",
            "Relates two issues, or removes the relation.",
            json!({ "issue": key, "kind": { "type": "string", "enum": store::RELATIONS }, "other": key, "remove": { "type": "boolean" } }),
            &["issue", "kind", "other"],
            false,
        ),
        (
            "create_attachment",
            "Attaches a link to an issue.",
            json!({ "issue": key, "url": text, "title": text }),
            &["issue", "url"],
            false,
        ),
        (
            "upload_file",
            "Attaches a file of up to 25 MB, given as base64, to an issue.",
            json!({ "issue": key, "name": text, "mime": { "type": "string", "default": "application/octet-stream" }, "base64": text }),
            &["issue", "name", "base64"],
            false,
        ),
        (
            "issue_from_error",
            "The issue tracking an error group from list_errors, created on first call.",
            json!({ "error": id }),
            &["error"],
            false,
        ),
        (
            "list_projects",
            "Projects with their issue prefix, lead, status, repository and progress.",
            json!({}),
            &[],
            true,
        ),
        (
            "get_project",
            "One project with its milestones and documents.",
            json!({ "project": text }),
            &["project"],
            true,
        ),
        (
            "save_project",
            "Creates a project, or updates project id. The prefix starts every issue id, such as CHI in CHI-12.",
            json!({
                "id": { "type": "string", "description": "prefix of the project to update; omit to create" },
                "prefix": text, "name": text, "description": text,
                "status": { "type": "string", "enum": store::PROJECT_STATUSES },
                "lead": text, "target_date": text,
                "repo": { "type": "string", "description": "repository id, name or url; empty to clear" },
            }),
            &[],
            false,
        ),
        (
            "list_milestones",
            "Milestones of a project with their progress.",
            json!({ "project": text }),
            &["project"],
            true,
        ),
        (
            "save_milestone",
            "Creates a milestone in a project, or updates milestone id.",
            json!({ "project": text, "id": id, "name": text, "description": text, "target_date": text }),
            &["project", "name"],
            false,
        ),
        (
            "list_documents",
            "Documents, optionally of one project or matching text.",
            json!({ "project": text, "query": text, "limit": limit }),
            &[],
            true,
        ),
        (
            "get_document",
            "One document with its Markdown body.",
            json!({ "id": id }),
            &["id"],
            true,
        ),
        (
            "save_document",
            "Creates a document, or updates document id.",
            json!({ "id": id, "title": text, "body": text, "project": { "type": "string", "description": "prefix, empty to detach" } }),
            &[],
            false,
        ),
        (
            "list_labels",
            "Labels with their color and open issue count.",
            json!({}),
            &[],
            true,
        ),
        (
            "save_label",
            "Creates a label, or renames or recolors label name.",
            json!({ "name": text, "new_name": text, "color": { "type": "string", "description": "#rrggbb" } }),
            &["name"],
            false,
        ),
        (
            "list_users",
            "Members and agents, by the handle that assignee and @mentions use.",
            json!({}),
            &[],
            true,
        ),
        ("list_templates", "Issue templates.", json!({}), &[], true),
        (
            "list_views",
            "Saved views: your own and the shared ones, each a list_issues query.",
            json!({}),
            &[],
            true,
        ),
        (
            "list_notifications",
            "Your inbox, newest first.",
            json!({ "unread": { "type": "boolean", "default": true }, "limit": limit }),
            &[],
            true,
        ),
        (
            "mark_notification",
            "Marks notification id, or all of them, read or unread.",
            json!({ "id": id, "read": { "type": "boolean", "default": true } }),
            &[],
            false,
        ),
        (
            "search",
            "Issues, comments, projects and documents matching text.",
            json!({ "query": text, "limit": limit }),
            &["query"],
            true,
        ),
    ]
}

fn issue_json(issue: &store::Issue, who: &People, url: String) -> Value {
    json!({
        "id": issue.identifier(),
        "title": issue.title,
        "description": issue.description,
        "status": issue.status,
        "priority": issue.priority_name(),
        "estimate": issue.estimate,
        "assignee": who.get(issue.assignee).map(|p| p.handle()),
        "creator": who.get(issue.creator).map(|p| p.handle()),
        "project": issue.prefix,
        "milestone": issue.milestone,
        "parent": issue.parent,
        "labels": issue.labels,
        "due_date": issue.due_date,
        "error": issue.error_ref,
        "created_at": utc(issue.created_at),
        "updated_at": utc(issue.updated_at),
        "completed_at": issue.completed_at.map(utc),
        "url": url,
    })
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in text.bytes().filter(|b| !b.is_ascii_whitespace()) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = buffer << 6 | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn field<T: serde::de::DeserializeOwned>(args: &Value, name: &str) -> Result<Option<T>, Failure> {
    match &args[name] {
        Value::Null => Ok(None),
        value => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|e| Failure::bad_request(&format!("{name}: {e}"))),
    }
}

fn text(args: &Value, key: &str) -> String {
    args[key].as_str().unwrap_or("").to_owned()
}

fn needed(args: &Value, key: &str) -> Result<String, Failure> {
    args[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Failure::bad_request(&format!("{key} is required")))
}

fn number(args: &Value, key: &str) -> Result<i64, Failure> {
    args[key]
        .as_i64()
        .ok_or_else(|| Failure::bad_request(&format!("{key} is required")))
}

fn limit(args: &Value) -> i64 {
    args["limit"].as_i64().unwrap_or(50).clamp(1, 250)
}

fn handle(who: &People, user: Option<i64>) -> Option<String> {
    who.get(user).map(|p| p.handle().to_owned())
}

fn or_current(args: &Value, key: &str, current: Option<String>) -> Option<String> {
    args[key].as_str().map(str::to_owned).or(current)
}

impl Tracker {
    fn page_url(&self, ctx: &Ctx, path: &str) -> String {
        self.shell.url(&ctx.href(path))
    }

    fn issue_value(&self, ctx: &Ctx, issue: &store::Issue, who: &People) -> Value {
        issue_json(
            issue,
            who,
            self.page_url(ctx, &format!("/issues/{}", issue.identifier())),
        )
    }

    pub async fn call(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
    ) -> Option<Result<Value, Failure>> {
        if !tools().iter().any(|tool| tool.0 == name) {
            return None;
        }
        Some(self.run(ctx, name, args).await)
    }

    async fn run(&self, ctx: &Ctx, name: &str, args: &Value) -> Result<Value, Failure> {
        let who = self.people(ctx.org_id).await?;
        if let Some(found) = self.issue_tool(ctx, name, args, &who).await? {
            return Ok(found);
        }
        if let Some(found) = self.comment_tool(ctx, name, args, &who).await? {
            return Ok(found);
        }
        if let Some(found) = self.attachment_tool(ctx, name, args).await? {
            return Ok(found);
        }
        if let Some(found) = self.project_tool(ctx, name, args, &who).await? {
            return Ok(found);
        }
        if let Some(found) = self.document_tool(ctx, name, args, &who).await? {
            return Ok(found);
        }
        self.people_tool(ctx, name, args, &who).await
    }

    async fn issue_tool(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
        who: &People,
    ) -> Result<Option<Value>, Failure> {
        Ok(Some(match name {
            "list_issues" => {
                let query = args["query"].as_str().unwrap_or("is:open");
                let order = args["order"].as_str().unwrap_or("updated");
                let issues = self.list(ctx, query, order, limit(args)).await?;
                issues
                    .iter()
                    .map(|issue| self.issue_value(ctx, issue, who))
                    .collect()
            }
            "get_issue" => self.get_issue(ctx, &needed(args, "id")?, who).await?,
            "save_issue" => {
                let changes: Changes = serde_json::from_value(args.clone())
                    .map_err(|e| Failure::bad_request(&e.to_string()))?;
                let issue = self.save(ctx, args["id"].as_str(), changes).await?;
                self.issue_value(ctx, &issue, who)
            }
            "issue_from_error" => {
                let issue = self.from_error(ctx, number(args, "error")?).await?;
                self.issue_value(ctx, &issue, who)
            }
            "list_statuses" => {
                json!({ "statuses": STATUSES, "open": OPEN, "priorities": PRIORITIES })
            }
            _ => return Ok(None),
        }))
    }

    async fn get_issue(&self, ctx: &Ctx, key: &str, who: &People) -> Result<Value, Failure> {
        let org_id = ctx.org_id;
        let issue = self.issue(ctx, key).await?;
        let issue_id = issue.id;
        let (children, relations, attachments, links) = self
            .db
            .call(move |conn| {
                Ok((
                    store::issues(
                        conn,
                        org_id,
                        "i.parent_id = ?",
                        vec![Sql::from(issue_id)],
                        "i.sort, i.id",
                        500,
                    )?,
                    store::relations(conn, issue_id)?,
                    store::attachments(conn, issue_id)?,
                    store::links(conn, issue_id)?,
                ))
            })
            .await?;
        let mut found = self.issue_value(ctx, &issue, who);
        found["sub_issues"] = children.iter().map(|child| json!({ "id": child.identifier(), "title": child.title, "status": child.status })).collect();
        found["relations"] = relations.iter().map(|r| json!({ "kind": r.label, "issue": r.other, "title": r.title, "status": r.status })).collect();
        found["attachments"] = attachments
            .iter()
            .map(|a| {
                let url = if a.file.is_empty() { a.url.clone() } else { self.page_url(ctx, &format!("/files/{}", a.id)) };
                json!({ "title": a.title, "url": url, "mime": a.mime, "size": a.size, "by": handle(who, a.uploader), "at": utc(a.created_at) })
            })
            .collect();
        found["links"] = links.iter().map(|l| json!({ "kind": l.kind, "title": l.title, "url": l.url, "state": l.state, "at": utc(l.at) })).collect();
        Ok(found)
    }

    async fn comment_tool(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
        who: &People,
    ) -> Result<Option<Value>, Failure> {
        Ok(Some(match name {
            "list_comments" => {
                let issue_id = self.issue(ctx, &needed(args, "issue")?).await?.id;
                let comments = self
                    .db
                    .call(move |conn| store::comments(conn, issue_id))
                    .await?;
                comments.iter().map(|c| json!({ "id": c.id, "author": handle(who, c.author), "body": c.body, "created_at": utc(c.created_at), "updated_at": utc(c.updated_at) })).collect()
            }
            "save_comment" => self.save_comment_tool(ctx, args).await?,
            "save_relation" => {
                let issue = self.issue(ctx, &needed(args, "issue")?).await?;
                let (kind, other) = (needed(args, "kind")?, needed(args, "other")?);
                self.relate(ctx, &issue, &kind, &other, args["remove"] == true)
                    .await?;
                json!({ "ok": true })
            }
            _ => return Ok(None),
        }))
    }

    async fn attachment_tool(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
    ) -> Result<Option<Value>, Failure> {
        Ok(Some(match name {
            "create_attachment" => {
                let issue = self.issue(ctx, &needed(args, "issue")?).await?;
                json!({ "id": self.attach_link(ctx, &issue, &text(args, "title"), &needed(args, "url")?).await? })
            }
            "upload_file" => {
                let issue = self.issue(ctx, &needed(args, "issue")?).await?;
                let bytes = base64(&needed(args, "base64")?)
                    .ok_or_else(|| Failure::bad_request("base64 is not valid base64"))?;
                let mime = args["mime"].as_str().unwrap_or("application/octet-stream");
                let id = self
                    .upload(ctx, &issue, &needed(args, "name")?, mime, bytes)
                    .await?;
                json!({ "id": id, "url": self.page_url(ctx, &format!("/files/{id}")) })
            }
            _ => return Ok(None),
        }))
    }

    async fn save_comment_tool(&self, ctx: &Ctx, args: &Value) -> Result<Value, Failure> {
        if let Some(comment) = args["id"].as_i64() {
            let body = if args["delete"] == true {
                None
            } else {
                Some(needed(args, "body")?)
            };
            return Ok(json!({ "issue": self.edit_comment(ctx, comment, body).await? }));
        }
        ctx.require(Role::Maintainer)?;
        let issue = self.issue(ctx, &needed(args, "issue")?).await?;
        let body = needed(args, "body")?;
        let id = self
            .comment(ctx.org_id, &issue, Some(ctx.user_id), &ctx.email, &body)
            .await?;
        Ok(json!({ "id": id }))
    }

    async fn project_tool(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
        who: &People,
    ) -> Result<Option<Value>, Failure> {
        let org_id = ctx.org_id;
        Ok(Some(match name {
            "list_projects" => {
                let projects = self
                    .db
                    .call(move |conn| store::projects(conn, org_id))
                    .await?;
                projects
                    .iter()
                    .map(|p| {
                        json!({ "prefix": p.prefix, "name": p.name, "status": p.status, "lead": handle(who, p.lead), "target_date": p.target_date,
                                "repo": p.repo_id, "issues": p.total, "done": p.done, "url": self.page_url(ctx, &format!("/projects/{}", p.prefix)) })
                    })
                    .collect()
            }
            "get_project" | "list_milestones" => {
                self.get_project(ctx, name, needed(args, "project")?, who)
                    .await?
            }
            "save_project" => self.save_project_tool(ctx, args).await?,
            "save_milestone" => {
                let (project, name) = (needed(args, "project")?, needed(args, "name")?);
                let id = self
                    .save_milestone(
                        ctx,
                        project,
                        args["id"].as_i64(),
                        name,
                        text(args, "description"),
                        text(args, "target_date"),
                    )
                    .await?;
                json!({ "id": id })
            }
            _ => return Ok(None),
        }))
    }

    async fn save_project_tool(&self, ctx: &Ctx, args: &Value) -> Result<Value, Failure> {
        let input = ProjectInput {
            prefix: field(args, "prefix")?,
            name: field(args, "name")?,
            description: field(args, "description")?,
            status: field(args, "status")?,
            lead: field(args, "lead")?,
            target_date: field(args, "target_date")?,
            repo: field(args, "repo")?,
        };
        let fields = self.project_fields(ctx, input).await?;
        let prefix = self.save_project(ctx, field(args, "id")?, fields).await?;
        Ok(json!({ "prefix": prefix, "url": self.page_url(ctx, &format!("/projects/{prefix}")) }))
    }

    async fn get_project(
        &self,
        ctx: &Ctx,
        name: &str,
        project: String,
        who: &People,
    ) -> Result<Value, Failure> {
        let org_id = ctx.org_id;
        let found = self
            .db
            .call(move |conn| {
                let Some(project) = store::projects(conn, org_id)?.into_iter().find(|p| {
                    p.prefix.eq_ignore_ascii_case(&project) || p.name.eq_ignore_ascii_case(&project)
                }) else {
                    return Ok(None);
                };
                let milestones = store::milestones(conn, project.id)?;
                let docs = store::documents(conn, org_id, &project.prefix, "", 100)?;
                Ok(Some((project, milestones, docs)))
            })
            .await?;
        let (project, milestones, docs) = found.ok_or_else(Failure::not_found)?;
        let milestones: Value = milestones
            .iter()
            .map(|m| json!({ "id": m.id, "name": m.name, "description": m.description, "target_date": m.target_date, "issues": m.total, "done": m.done }))
            .collect();
        if name == "list_milestones" {
            return Ok(milestones);
        }
        let repo = match project.repo_id {
            Some(repo) => self.shell.repo(org_id, repo).await?.map(|repo| repo.url),
            None => None,
        };
        Ok(json!({
            "prefix": project.prefix, "name": project.name, "description": project.description, "status": project.status,
            "lead": handle(who, project.lead), "target_date": project.target_date, "repo": repo, "issues": project.total, "done": project.done,
            "milestones": milestones,
            "documents": docs.iter().map(|d| json!({ "id": d.id, "title": d.title, "updated_at": utc(d.updated_at) })).collect::<Value>(),
            "url": self.page_url(ctx, &format!("/projects/{}", project.prefix)),
        }))
    }

    async fn document_tool(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
        who: &People,
    ) -> Result<Option<Value>, Failure> {
        let org_id = ctx.org_id;
        Ok(Some(match name {
            "list_documents" => {
                let (project, query, limit) =
                    (text(args, "project"), text(args, "query"), limit(args));
                let docs = self
                    .db
                    .call(move |conn| store::documents(conn, org_id, &project, &query, limit))
                    .await?;
                docs.iter().map(|d| json!({ "id": d.id, "title": d.title, "project": d.project, "author": handle(who, d.author), "updated_at": utc(d.updated_at) })).collect()
            }
            "get_document" => {
                let doc = self.document(org_id, number(args, "id")?).await?;
                json!({ "id": doc.id, "title": doc.title, "body": doc.body, "project": doc.project, "author": handle(who, doc.author),
                        "created_at": utc(doc.created_at), "updated_at": utc(doc.updated_at), "url": self.page_url(ctx, &format!("/docs/{}", doc.id)) })
            }
            "save_document" => self.save_document_tool(ctx, args).await?,
            "list_labels" => {
                let labels = self
                    .db
                    .call(move |conn| store::labels(conn, org_id))
                    .await?;
                labels
                    .iter()
                    .map(|l| json!({ "name": l.name, "color": l.color, "open": l.open }))
                    .collect()
            }
            "save_label" => {
                let (label, new_name, color) = (
                    needed(args, "name")?,
                    args["new_name"].as_str().map(str::to_owned),
                    args["color"].as_str().map(str::to_owned),
                );
                self.change(ctx, "issues", move |conn| {
                    store::save_label(conn, org_id, &label, new_name.as_deref(), color.as_deref())
                })
                .await?;
                json!({ "ok": true })
            }
            _ => return Ok(None),
        }))
    }

    async fn document(&self, org_id: i64, id: i64) -> Result<store::Document, Failure> {
        self.db
            .call(move |conn| store::document(conn, org_id, id))
            .await?
            .ok_or_else(Failure::not_found)
    }

    async fn save_document_tool(&self, ctx: &Ctx, args: &Value) -> Result<Value, Failure> {
        let id = args["id"].as_i64();
        let current = match id {
            Some(id) => Some(self.document(ctx.org_id, id).await?),
            None => None,
        };
        let title = or_current(args, "title", current.as_ref().map(|d| d.title.clone()));
        let body = or_current(args, "body", current.as_ref().map(|d| d.body.clone()));
        let project = or_current(args, "project", current.map(|d| d.project));
        let id = self
            .save_document(
                ctx,
                id,
                title.unwrap_or_default(),
                body.unwrap_or_default(),
                project,
            )
            .await?;
        Ok(json!({ "id": id, "url": self.page_url(ctx, &format!("/docs/{id}")) }))
    }

    async fn people_tool(
        &self,
        ctx: &Ctx,
        name: &str,
        args: &Value,
        who: &People,
    ) -> Result<Value, Failure> {
        let (org_id, user) = (ctx.org_id, ctx.user_id);
        Ok(match name {
            "list_users" => who
                .0
                .iter()
                .map(|p| json!({ "handle": p.handle(), "email": p.email, "agent": p.agent }))
                .collect(),
            "list_templates" => {
                let templates = self
                    .db
                    .call(move |conn| store::templates(conn, org_id))
                    .await?;
                templates.iter().map(|t| json!({ "id": t.id, "name": t.name, "title": t.title, "description": t.description, "fields": t.fields })).collect()
            }
            "list_views" => {
                let views = self
                    .db
                    .call(move |conn| store::views(conn, org_id, user))
                    .await?;
                views.iter().map(|v| json!({ "id": v.id, "name": v.name, "query": v.query, "layout": v.layout, "shared": v.shared, "owner": handle(who, Some(v.owner)) })).collect()
            }
            "list_notifications" => {
                let (unread, limit) = (args["unread"] != false, limit(args));
                let items = self
                    .db
                    .call(move |conn| store::notifications(conn, org_id, user, unread, limit))
                    .await?;
                items
                    .iter()
                    .map(|n| json!({ "id": n.id, "issue": n.issue, "title": n.title, "by": handle(who, n.actor), "text": n.text, "at": utc(n.created_at), "read": n.read }))
                    .collect()
            }
            "mark_notification" => {
                self.mark(ctx, args["id"].as_i64(), args["read"] != false)
                    .await?;
                json!({ "ok": true })
            }
            "search" => Value::Array(
                self.search(ctx, &needed(args, "query")?, limit(args))
                    .await?,
            ),
            _ => return Err(Failure::not_found()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_padded_unpadded_and_wrapped() {
        assert_eq!(
            base64("aGVsbG8gd29ybGQ=").as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(
            base64("aGVsbG8gd29ybGQ").as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(base64("aGVs\nbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64("+/8=").as_deref(), Some(&[0xfb, 0xff][..]));
        assert_eq!(base64("a*b"), None);
    }

    #[test]
    fn tool_names_are_unique_and_tracker_owned() {
        let names: std::collections::HashSet<&str> = tools().iter().map(|tool| tool.0).collect();
        assert_eq!(names.len(), tools().len());
        assert!(!names.contains("list_errors") && !names.contains("list_orgs"));
    }
}
