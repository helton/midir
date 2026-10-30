//! HTML reduced to the text an agent can read: the main content (`<main>`, `<article>`, `role="main"`, else the
//! body) without scripts, styles, navigation and decoration, block tags as line breaks, entities decoded. A small
//! hand-written scanner, not a DOM: pages are untrusted input and the output only has to read well.

/// The page's title and its text.
pub fn to_text(html: &str) -> (Option<String>, String) {
    let title = between_tags(html, "title").map(|t| collapse(&decode(&strip_tags(t)))).filter(|t| !t.is_empty());
    let content = main_content(html);
    (title, render(content))
}

/// The text between `<tag ...>` and `</tag>` (first occurrence, case-insensitive).
fn between_tags<'a>(html: &'a str, tag: &str) -> Option<&'a str> {
    let open = find_tag_open(html, tag, 0)?;
    let body_start = html[open..].find('>')? + open + 1;
    let close = find_close(html, tag, body_start)?;
    Some(&html[body_start..close])
}

/// The byte offset of `<tag` (as a whole tag name) at or after `from`.
fn find_tag_open(html: &str, tag: &str, from: usize) -> Option<usize> {
    let mut i = from;
    let lower = tag.as_bytes();
    while let Some(k) = html[i..].find('<') {
        let at = i + k;
        let rest = &html.as_bytes()[at + 1..];
        if rest.len() >= lower.len()
            && rest[..lower.len()].eq_ignore_ascii_case(lower)
            && rest.get(lower.len()).is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// The byte offset of `</tag>` matching an open tag whose content starts at `from` (nested tags of the same name are
/// skipped); None when it never closes.
fn find_close(html: &str, tag: &str, from: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = from;
    let lower = tag.as_bytes();
    while let Some(k) = html[i..].find('<') {
        let at = i + k;
        let rest = &html.as_bytes()[at + 1..];
        let (name_at, closing) = if rest.first() == Some(&b'/') { (&rest[1..], true) } else { (rest, false) };
        if name_at.len() >= lower.len()
            && name_at[..lower.len()].eq_ignore_ascii_case(lower)
            && name_at.get(lower.len()).is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            if closing {
                if depth == 0 {
                    return Some(at);
                }
                depth -= 1;
            } else if !html[at..].split_once('>').is_some_and(|(open, _)| open.ends_with('/')) {
                depth += 1;
            }
        }
        i = at + 1;
    }
    None
}

/// `<main>`, `<article>` or `role="main"`, else `<body>`, else the whole document.
fn main_content(html: &str) -> &str {
    for tag in ["main", "article"] {
        if let Some(inner) = between_tags(html, tag)
            && inner.len() > 200
        {
            return inner;
        }
    }
    if let Some(at) = html.find("role=\"main\"").or_else(|| html.find("role='main'"))
        && let Some(open) = html[..at].rfind('<')
    {
        let name: String = html[open + 1..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if let Some(body_start) = html[open..].find('>').map(|k| open + k + 1)
            && let Some(close) = find_close(html, &name, body_start)
        {
            return &html[body_start..close];
        }
    }
    between_tags(html, "body").unwrap_or(html)
}

/// Elements whose content is never text for a reader.
const DROP: [&str; 9] = ["script", "style", "noscript", "svg", "template", "nav", "header", "footer", "aside"];
/// Elements that start a new line.
const BLOCK: [&str; 24] = [
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "pre",
    "tr",
    "section",
    "article",
    "table",
    "blockquote",
    "dd",
    "dt",
    "hr",
    "form",
    "figure",
    "details",
];

/// Tags removed, blocks as line breaks, `<pre>` kept as written, entities decoded, whitespace collapsed.
fn render(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut i = 0;
    let mut pre_depth = 0usize;
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut String, pre: bool| {
        if text.is_empty() {
            return;
        }
        let decoded = decode(text);
        if pre {
            out.push_str(&decoded);
        } else {
            let c = collapse(&decoded);
            if !c.is_empty() {
                if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(&c);
            }
        }
        text.clear();
    };
    while i < html.len() {
        let Some(k) = html[i..].find('<') else {
            text.push_str(&html[i..]);
            break;
        };
        text.push_str(&html[i..i + k]);
        let at = i + k;
        let rest = &html[at..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(html.len(), |e| at + e + 3);
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[1..end];
        let closing = tag.starts_with('/');
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
        if !closing && DROP.contains(&name.as_str()) {
            flush(&mut text, &mut out, pre_depth > 0);
            i = find_close(html, &name, at + end + 1).map_or(html.len(), |c| html[c..].find('>').map_or(html.len(), |e| c + e + 1));
            continue;
        }
        if BLOCK.contains(&name.as_str()) {
            flush(&mut text, &mut out, pre_depth > 0);
            if name == "pre" {
                pre_depth = if closing { pre_depth.saturating_sub(1) } else { pre_depth + 1 };
            }
            if !closing && name == "li" {
                newlines(&mut out, 1);
                out.push_str("- ");
            } else if matches!(name.as_str(), "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "table" | "blockquote" | "section")
                && !closing
            {
                newlines(&mut out, 2);
            } else if name == "br" && !closing {
                // every <br> is a line break, even several in a row
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push('\n');
            } else if !closing || name != "br" {
                newlines(&mut out, 1);
            }
        } else if !closing && (name == "td" || name == "th") {
            flush(&mut text, &mut out, pre_depth > 0);
            if !out.is_empty() && !out.ends_with('\n') {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push_str("  ");
            }
        }
        i = at + end + 1;
    }
    flush(&mut text, &mut out, pre_depth > 0);
    squeeze_newlines(out.trim())
}

/// The output ends with at least `n` newlines (none at its very start).
fn newlines(out: &mut String, n: usize) {
    while out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    let have = out.chars().rev().take_while(|c| *c == '\n').count();
    for _ in have..n {
        out.push('\n');
    }
}

fn squeeze_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push(c);
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' if inside => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whitespace runs as one space.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The named entities worth knowing and the numeric ones; anything else stays as written.
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(semi) = after.find(';').filter(|n| *n <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };
        let name = &after[..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "copy" => Some('©'),
            "laquo" => Some('«'),
            "raquo" => Some('»'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            "middot" => Some('·'),
            "bull" => Some('•'),
            "reg" => Some('®'),
            "trade" => Some('™'),
            "times" => Some('×'),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_content_wins_over_navigation() {
        let html = "<html><head><title>Doc &amp; Co</title><style>p{}</style></head><body><nav><a>Home</a><a>Docs</a></nav>\
                    <main><h1>Mutex</h1><p>A mutual  exclusion\n primitive.</p><ul><li>one</li><li>two &lt;3</li></ul>\
                    <pre>fn x() {\n    1\n}</pre><script>alert(1)</script></main><footer>(c) 2026</footer></body></html>";
        let (title, text) = to_text(&html.replace("<main>", &format!("<main>{}", "x".repeat(200))));
        assert_eq!(title.as_deref(), Some("Doc & Co"));
        assert!(!text.contains("Home") && !text.contains("alert") && !text.contains("2026") && !text.contains("p{}"), "{text}");
        assert!(text.contains("Mutex\n\nA mutual exclusion primitive.\n- one\n- two <3"), "{text}");
        assert!(text.contains("fn x() {\n    1\n}"), "{text}");
    }

    #[test]
    fn a_page_without_main_falls_back_to_the_body() {
        let (title, text) = to_text("<body><div>Hello<br>world</div><div>Bye</div></body>");
        assert_eq!(title, None);
        assert_eq!(text, "Hello\nworld\nBye");
    }

    #[test]
    fn nested_dropped_elements_and_comments() {
        let html = "<body><div><header><div>deep</div>nav</header><!-- hidden <p>x</p> -->kept &#169; &#x41; &unknown;</div></body>";
        assert_eq!(to_text(html).1, "kept © A &unknown;");
        assert_eq!(to_text("<body>a &middot; b &bull; c&trade;</body>").1, "a · b • c™");
    }

    #[test]
    fn tables_read_as_rows() {
        let html = "<body><table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table></body>";
        assert_eq!(to_text(html).1, "a  b\n1  2");
    }

    #[test]
    fn anything_is_safe() {
        for junk in ["<", "<<<>>>", "<p", "</", "<main>", "<pre><pre></pre>", "&#99999999999;", "<script>never closed"] {
            let _ = to_text(junk);
        }
    }
}
