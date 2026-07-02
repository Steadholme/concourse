//! Message-body text sanitization + autolinking for the server-rendered timeline.
//!
//! SECURITY: message bodies are author-supplied, so they are treated as PLAIN TEXT, never markup.
//! [`render_body`] first HTML-escapes the whole body (so any `<`, `>`, `&`, quotes become inert
//! entities — no tag, attribute, or `javascript:` payload can survive), THEN autolinks bare
//! `http`/`https` URLs into `<a>` elements. Because escaping happens BEFORE linking and the only
//! tag we emit is a fixed `<a href="…" rel="noopener noreferrer nofollow">` whose href is itself
//! escaped + scheme-restricted, no user input ever reaches the page as live HTML. The dashboard's
//! live-update JS performs the identical escape-then-link using DOM text nodes.

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

/// Render a message body to SAFE HTML: escape everything, then turn bare http/https URLs into
/// links. Newlines become `<br>`. The result is safe to interpolate directly into the page.
pub fn render_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 16);
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push_str("<br>");
        }
        autolink_line(line, &mut out);
    }
    out
}

/// Autolink one line: scan for `http://` / `https://` spans, emit them as `<a>`, escape the rest.
fn autolink_line(line: &str, out: &mut String) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    while i < line.len() {
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
            i = i + url.len();
            text_start = i;
        } else {
            // Advance by one full UTF-8 char.
            i += utf8_len(bytes[i]);
        }
    }
    out.push_str(&esc(&line[text_start..]));
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
    url.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']'))
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
