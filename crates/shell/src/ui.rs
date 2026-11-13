use axum::response::Html;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::Ctx;

const CSS: &str = "
:root{color-scheme:light dark;--line:color-mix(in srgb,currentColor 18%,transparent);--dim:color-mix(in srgb,currentColor 60%,transparent)}
*{box-sizing:border-box}
body{margin:0;font:15px/1.45 system-ui,sans-serif}
nav{display:flex;gap:14px;align-items:center;padding:10px 16px;border-bottom:1px solid var(--line);position:sticky;top:0;background:Canvas;overflow-x:auto;white-space:nowrap}
nav b{margin-right:auto}
main{max-width:900px;margin:0 auto;padding:16px}
a{color:inherit}
h1{font-size:19px;margin:0 0 12px;overflow-wrap:anywhere}
h2{font-size:15px;margin:24px 0 8px}
.row{display:block;padding:12px 0;border-bottom:1px solid var(--line);text-decoration:none}
.row strong{display:block;overflow-wrap:anywhere}
.dim,.row span{color:var(--dim);font-size:13px}
.badge{display:inline-block;padding:1px 7px;border-radius:9px;border:1px solid var(--line);font-size:12px}
.error,.fatal,.failure,.failed{color:#d33}
.tabs{display:flex;gap:12px;margin-bottom:8px}
pre,code{font:12px/1.5 ui-monospace,monospace}
pre{overflow-x:auto;padding:10px;border:1px solid var(--line);border-radius:8px;margin:6px 0;white-space:pre-wrap;overflow-wrap:anywhere}
.frame{padding:8px 0;border-bottom:1px solid var(--line);overflow-wrap:anywhere}
.lib{opacity:.5}
form.inline{display:inline}
input,select,button{font:inherit;padding:8px 10px;border-radius:8px;border:1px solid var(--line);background:Canvas;color:inherit}
form.stack input,form.stack select{display:block;width:100%;margin:6px 0}
nav button{padding:2px 8px}
dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 14px;margin:0}
dt{color:var(--dim)}dd{margin:0;overflow-wrap:anywhere}
";

fn frame(title: &str, nav: Markup, body: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width,initial-scale=1";
                    meta name="referrer" content="same-origin";
                    title { (title) " · chimera" }
                    style { (PreEscaped(CSS)) }
                }
                body { nav { (nav) } main { (body) } }
            }
        }
        .into_string(),
    )
}

fn logout() -> Markup {
    html! { form.inline method="post" action="/logout" { button { "sign out" } } }
}

pub fn hub_page(title: &str, signed_in: bool, body: Markup) -> Html<String> {
    frame(title, html! { b { a href="/" { "chimera" } } @if signed_in { (logout()) } }, body)
}

pub fn org_page(ctx: &Ctx, title: &str, body: Markup) -> Html<String> {
    frame(
        title,
        html! {
            b { a href="/" { (ctx.org_name) } }
            a href="/faults" { "faults" }
            a href="/depot" { "depot" }
            a href="/settings" { "settings" }
            (logout())
        },
        body,
    )
}
