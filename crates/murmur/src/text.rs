//! Message-body text sanitization + autolinking for the server-rendered timeline.
//!
//! SECURITY: message bodies are author-supplied, so they are treated as PLAIN TEXT, never markup.
//! [`render_body`] tokenizes the raw plain text, HTML-escapes every literal span, and emits only a
//! fixed formatting whitelist (`pre code strong em del blockquote span.mention a`). Because every
//! user-controlled text/attribute fragment routes through [`esc`] and links stay scheme-restricted
//! to `http`/`https`, no user input ever reaches the page as live HTML. The dashboard's live-update
//! JS mirrors this renderer using DOM text nodes.

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Parse the distinct `@username` mentions out of a message body.
///
/// A mention is an `@` that begins a token — i.e. `@` at the START of the body or immediately
/// after a NON-alphanumeric, non-(`.`/`_`/`-`/`@`) character — followed by a run of
/// `[A-Za-z0-9._-]`. Requiring the preceding character to be a boundary means an email like
/// `alice@example.com` never yields a bogus `@example` mention (the `@` there is preceded by the
/// alphanumeric `e`). Tokens are lowercased and de-duplicated, preserving first-seen order.
pub fn parse_mentions(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            // The char before the '@' must be a token boundary (start-of-body counts).
            let boundary = match i.checked_sub(1).map(|p| chars[p]) {
                None => true,
                Some(prev) => !is_mention_char(prev) && prev != '@',
            };
            if boundary {
                let mut j = i + 1;
                while j < chars.len() && is_mention_char(chars[j]) {
                    j += 1;
                }
                if j > i + 1 {
                    let token: String = chars[i + 1..j].iter().collect::<String>().to_lowercase();
                    if !out.contains(&token) {
                        out.push(token);
                    }
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Characters permitted inside an `@mention` token (matches a username / email local-part shape).
fn is_mention_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Render a message body to SAFE HTML: escape all literal text, then emit fixed formatting tags.
/// Newlines in normal text become `<br>`; fenced code blocks preserve raw newlines inside `<pre>`.
/// The result is safe to interpolate directly into the page.
pub fn render_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 16);
    let lines: Vec<&str> = body.split('\n').collect();
    let mut normal = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(lang) = fence_lang(lines[i]) {
            if let Some(close) = find_fence_close(&lines, i + 1) {
                flush_normal(&mut normal, &mut out);
                emit_fence(&lines[i + 1..close], lang, &mut out);
                i = close + 1;
                continue;
            }
        }
        normal.push(lines[i]);
        i += 1;
    }
    flush_normal(&mut normal, &mut out);
    out
}

/// Plain one-line preview for compact quote/chrome surfaces: no rich tags, no autolinks.
pub fn preview_text(body: &str) -> String {
    let mut lines = Vec::new();
    for line in body.split('\n') {
        if fence_lang(line).is_some() || is_fence_close(line) {
            continue;
        }
        lines.push(line.trim_end_matches('\r'));
    }
    lines.join(" ")
}

/// Safe HTML form of [`preview_text`], escaped exactly once for direct sink interpolation.
pub fn render_preview(body: &str) -> String {
    esc(&preview_text(body))
}

fn flush_normal(lines: &mut Vec<&str>, out: &mut String) {
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("<br>");
        }
        render_normal_line(line, out);
    }
    lines.clear();
}

fn render_normal_line(line: &str, out: &mut String) {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("> ") {
        out.push_str("<blockquote>");
        render_inline(rest, out);
        out.push_str("</blockquote>");
    } else {
        render_inline(line, out);
    }
}

