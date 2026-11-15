use maud::{Markup, PreEscaped, html};
use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};

pub fn identifiers(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let word = |i: usize| {
        bytes
            .get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    };
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_uppercase() || (i > 0 && word(i - 1)) {
            i += 1;
            continue;
        }
        let prefix = bytes[i..]
            .iter()
            .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            .count();
        let dash = i + prefix;
        let digits = bytes.get(dash + 1..).map_or(0, |rest| {
            rest.iter().take_while(|b| b.is_ascii_digit()).count()
        });
        let end = dash + 1 + digits;
        if prefix <= 10 && bytes.get(dash) == Some(&b'-') && (1..=7).contains(&digits) && !word(end)
        {
            found.push((i, end));
            i = end;
        } else {
            i += prefix.max(1);
        }
    }
    found
}

pub fn autolink(text: &str) -> Markup {
    let (mut out, mut last) = (String::new(), 0);
    for (start, end) in identifiers(text) {
        let id = &text[start..end];
        out += &html! { (text[last..start]) a href={ "issues/" (id) } { (id) } }.into_string();
        last = end;
    }
    out += &html! { (text[last..]) }.into_string();
    PreEscaped(out)
}

fn linked(text: CowStr<'_>) -> Vec<Event<'_>> {
    let found = identifiers(&text);
    if found.is_empty() {
        return vec![Event::Text(text)];
    }
    let (mut events, mut last) = (Vec::new(), 0);
    for (start, end) in found {
        let id = text[start..end].to_owned();
        events.push(Event::Text(text[last..start].to_owned().into()));
        events.push(Event::Start(Tag::Link {
            link_type: LinkType::Inline,
            dest_url: format!("issues/{id}").into(),
            title: "".into(),
            id: "".into(),
        }));
        events.push(Event::Text(id.into()));
        events.push(Event::End(TagEnd::Link));
        last = end;
    }
    events.push(Event::Text(text[last..].to_owned().into()));
    events
}

fn safe_url(url: &str) -> bool {
    let scheme_end = url.find(['/', '?', '#']).unwrap_or(url.len());
    match url[..scheme_end].find(':') {
        Some(colon) => matches!(
            &url[..colon].to_ascii_lowercase()[..],
            "http" | "https" | "mailto"
        ),
        None => true,
    }
}

pub fn markdown(text: &str) -> Markup {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut inside = 0usize;
    let events = Parser::new_ext(text, options).flat_map(|event| match event {
        Event::Start(Tag::Link { .. } | Tag::CodeBlock(_)) => {
            inside += 1;
            vec![sanitize(event)]
        }
        Event::End(TagEnd::Link | TagEnd::CodeBlock) => {
            inside = inside.saturating_sub(1);
            vec![event]
        }
        Event::Text(text) if inside == 0 => linked(text),
        other => vec![sanitize(other)],
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    PreEscaped(out)
}

fn sanitize(event: Event<'_>) -> Event<'_> {
    match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_url(&dest_url) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::Borrowed("#"),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_url(&dest_url) => Event::Start(Tag::Image {
            link_type,
            dest_url: CowStr::Borrowed("#"),
            title,
            id,
        }),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_html_and_script_links_are_neutralized() {
        let out = markdown("<script>alert(1)</script>\n\n[x](javascript:alert(1)) [y](JavaScript:x) [z](https://a.b/c:d) [w](issues/CHI-1)").into_string();
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(!out.to_lowercase().contains("javascript:"), "{out}");
        assert!(out.contains("href=\"https://a.b/c:d\""), "{out}");
        assert!(out.contains("href=\"issues/CHI-1\""), "{out}");
    }

    #[test]
    fn identifiers_are_whole_words() {
        let text = "fixes CHI-12, ABC-1 and xCHI-3 CHI-4a utf-8 A1-2 TOOLONGPREFIX-1";
        let found: Vec<&str> = identifiers(text)
            .into_iter()
            .map(|(start, end)| &text[start..end])
            .collect();
        assert_eq!(found, ["CHI-12", "ABC-1", "A1-2"]);
        let out = markdown("see CHI-12 and `CHI-13` and [CHI-14](https://x.y)").into_string();
        assert!(
            out.contains("<a href=\"issues/CHI-12\">CHI-12</a>"),
            "{out}"
        );
        assert!(
            out.contains("<code>CHI-13</code>") && out.matches("href").count() == 2,
            "{out}"
        );
        assert_eq!(
            autolink("a <b> CHI-1").into_string(),
            "a &lt;b&gt; <a href=\"issues/CHI-1\">CHI-1</a>"
        );
    }
}
