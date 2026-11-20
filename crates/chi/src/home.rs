use std::cmp::Reverse;

use axum::{
    Extension, Router,
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::get,
};
use maud::{Markup, PreEscaped, html};
use serde::Deserialize;
use shell::{Ctx, Failure, Item, ago, find, org_page, url_encode, utc};

use crate::Apps;

const LIMIT: i64 = 50;
const TILES: [(&str, &str, bool); 5] = [
    ("my open issues", "in:issues assignee:me is:open", false),
    ("unread", "in:inbox is:unread", false),
    ("open errors", "in:errors is:open -is:dev", true),
    ("failed builds this week", "in:builds is:failed since:7d", true),
    ("running agents", "in:agents is:running", false),
];
const CHIPS: [&str; 10] = [
    "is:open",
    "assignee:me",
    "since:24h",
    "since:7d",
    "in:issues",
    "in:docs",
    "in:errors",
    "in:builds",
    "in:agents",
    "in:inbox",
];
const FEEDS: [&str; 4] = ["work", "errors", "builds", "agents"];
const RECENT: &str = "const K='chi-searches '+document.baseURI,q=(new URLSearchParams(location.search).get('q')||'').trim(),r=JSON.parse(localStorage.getItem(K)||'[]').filter(x=>x!==q);
if(q)localStorage.setItem(K,JSON.stringify([q,...r].slice(0,8)));
else if(r.length){const box=document.getElementById('recent'),h=document.createElement('h2');h.textContent='recent';box.append(h);for(const x of r){const a=document.createElement('a');a.className='row';a.href='search?q='+encodeURIComponent(x);a.textContent=x;box.append(a)}}";

pub fn routes() -> Router<Apps> {
    Router::new()
        .route("/", get(home))
        .route("/activity", get(activity))
        .route("/search", get(search))
}

fn scopes() -> impl Iterator<Item = (&'static str, &'static str)> {
    tracker::SCOPES.into_iter().chain([
        ("errors", bugs::SEARCH_HELP),
        ("builds", builds::SEARCH_HELP),
        ("agents", agents::SEARCH_HELP),
    ])
}