fn render_inline(line: &str, out: &mut String) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    while i < line.len() {
        let ch = line[i..].chars().next().unwrap();
        if ch == '`' {
            if let Some(j) = find_next_char(line, i + 1, '`') {
                if j > i + 1 {
                    out.push_str(&esc(&line[text_start..i]));
                    out.push_str("<code>");
                    out.push_str(&esc(&line[i + 1..j]));
                    out.push_str("</code>");
                    i = j + 1;
                    text_start = i;
                    continue;
                }
            }
        }
        if starts_url(&line[i..]) {
            // Flush the escaped plain text before the URL.
            out.push_str(&esc(&line[text_start..i]));
            // Consume the URL up to the first whitespace or character that cannot be in a URL.
            let mut j = i;
            while j < line.len() && is_url_byte(bytes[j]) {
                j += 1;
            }
            // Trailing punctuation that is almost never part of the URL itself.
            let url = trim_url_trailing(&line[i..j]);
            let url_escaped = esc(url);
            out.push_str(&format!(
                "<a href=\"{u}\" rel=\"noopener noreferrer nofollow\" target=\"_blank\">{u}</a>",
                u = url_escaped
            ));
            i += url.len();
            text_start = i;
            continue;
        }
        if ch == '@' && is_mention_boundary(line, i) {
            if let Some(j) = mention_end(line, i) {
                out.push_str(&esc(&line[text_start..i]));
                out.push_str(r#"<span class="mention">@"#);
                out.push_str(&esc(&line[i + 1..j]));
                out.push_str("</span>");
                i = j;
                text_start = i;
                continue;
            }
        }
        if matches!(ch, '*' | '_' | '~') && is_format_open_boundary(line, i) {
            if let Some(j) = find_closing_delim(line, i + 1, ch) {
                if j > i + 1 {
                    out.push_str(&esc(&line[text_start..i]));
                    let (open, close) = match ch {
                        '*' => ("<strong>", "</strong>"),
                        '_' => ("<em>", "</em>"),
                        '~' => ("<del>", "</del>"),
                        _ => unreachable!(),
                    };
                    out.push_str(open);
                    out.push_str(&esc(&line[i + 1..j]));
                    out.push_str(close);
                    i = j + 1;
                    text_start = i;
                    continue;
                }
            }
        }
        // Advance by one full UTF-8 char.
        i += utf8_len(bytes[i]);
    }
    out.push_str(&esc(&line[text_start..]));
}

fn fence_lang(line: &str) -> Option<Option<&str>> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("```")?;
    let lang = rest.trim();
    if lang.is_empty() {
        Some(None)
    } else {
        let end = lang.find(char::is_whitespace).unwrap_or(lang.len());
        Some(Some(&lang[..end]))
    }
}

fn is_fence_close(line: &str) -> bool {
    line.trim_end() == "```"
}

fn find_fence_close(lines: &[&str], start: usize) -> Option<usize> {
    (start..lines.len()).find(|&i| is_fence_close(lines[i]))
}

fn emit_fence(lines: &[&str], lang: Option<&str>, out: &mut String) {
    out.push_str("<pre><code");
    if let Some(lang) = lang {
        out.push_str(" class=\"lang-");
        out.push_str(&esc(lang));
        out.push('"');
    }
    out.push('>');
    out.push_str(&esc(&lines.join("\n")));
    out.push_str("</code></pre>");
}

fn find_next_char(line: &str, start: usize, needle: char) -> Option<usize> {
    line[start..]
        .char_indices()
        .find_map(|(offset, ch)| (ch == needle).then_some(start + offset))
}

fn is_mention_boundary(line: &str, at: usize) -> bool {
    match line[..at].chars().next_back() {
        None => true,
        Some(prev) => !is_mention_char(prev) && prev != '@',
    }
}

fn mention_end(line: &str, at: usize) -> Option<usize> {
    let mut j = at + 1;
    while j < line.len() {
        let ch = line[j..].chars().next().unwrap();
        if !is_mention_char(ch) {
            break;
        }
        j += ch.len_utf8();
    }
    (j > at + 1).then_some(j)
}

fn is_format_open_boundary(line: &str, at: usize) -> bool {
    match line[..at].chars().next_back() {
        None => true,
        Some(prev) => !prev.is_alphanumeric() && prev != '_',
    }
}

fn is_format_close_boundary(line: &str, after: usize) -> bool {
    match line[after..].chars().next() {
        None => true,
        Some(next) => !next.is_alphanumeric() && next != '_',
    }
}

fn find_closing_delim(line: &str, start: usize, delim: char) -> Option<usize> {
    let mut i = start;
    while i < line.len() {
        let ch = line[i..].chars().next().unwrap();
        if ch == delim && (delim != '_' || is_format_close_boundary(line, i + ch.len_utf8())) {
            return Some(i);
        }
        i += ch.len_utf8();
    }
    None
}

fn starts_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Bytes permitted inside an autolinked URL run (RFC 3986 unreserved + common sub-delims).
fn is_url_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'?'
                | b'#'
                | b'['
                | b']'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b'%'
        )
}

/// Strip trailing punctuation that is more likely sentence punctuation than part of the URL.
fn trim_url_trailing(url: &str) -> &str {
    url.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']'])
}

