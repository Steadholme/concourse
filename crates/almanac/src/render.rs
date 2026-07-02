//! Server-side HTML rendering helpers: escaping, the page shell, the sub-nav, error pages.
//!
//! The enterprise HOLDFAST shell (top app-bar with the shield + wordmark + page name, and the
//! signed-in email + Logout on the right) and the design-system CSS are embedded via
//! `include_str!`, so every page is self-contained with no asset round-trips. Each handler
//! builds only its inner `content` HTML and hands it to [`layout`].

use crate::auth;
use axum::http::HeaderMap;

/// Embedded design-system CSS (brand tokens shared across the HOLDFAST estate).
const APP_CSS: &str = include_str!("../static/app.css");
/// Page shell with `{{...}}` slots.
const LAYOUT: &str = include_str!("../templates/layout.html");

/// Cross-subdomain SSO logout (terminated at the Keystone IdP behind the gateway).
const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// Lucide-style line icons (viewBox 0 0 24 24, no fill, rounded caps) used across the app-bar.
const IC_GRID: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>"#;
const IC_USER: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>"#;
const IC_LOGOUT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/></svg>"#;

/// The section nav for the v2 app-bar (`Calendar` / `Contacts` / `Settings`), with the current
/// page marked `is-active` from the page title. Same hrefs/labels as the in-content [`subnav`]
/// (which remains for narrow viewports where the app-bar nav collapses).
pub fn appnav(page_title: &str) -> String {
    let cal_ic = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="18" rx="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/></svg>"#;
    let contacts_ic = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/></svg>"#;
    let set_ic = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="2" y1="14" x2="6" y2="14"/><line x1="10" y1="8" x2="14" y2="8"/><line x1="18" y1="16" x2="22" y2="16"/></svg>"#;
    let on = |b: bool| if b { " is-active" } else { "" };
    let cal = matches!(
        page_title,
        "Calendar" | "Week" | "Day" | "Agenda" | "New event" | "Edit event"
    );
    let contacts = matches!(page_title, "Contacts" | "Edit contact");
    let settings = page_title == "Settings";
    format!(
        r#"<a class="appnav{c}" href="/">{cal_ic}Calendar</a><a class="appnav{ct}" href="/contacts">{contacts_ic}Contacts</a><a class="appnav{s}" href="/settings">{set_ic}Settings</a>"#,
        c = on(cal),
        ct = on(contacts),
        s = on(settings),
        cal_ic = cal_ic,
        contacts_ic = contacts_ic,
        set_ic = set_ic,
    )
}

/// The right side of the app-bar: an "All apps" button back to the apex portal and the signed-in
/// avatar menu (a CSS focus-within dropdown from the v2 kit) listing Account, All apps, and the
/// PRESERVED cross-subdomain logout link. A blank identity falls back to a static glyph so the
/// chrome always renders.
pub fn userbox(headers: &HeaderMap) -> String {
    let email = auth::signed_in_email(headers).unwrap_or_default();
    let signed_in = !email.is_empty();
    let (initial, name, head) = if signed_in {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        let initial = esc(&initial);
        let head = format!(
            r#"<div class="usermenu__head"><span class="avatar" aria-hidden="true">{initial}</span><div><b>{email}</b><span>Signed in</span></div></div>"#,
            initial = initial,
            email = esc(&email),
        );
        (initial, esc(&email), head)
    } else {
        ("?".to_string(), "Account".to_string(), String::new())
    };
    format!(
        r#"<a class="iconbtn" href="https://w33d.xyz" title="All apps" aria-label="All apps">{grid}</a>
    <div class="usermenu">
      <button class="usermenu__btn" type="button" aria-haspopup="menu" aria-expanded="false">
        <span class="avatar" aria-hidden="true">{initial}</span>
        <span class="usermenu__name">{name}</span>
        <svg class="usermenu__caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>
      </button>
      <div class="usermenu__pop" role="menu">
        {head}
        <a class="menuitem" role="menuitem" href="https://account.w33d.xyz">{user}Account</a>
        <a class="menuitem" role="menuitem" href="https://w33d.xyz">{grid}All apps</a>
        <a class="menuitem menuitem--danger" role="menuitem" href="{logout}">{out}Log out</a>
      </div>
    </div>"#,
        grid = IC_GRID,
        initial = initial,
        name = name,
        head = head,
        user = IC_USER,
        out = IC_LOGOUT,
        logout = LOGOUT_URL,
    )
}

