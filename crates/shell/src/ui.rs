use axum::response::Html;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::Ctx;

const CSS: &str = "
:root{color-scheme:dark;--bg:#18191b;--surface:#222427;--ink:#eff0f1;--muted:#a3a7ad;--line:#3a3d42;--accent:#ff4a1f;--link:#ff7a57;--bad:#ff8f8f;--bad-bg:#4a2323;--warn:#f0c674;--warn-bg:#43371b;--good:#8fd6a4;--good-bg:#1f3a29}
@media(prefers-color-scheme:light){:root{color-scheme:light;--bg:#f3f3f1;--surface:#e8e8e5;--ink:#18191b;--muted:#5c6066;--line:#cfcfca;--link:#b8300c;--bad:#a61b1b;--bad-bg:#f6d9d6;--warn:#7a5600;--warn-bg:#f3e6c0;--good:#17633a;--good-bg:#d5ecdc}}
@font-face{font-family:sans;src:url(/sans.woff2) format('woff2');font-weight:400 600;font-display:swap}
@font-face{font-family:mono;src:url(/mono.woff2) format('woff2');font-weight:400 600;font-display:swap}
*{box-sizing:border-box}
body{margin:0;font:16px/1.5 sans,system-ui,sans-serif;background:var(--bg);color:var(--ink);-webkit-text-size-adjust:100%}
:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
a{color:inherit}
nav{display:flex;gap:4px;align-items:center;height:56px;padding:0 16px;border-bottom:1px solid var(--line);position:sticky;top:0;z-index:1;background:var(--bg);overflow-x:auto;white-space:nowrap}
nav a{display:flex;align-items:center;height:44px;padding:0 12px;border-radius:6px;color:var(--muted);text-decoration:none;transition:color .15s,background .15s}
nav a:hover{color:var(--ink);background:var(--surface)}
nav b{margin-right:12px}
nav form{margin-left:auto}
nav b a{padding-left:0;color:var(--ink);font-weight:600;letter-spacing:-.01em}
nav b a:hover{background:none}
nav b a::before{content:'';width:10px;height:10px;margin-right:8px;border-radius:2px;background:var(--accent)}
main{max-width:880px;margin:0 auto;padding:32px 16px 96px}
h1{font-size:28px;line-height:1.2;font-weight:600;letter-spacing:-.02em;margin:0 0 16px;overflow-wrap:anywhere}
h2{font-size:20px;line-height:1.3;font-weight:600;letter-spacing:-.01em;margin:48px 0 12px}
p{margin:0 0 16px}
main a:not(.row){color:var(--link);text-underline-offset:3px}
.row{display:block;margin:0 -12px;padding:16px 12px;border-bottom:1px solid var(--line);text-decoration:none}
a.row{transition:background .15s}
a.row:hover{background:var(--surface);border-radius:6px;border-color:transparent}
a.row:active{transform:translateY(1px)}
.row strong{display:block;font-weight:600;overflow-wrap:anywhere}
.dim,.row span,dt{color:var(--muted)}
.row span,span.dim,dt{font-size:13px}
.row>span{overflow-wrap:anywhere}
.badge{display:inline-block;padding:0 8px;border-radius:4px;background:var(--surface);font-size:13px;font-weight:500;line-height:22px}
.row .badge{color:var(--ink)}
.error,.fatal,.failure,.failed,.row .error,.row .fatal,.row .failed{color:var(--bad)}
.badge.error,.badge.fatal,.badge.failed{background:var(--bad-bg)}
.badge.warning,.row .badge.warning,.badge.uploading,.row .badge.uploading{color:var(--warn);background:var(--warn-bg)}
.badge.finished,.row .badge.finished{color:var(--good);background:var(--good-bg)}
p.error{padding:12px 16px;border-radius:6px;background:var(--bad-bg)}
.tabs{display:flex;gap:4px;margin:0 -12px 8px;border-bottom:1px solid var(--line);overflow-x:auto;white-space:nowrap}
main .tabs a{display:flex;align-items:center;height:44px;padding:0 12px;border-bottom:2px solid transparent;color:var(--muted);text-decoration:none;transition:color .15s}
main .tabs a:hover{color:var(--ink)}
main .tabs a[aria-current]{color:var(--ink);font-weight:500;border-color:var(--accent)}
.tabs .end{margin-left:auto}
pre,code{font:13px/1.6 mono,ui-monospace,monospace}
pre{overflow-x:auto;padding:12px;border-radius:6px;background:var(--surface);margin:8px 0;white-space:pre-wrap;overflow-wrap:anywhere}
.frame{padding:12px 0;border-bottom:1px solid var(--line);overflow-wrap:anywhere}
.lib{opacity:.55}
dl{display:grid;grid-template-columns:max-content 1fr;gap:8px 24px;align-items:baseline;margin:0 0 16px;padding:16px;border:1px solid var(--line);border-radius:8px}
dd{margin:0;overflow-wrap:anywhere}
form.inline{display:inline}
label{display:block;margin-bottom:16px;font-size:13px;font-weight:500;color:var(--muted)}
input,select,button{font:inherit;font-size:16px;color:var(--ink);border:1px solid var(--line);border-radius:6px}
input,select{height:48px;max-width:100%;padding:0 12px;background:var(--bg);transition:border-color .15s}
input:hover,select:hover{border-color:var(--muted)}
input::placeholder{color:var(--muted)}
label input,label select,input[type=search]{display:block;width:100%;margin-top:4px}
main>form{margin:16px 0}
main>form:first-child{margin-top:0}
form.stack{max-width:420px}
.row .inline{display:inline-block;margin-top:8px}
.inline input,.inline select{height:44px}
button,main a.button{height:44px;padding:0 24px;background:none;font-weight:500;cursor:pointer;transition:background .15s,border-color .15s,transform .1s}
button:hover,a.button:hover{background:var(--surface);border-color:var(--muted)}
button:active,a.button:active{transform:translateY(1px)}
button::first-letter{text-transform:uppercase}
button.primary{background:#d63c14;border-color:#d63c14;color:#fff}
button.primary:hover{background:#bf3410;border-color:#bf3410}
main a.button{display:flex;align-items:center;justify-content:center;max-width:420px;margin-bottom:12px;border:1px solid var(--line);border-radius:6px;color:var(--ink);text-decoration:none}
form.stack .primary{width:100%}
.or{display:flex;align-items:center;gap:12px;max-width:420px;margin:24px 0;font-size:13px;color:var(--muted)}
.or::before,.or::after{content:'';flex:1;border-top:1px solid var(--line)}
.inline button{padding:0 16px}
nav button{height:36px;padding:0 12px;font-size:14px;color:var(--muted)}
.glyph,.row .glyph{margin-right:8px;font:600 13px/1 mono,ui-monospace,monospace;color:var(--accent);white-space:pre}
summary{display:flex;gap:12px;align-items:center;min-height:44px;cursor:pointer;list-style:none}
summary::-webkit-details-marker{display:none}
summary>div{flex:1;min-width:0}
summary .glyph{width:2ch;margin:0}
details form{margin-top:16px}
details.add{margin-top:8px}
details.add summary{color:var(--muted);font-weight:500}
details.add summary:hover{color:var(--ink)}
details.add summary::before{content:'+';font:600 13px/1 mono,ui-monospace,monospace;color:var(--accent)}
details.add[open] summary::before{content:'-'}
.actions{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:12px}
h2+.dim{margin-top:-4px}
#status{position:fixed;bottom:0;left:0;right:0;display:flex;gap:24px;align-items:center;height:36px;padding:0 16px;border-top:1px solid var(--line);background:var(--surface);font:13px/1 mono,ui-monospace,monospace;color:var(--muted);white-space:nowrap;overflow:hidden;-webkit-touch-callout:none;user-select:none}
.lane{flex:1;min-width:0;height:100%;overflow:hidden}
.track{display:flex;width:max-content;height:100%}
.track>span{display:flex;gap:24px;height:100%;padding-right:48px}
.track>span+span{display:none}
.run .track>span+span{display:flex}
.run .track{animation:ticker linear infinite}
.run:hover .track,.hold .track{animation-play-state:paused}
@keyframes ticker{to{transform:translateX(-50%)}}
#tip{position:fixed;left:12px;right:12px;bottom:44px;z-index:2;padding:10px 12px;border:1px solid var(--line);border-radius:6px;background:var(--surface);font:13px/1.5 mono,ui-monospace,monospace;color:var(--muted);overflow-wrap:anywhere;box-shadow:0 4px 16px rgb(0 0 0/.25)}
#tip b:not(:first-child){margin-left:1ch}
#tip b{margin-right:1ch;color:var(--ink);font-weight:500}
#tip div{margin-top:6px;color:var(--ink)}
#status a{display:flex;gap:8px;align-items:center;height:100%;color:inherit;text-decoration:none}
#status a:hover{color:var(--ink)}
#status b{font-weight:500;color:var(--ink)}
#status .hot{color:var(--bad)}
#status .calm{color:var(--good)}
#status::after{content:'● live';margin-left:auto;color:var(--good)}
#status.off::after{content:'○ offline';color:var(--bad)}
.app nav>a{display:none}
#get-app{display:none;gap:12px;align-items:center;padding:8px 8px 8px 16px;border-bottom:1px solid var(--line);background:var(--surface);font-size:14px}
.android #get-app{display:flex}
#get-app span{flex:1}
#get-app a{font-weight:500;color:var(--link)}
#get-app button{height:36px;padding:0 12px;border:0;color:var(--muted)}
@media(max-width:600px){nav{flex-wrap:wrap;height:auto;padding:6px 16px;overflow:visible}nav::after{content:'';order:1;flex-basis:100%}nav>a{order:2;padding:0 12px 0 0}nav>a:hover{background:none}dd:last-child{margin-bottom:0}main{padding-top:24px}h1{font-size:24px}form.stack{max-width:none}form.stack button{width:100%}dl{grid-template-columns:1fr;gap:0}dd{margin-bottom:12px}}
@media(prefers-reduced-motion:reduce){*{transition:none!important}.run .track{animation:none}.lane{overflow-x:auto}}
";

const ICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%2318191b'/%3E%3Crect x='9' y='9' width='14' height='14' rx='3' fill='%23ff4a1f'/%3E%3C/svg%3E";

const IN_APP: &str = "const u=navigator.userAgent;document.documentElement.className=u.includes('chi-app')?'app':/Android/.test(u)&&!localStorage.noapp?'android':''";

const LIVE: &str = r#"const s=document.getElementById('status'),tip=document.getElementById('tip');let last,timer,held,skip=0;
function fit(){const l=s.firstChild;if(!l)return;const w=l.firstChild.firstChild.scrollWidth;s.classList.toggle('run',w>l.clientWidth);l.firstChild.style.animationDuration=w/40+'s'}
function paint(h){if(h===last)return;last=h;s.innerHTML='<div class=lane><div class=track><span>'+h+'</span><span inert>'+h+'</span></div></div>';fit()}
async function tick(){if(document.hidden||held)return;try{const r=await fetch('status');if(!r.ok)throw 0;paint(await r.text());s.classList.remove('off')}catch{s.classList.add('off')}}
function show(a){held=a;tip.replaceChildren(...a.cloneNode(true).childNodes);if(a.title&&!a.textContent.includes(a.title)){const d=document.createElement('div');d.textContent=a.title;tip.append(d)}tip.hidden=false}
function up(){clearTimeout(timer);s.classList.remove('hold');tip.hidden=true;if(held)skip=Date.now();held=null}
s.addEventListener('touchstart',e=>{const a=e.target.closest('a');if(!a)return;s.classList.add('hold');timer=setTimeout(show,450,a)},{passive:true});
s.addEventListener('touchmove',()=>held||up(),{passive:true});
s.addEventListener('touchend',up);s.addEventListener('touchcancel',up);
s.addEventListener('click',e=>{if(Date.now()-skip<800)e.preventDefault()},true);
s.addEventListener('contextmenu',e=>e.preventDefault());
addEventListener('resize',fit);tick();setInterval(tick,10000);document.addEventListener('visibilitychange',tick)"#;

pub const REPO_KINDS: [(&str, &str); 5] = [("backend", ">_"), ("mobile", "[]"), ("web", "<>"), ("library", "{}"), ("cli", "$_")];

pub fn repo_glyph(kind: &str) -> &'static str {
    REPO_KINDS.iter().find(|(name, _)| *name == kind).map_or("--", |(_, glyph)| glyph)
}

pub fn glyph(text: &str) -> Markup {
    html! { span.glyph aria-hidden="true" { (text) } }
}

fn frame(title: &str, base: &str, nav: Markup, body: Markup, tail: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width,initial-scale=1";
                    meta name="referrer" content="same-origin";
                    base href=(base);
                    link rel="icon" href=(ICON);
                    link rel="preload" href="/sans.woff2" as="font" type="font/woff2" crossorigin;
                    link rel="preload" href="/mono.woff2" as="font" type="font/woff2" crossorigin;
                    script { (PreEscaped(IN_APP)) }
                    title { (title) " · chi" }
                    style { (PreEscaped(CSS)) }
                }
                body {
                    nav { (nav) }
                    aside #get-app {
                        span { "chi has an Android app" }
                        a href="/app.apk" download { "install" }
                        button aria-label="dismiss" onclick="localStorage.noapp=1;document.documentElement.className=''" { "✕" }
                    }
                    main { (body) }
                    (tail)
                }
            }
        }
        .into_string(),
    )
}

fn logout() -> Markup {
    html! { form.inline method="post" action="/logout" { button { "sign out" } } }
}

pub fn hub_page(title: &str, signed_in: bool, body: Markup) -> Html<String> {
    frame(title, "/", html! { b { a href="/" { "chi" } } @if signed_in { (logout()) } }, body, html! {})
}

pub fn org_page(ctx: &Ctx, title: &str, body: Markup) -> Html<String> {
    frame(
        title,
        &ctx.href("/"),
        html! {
            b { a href="/" { (ctx.org_name) } }
            a href="bugs" { (glyph("[!]")) "bugs" }
            a href="builds" { (glyph("[#]")) "builds" }
            a href="settings" { (glyph("[~]")) "settings" }
            (logout())
        },
        body,
        html! { div #tip hidden {} footer #status aria-live="polite" {} script { (PreEscaped(LIVE)) } },
    )
}