/// Length in bytes of the UTF-8 char starting at lead byte `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_tags() {
        let r = render_body("<script>alert(1)</script>");
        assert!(!r.contains("<script>"));
        assert!(r.contains("&lt;script&gt;"));
    }

    #[test]
    fn autolinks_http_urls() {
        let r = render_body("see https://example.com/x?a=1 now");
        assert!(r.contains("<a href=\"https://example.com/x?a=1\""));
        assert!(r.contains(">https://example.com/x?a=1</a>"));
        assert!(r.contains("rel=\"noopener noreferrer nofollow\""));
    }

    #[test]
    fn does_not_link_non_http_schemes() {
        let r = render_body("javascript:alert(1) and file:///etc/passwd");
        assert!(!r.contains("<a "));
        assert!(r.contains("javascript:alert(1)"));
    }

    #[test]
    fn url_in_a_tag_cannot_break_out() {
        // A crafted "url" containing a quote is escaped, so it cannot escape the href attribute.
        let r = render_body("https://example.com/\"><img src=x onerror=alert(1)>");
        assert!(!r.contains("<img"));
        assert!(r.contains("&quot;") || r.contains("&gt;"));
    }

    #[test]
    fn newlines_become_br() {
        let r = render_body("a\nb");
        assert_eq!(r, "a<br>b");
    }

    #[test]
    fn trailing_period_not_part_of_link() {
        let r = render_body("go to https://example.com.");
        assert!(r.contains(">https://example.com</a>."));
    }

    #[test]
    fn unicode_body_is_preserved() {
        let r = render_body("你好 https://例え.test world");
        assert!(r.contains("你好"));
        assert!(r.contains("world"));
    }

    #[test]
    fn fenced_code_escapes_without_inline_processing() {
        let r =
            render_body("```rust\n<script>alert(1)</script>\nhttps://example.com\n_snake_\n```");
        assert!(r.contains(r#"<pre><code class="lang-rust">"#));
        assert!(r.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!r.contains("<script>"));
        assert!(!r.contains(r#"<a href="https://example.com""#));
        assert!(r.contains("_snake_"));
        assert!(!r.contains("<br>"));
    }

    #[test]
    fn inline_code_emphasis_quote_and_mentions_render() {
        let r = render_body("`code` *bold* _italic_ ~strike~ @alice\n> q");
        assert!(r.contains("<code>code</code>"));
        assert!(r.contains("<strong>bold</strong>"));
        assert!(r.contains("<em>italic</em>"));
        assert!(r.contains("<del>strike</del>"));
        assert!(r.contains(r#"<span class="mention">@alice</span>"#));
        assert!(r.contains("<blockquote>q</blockquote>"));
    }

    #[test]
    fn unbalanced_markup_falls_back_to_literal_text() {
        assert_eq!(render_body("lone * marker"), "lone * marker");
        let r = render_body("```\nno close");
        assert!(!r.contains("<pre>"));
        assert_eq!(r, "```<br>no close");
    }

    #[test]
    fn snake_case_identifiers_are_not_italicized() {
        let r = render_body("snake_case_id and ok _italic_");
        assert!(r.contains("snake_case_id"));
        assert!(r.contains("<em>italic</em>"));
        assert!(!r.contains("<em>case</em>"));
    }

    #[test]
    fn fence_language_class_is_escaped() {
        let r = render_body("```x\"><img>\nok\n```");
        assert!(r.contains(r#"<code class="lang-x&quot;&gt;&lt;img&gt;">"#));
        assert!(!r.contains("<img"));
    }

    #[test]
    fn render_preview_is_plain_single_line_without_fence_markers() {
        let r = render_preview("```rust\nlet x = 1;\n```\n> not rich @alice");
        assert_eq!(r, "let x = 1; &gt; not rich @alice");
    }

    #[test]
    fn hostile_preview_is_escaped_exactly_once() {
        let r = render_preview(r#"<script>alert("x")</script> & "quoted""#);
        assert_eq!(
            r,
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &quot;quoted&quot;"
        );
        assert!(!r.contains("&amp;lt;"));
        assert!(!r.contains("<script>"));
    }

    #[test]
    fn parses_at_mentions_distinct_and_lowercased() {
        let m = parse_mentions("hi @alice and @Bob_1, cc @alice again");
        assert_eq!(m, vec!["alice".to_string(), "bob_1".to_string()]);
    }

    #[test]
    fn mention_at_start_of_body() {
        assert_eq!(parse_mentions("@carol ping"), vec!["carol".to_string()]);
    }

    #[test]
    fn email_is_not_a_mention() {
        // The '@' in an email is preceded by an alphanumeric, so it is NOT a mention boundary.
        assert!(parse_mentions("mail bob@example.com now").is_empty());
    }

    #[test]
    fn bare_at_and_double_at_are_not_mentions() {
        assert!(parse_mentions("look @ this").is_empty());
        assert!(parse_mentions("email me @@ home").is_empty());
    }
}