/// Wrap inner `content` HTML in the full HOLDFAST shell.
///
/// `page_title` is escaped into the `<title>` and the app-bar; `headers` supplies the
/// gateway-injected signed-in email shown in the app-bar user chip. `content` is already-safe
/// HTML built by the handler.
pub fn layout(page_title: &str, headers: &HeaderMap, content: &str) -> String {
    LAYOUT
        .replace("{{STYLE}}", APP_CSS)
        .replace("{{PAGE_TITLE}}", &esc(page_title))
        .replace("{{NAV}}", &appnav(page_title))
        .replace("{{USERBOX}}", &userbox(headers))
        .replace("{{CONTENT}}", content)
}

/// The section nav (`Calendar` / `Contacts` / `Settings`). `active` is `"calendar"`,
/// `"contacts"`, or `"settings"`.
pub fn subnav(active: &str) -> String {
    let tab = |href: &str, label: &str, key: &str| {
        let cls = if key == active {
            "subnav__link is-active"
        } else {
            "subnav__link"
        };
        format!("<a class=\"{cls}\" href=\"{href}\">{label}</a>")
    };
    format!(
        "<nav class=\"subnav\">{}{}{}</nav>",
        tab("/", "Calendar", "calendar"),
        tab("/contacts", "Contacts", "contacts"),
        tab("/settings", "Settings", "settings"),
    )
}

/// A standalone HOLDFAST-styled error page (used by [`crate::error::AppError`]).
pub fn error_page(status: u16, title: &str, detail: &str) -> String {
    let content = format!(
        "<section class=\"card empty-state\">\
           <div class=\"empty-state__code\">{status}</div>\
           <h1>{title}</h1>\
           <p class=\"muted\">{detail}</p>\
           <a class=\"btn btn-primary\" href=\"/\">Back to the calendar</a>\
         </section>",
        status = status,
        title = esc(title),
        detail = esc(detail),
    );
    // No gateway headers here — render with a generic shell.
    layout(title, &HeaderMap::new(), &content)
}

/// A minimal PUBLIC page shell (no SSO app-bar, nav, or user chrome) for the no-SSO RSVP pages.
/// Reuses the estate design tokens so it still looks native, but exposes no authenticated links.
/// `content` is already-safe HTML built by the handler; `page_title` is escaped.
pub fn public_shell(page_title: &str, content: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light\"><meta name=\"robots\" content=\"noindex\">\
         <title>{title} · Almanac · HOLDFAST</title><style>{css}</style></head>\
         <body class=\"page-almanac\"><main class=\"wrap wrap--narrow\">{content}</main></body></html>",
        title = esc(page_title),
        css = APP_CSS,
        content = content,
    )
}

/// A design-system pill for an attendee's RSVP status.
pub fn status_pill(status: &str) -> String {
    let (cls, label) = match status {
        "accepted" => ("pill pill-ok", "Accepted"),
        "declined" => ("pill pill-down", "Declined"),
        "tentative" => ("pill pill-warn", "Tentative"),
        _ => ("pill pill-info", "No response"),
    };
    format!("<span class=\"{cls}\">{label}</span>")
}

/// Minimal HTML-escape for any untrusted text rendered into the page.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_metacharacters() {
        assert_eq!(esc("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn layout_embeds_title_and_email() {
        let mut headers = HeaderMap::new();
        headers.insert(auth::HEADER_EMAIL, "me@holdfast.local".parse().unwrap());
        let html = layout("Calendar", &headers, "<p>hi</p>");
        assert!(html.contains("<p>hi</p>"));
        assert!(html.contains("me@holdfast.local"));
        assert!(html.contains("HOLDFAST"));
        assert!(html.contains("sso.w33d.xyz/_gw/auth/logout"));
    }

    #[test]
    fn subnav_marks_active_tab() {
        let html = subnav("contacts");
        assert!(html.contains("href=\"/contacts\""));
        assert!(html.contains("is-active"));
        // The active class sits on the contacts link, not calendar.
        let calendar_idx = html.find(">Calendar<").unwrap();
        let contacts_idx = html.find(">Contacts<").unwrap();
        assert!(calendar_idx < contacts_idx);
    }
}