#[derive(Default)]
struct Outcome {
    found: Vec<(&'static str, Vec<Item>)>,
    skipped: Vec<(&'static str, String)>,
}

impl Outcome {
    fn total(&self) -> usize {
        self.found.iter().map(|(_, items)| items.len()).sum()
    }

    fn shown(&self, tab: &str, limit: i64) -> Vec<&Item> {
        let mut shown: Vec<&Item> = self
            .found
            .iter()
            .filter(|(scope, _)| tab.is_empty() || *scope == tab)
            .flat_map(|(_, items)| items)
            .collect();
        shown.sort_by_key(|item| Reverse(item.at));
        shown.truncate(limit as usize);
        shown
    }
}

fn picked(query: &str) -> Result<(Vec<&'static str>, Vec<&'static str>), Failure> {
    let (mut wanted, mut unwanted) = (Vec::new(), Vec::new());
    for term in find::tokens(query).iter().filter_map(|token| find::term(token)) {
        let values = if term.key == "in" { term.lower() } else { Vec::new() };
        for value in values {
            let scope = scopes().map(|(name, _)| name).find(|name| *name == value).ok_or_else(|| {
                let names: Vec<&str> = scopes().map(|(name, _)| name).collect();
                Failure::bad_request(&format!("no scope {value}, pick from {}", names.join(", ")))
            })?;
            if term.negated { &mut unwanted } else { &mut wanted }.push(scope);
        }
    }
    Ok((wanted, unwanted))
}

impl Apps {
    async fn find(&self, ctx: &Ctx, scope: &str, query: &str, limit: i64) -> Result<Vec<Item>, Failure> {
        match scope {
            "errors" => self.bugs.find(ctx.org_id, query, limit).await,
            "builds" => self.builds.find(ctx.org_id, query, limit).await,
            "agents" => self.agents.find(ctx.org_id, query, limit).await,
            scope => self.tracker.find(ctx, scope, query, limit).await,
        }
    }

    async fn search(&self, ctx: &Ctx, query: &str, limit: i64) -> Result<Outcome, Failure> {
        let (wanted, unwanted) = picked(query)?;
        let mut outcome = Outcome::default();
        for (scope, _) in scopes() {
            if unwanted.contains(&scope) || !(wanted.is_empty() || wanted.contains(&scope)) {
                continue;
            }
            match self.find(ctx, scope, query, limit).await {
                Ok(items) => outcome.found.push((scope, items)),
                Err(Failure(StatusCode::BAD_REQUEST, message)) => outcome.skipped.push((scope, message)),
                Err(failure) => return Err(failure),
            }
        }
        Ok(outcome)
    }

    async fn activity(&self, org_id: i64, only: &str, before: i64, limit: i64) -> Result<Vec<Item>, Failure> {
        let mut items = Vec::new();
        let wants = |feed: &str| only.is_empty() || only == feed;
        if wants("work") {
            items.extend(self.tracker.activity(org_id, before, limit).await?);
        }
        if wants("errors") {
            items.extend(self.bugs.activity(org_id, before, limit).await?);
        }
        if wants("builds") {
            items.extend(self.builds.activity(org_id, before, limit).await?);
        }
        if wants("agents") {
            items.extend(self.agents.activity(org_id, before, limit).await?);
        }
        items.sort_by_key(|item| Reverse(item.at));
        items.truncate(limit as usize);
        Ok(items)
    }
}

fn search_href(query: &str, tab: &str) -> String {
    match tab {
        "" => format!("search?q={}", url_encode(query)),
        tab => format!("search?q={}&tab={tab}", url_encode(query)),
    }
}

fn row(item: &Item) -> Markup {
    html! {
        a.row href=(item.url) {
            strong {
                @if !item.badge.is_empty() { span.badge.(item.badge) { (item.badge) } " " }
                (item.title)
            }
            span { (item.kind) @if !item.detail.is_empty() { " · " (item.detail) } " · " (ago(item.at)) }
        }
    }
}

fn count(n: usize, limit: i64) -> String {
    if n as i64 >= limit { format!("{n}+") } else { n.to_string() }
}

#[derive(Deserialize, Default)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    tab: String,
}

async fn search(
    State(apps): State<Apps>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<SearchQuery>,
) -> Result<Html<String>, Failure> {
    let q = query.q.trim();
    let (outcome, problem) = match q {
        "" => (Outcome::default(), None),
        q => match apps.search(&ctx, q, LIMIT).await {
            Ok(outcome) => (outcome, None),
            Err(Failure(StatusCode::BAD_REQUEST, message)) => (Outcome::default(), Some(message)),
            Err(failure) => return Err(failure),
        },
    };
    let shown = outcome.shown(&query.tab, LIMIT);
    let tokens = find::tokens(q);
    let toggle = |chip: &str| {
        let mut next = tokens.clone();
        match next.iter().position(|token| token == chip) {
            Some(i) => drop(next.remove(i)),
            None => next.push(chip.to_owned()),
        }
        search_href(&find::join(&next), "")
    };
    Ok(org_page(
        &ctx,
        if q.is_empty() { "search" } else { q },
        html! {
            form method="get" action="search" {
                input type="search" name="q" value=(q) autofocus aria-label="search everything"
                    placeholder="crash in:errors since:7d, or assignee:me is:open";
            }
            div.chips {
                @for chip in CHIPS {
                    a href=(toggle(chip)) aria-pressed=(tokens.iter().any(|token| token == chip)) { (chip) }
                }
            }
            @if let Some(problem) = &problem { p.error { (problem) } }
            @if !q.is_empty() && problem.is_none() {
                div.tabs data-live="issues inbox builds agents" {
                    a href=(search_href(q, "")) aria-current=[query.tab.is_empty().then_some("page")] {
                        "all " span.dim { (outcome.total()) }
                    }
                    @for (scope, items) in &outcome.found {
                        a href=(search_href(q, scope)) aria-current=[(query.tab == *scope).then_some("page")] {
                            (scope) " " span.dim { (count(items.len(), LIMIT)) }
                        }
                    }
                    form.inline.end method="post" action="pins" {
                        input type="hidden" name="name" value=(q);
                        input type="hidden" name="url" value=(search_href(q, &query.tab));
                        button { "pin" }
                    }
                }
                @if shown.is_empty() { p.dim { "Nothing matches." } }
                @for item in shown { (row(item)) }
                @if !outcome.skipped.is_empty() {
                    details {
                        summary.dim { "skipped in " (outcome.skipped.iter().map(|(scope, _)| *scope).collect::<Vec<_>>().join(", ")) }
                        dl { @for (scope, why) in &outcome.skipped { dt { (scope) } dd { (why) } } }
                    }
                }
            }
            div #recent {}
            details {
                summary { "syntax" }
                dl {
                    dt { "anywhere" }
                    dd { "in:scope and -in:scope pick where to look, " (find::TIME_HELP) ", -key: negates, key:a,b means either, other words must all appear, \"quote phrases\"" }
                    @for (scope, help) in scopes() { dt { "in:" (scope) } dd { (help) } }
                }
            }
            script { (PreEscaped(RECENT)) }
        },
    ))
}

#[derive(Deserialize, Default)]
struct FeedQuery {
    #[serde(default)]
    only: String,
    before: Option<i64>,
}

fn feed(items: &[Item]) -> Markup {
    let day = |item: &Item| utc(item.at)[..10].to_owned();
    html! {
        @for (i, item) in items.iter().enumerate() {
            @if i == 0 || day(&items[i - 1]) != day(item) { h2.day { (day(item)) } }
            (row(item))
        }
    }
}

async fn activity(
    State(apps): State<Apps>,
    Extension(ctx): Extension<Ctx>,
    Query(query): Query<FeedQuery>,
) -> Result<Html<String>, Failure> {
    if !(query.only.is_empty() || FEEDS.contains(&query.only.as_str())) {
        return Err(Failure::not_found());
    }
    let before = query.before.unwrap_or(i64::MAX);
    let items = apps.activity(ctx.org_id, &query.only, before, LIMIT).await?;
    Ok(org_page(
        &ctx,
        "activity",
        html! {
            div.tabs data-live="issues builds agents" {
                a href="activity" aria-current=[query.only.is_empty().then_some("page")] { "all" }
                @for name in FEEDS {
                    a href={ "activity?only=" (name) } aria-current=[(query.only == name).then_some("page")] { (name) }
                }
            }
            @if items.is_empty() { p.dim { "Nothing happened yet." } }
            (feed(&items))
            @if items.len() as i64 == LIMIT {
                @if let Some(last) = items.last() {
                    p { a href={ "activity?only=" (query.only) "&before=" (last.at) } { "older" } }
                }
            }
        },
    ))
}

async fn home(
    State(apps): State<Apps>,
    Extension(ctx): Extension<Ctx>,
) -> Result<Html<String>, Failure> {
    let mut tiles = Vec::new();
    for (name, q, alarming) in TILES {
        let n = apps.search(&ctx, q, 100).await?.total();
        tiles.push((name, q, alarming && n > 0, n));
    }
    let mut pins = Vec::new();
    for pin in apps.shell.pins(&ctx).await? {
        let saved = pin
            .url
            .strip_prefix("search?")
            .and_then(|rest| format!("/?{rest}").parse().ok())
            .and_then(|uri| Query::<SearchQuery>::try_from_uri(&uri).ok());
        let found = match saved {
            Some(Query(saved)) if !saved.q.trim().is_empty() => match apps.search(&ctx, saved.q.trim(), 3).await {
                Ok(outcome) => outcome.shown(&saved.tab, 3).into_iter().map(row).collect(),
                Err(Failure(StatusCode::BAD_REQUEST, _)) => Vec::new(),
                Err(failure) => return Err(failure),
            },
            _ => Vec::new(),
        };
        pins.push((pin, found));
    }
    let recent = apps.activity(ctx.org_id, "", i64::MAX, 10).await?;
    Ok(org_page(
        &ctx,
        "home",
        html! {
            h1 data-live="issues inbox builds agents" { "home" }
            div.tiles {
                @for (name, q, hot, n) in &tiles {
                    a.tile.hot[*hot] href=(search_href(q, "")) {
                        b { (count(*n, 100)) } span { (name) }
                    }
                }
            }
            h2 { "pinned" }
            @if pins.is_empty() { p.dim { "Pin any page from the menu, or a search from its results." } }
            @for (pin, found) in &pins {
                div.pin {
                    div.actions {
                        a href=(pin.url) { strong { (pin.name) } }
                        form.inline method="post" action={ "pins/" (pin.id) "/delete" } { button aria-label={ "unpin " (pin.name) } { "unpin" } }
                    }
                    @for item in found { (item) }
                }
            }
            h2 { "recent" }
            (feed(&recent))
            p { a href="activity" { "all activity" } }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tile_and_chip_names_a_real_scope() {
        let names: Vec<&str> = scopes().map(|(name, _)| name).collect();
        for query in TILES.iter().map(|(_, q, _)| *q).chain(CHIPS) {
            for term in find::tokens(query).iter().filter_map(|token| find::term(token)) {
                if term.key == "in" {
                    assert!(names.contains(&term.values[0].as_str()), "{query}");
                }
            }
        }
        assert_eq!(count(50, 50), "50+");
        assert_eq!(search_href("a b", "docs"), "search?q=a%20b&tab=docs");
    }
}
