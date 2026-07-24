//! Public RSVP capability surface.
//!
//! `GET /rsvp/{token}` is strictly side-effect free. A response is a deliberate two-stage POST:
//! `confirm` renders the consequence, then `commit` performs the token-scoped attendee update.
//! The capability remains only in the request path; forms deliberately omit `action`.

use axum::extract::{rejection::PathRejection, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::calendar;
use crate::render::{esc, public_shell, status_tag};
use crate::store::StoreError;
use crate::AppState;

const CACHE_CONTROL: &str = "private, no-store";
const REFERRER_POLICY: &str = "no-referrer";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RsvpIntent {
    Confirm,
    Commit,
    Back,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RsvpReply {
    Accepted,
    Tentative,
    Declined,
}

impl RsvpReply {
    fn status(self) -> &'static str {
        match self {
            RsvpReply::Accepted => "accepted",
            RsvpReply::Tentative => "tentative",
            RsvpReply::Declined => "declined",
        }
    }

    fn label(self) -> &'static str {
        match self {
            RsvpReply::Accepted => "Accepted",
            RsvpReply::Tentative => "Tentative",
            RsvpReply::Declined => "Declined",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RsvpForm {
    #[serde(default)]
    pub csrf_token: String,
    pub intent: RsvpIntent,
    #[serde(default)]
    pub reply: Option<RsvpReply>,
    #[serde(default)]
    pub confirmation_proof: String,
}

#[derive(Clone, Debug)]
struct RsvpView {
    title: String,
    when: String,
    current_status: String,
}

/// Side-effect-free public read. Legacy `?reply=` input is intentionally ignored by this handler.
pub async fn show(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let Path(token) = match path {
        Ok(path) => path,
        Err(_) => return invalid_form(),
    };
    let csrf = auth::new_csrf_token();
    match load_view(&state, &token).await {
        Ok(Some(view)) => privacy_html(
            StatusCode::OK,
            public_shell("RSVP", &render_choice(&view, &csrf)),
            Some(&csrf),
        ),
        Ok(None) => not_found(Some(&csrf)),
        Err(_) => unavailable(Some(&csrf)),
    }
}

/// Unsafe RSVP response. `Confirm` and `Back` never mutate; only `Commit` reaches the Store write.
pub async fn respond(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<RsvpForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let Path(token) = match path {
        Ok(path) => path,
        Err(_) => return invalid_form(),
    };
    let Form(form) = match form {
        Ok(form) => form,
        Err(_) => return invalid_form(),
    };
    if !auth::verify_csrf(&headers, &form.csrf_token) {
        return csrf_expired();
    }

    if form.intent == RsvpIntent::Commit {
        let reply = match form.reply {
            Some(reply) => reply,
            None => return invalid_form(),
        };
        if !auth::verify_rsvp_confirmation(
            &form.confirmation_proof,
            &token,
            reply.status(),
            &form.csrf_token,
            auth::now_unix(),
        ) {
            return confirmation_expired();
        }
        match state
            .store
            .set_attendee_status_by_token(&token, reply.status())
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => return not_found(None),
            Err(_) => return unavailable(None),
        }

        // Value-free audit: neither capability nor selected reply enters the audit projection.
        tracing::info!(target: "audit", event = "attendee.rsvp", "attendee rsvp recorded");
        return privacy_redirect(&token);
    }

    let view = match load_view(&state, &token).await {
        Ok(Some(view)) => view,
        Ok(None) => return not_found(None),
        Err(_) => return unavailable(None),
    };

    match form.intent {
        RsvpIntent::Back => privacy_html(
            StatusCode::OK,
            public_shell("RSVP", &render_choice(&view, &form.csrf_token)),
            None,
        ),
        RsvpIntent::Confirm => match form.reply {
            Some(reply) => {
                let proof = auth::mint_rsvp_confirmation(
                    &token,
                    reply.status(),
                    &form.csrf_token,
                    auth::now_unix(),
                );
                privacy_html(
                    StatusCode::OK,
                    public_shell(
                        "Confirm response",
                        &render_confirmation(&view, reply, &form.csrf_token, &proof),
                    ),
                    None,
                )
            }
            None => invalid_form(),
        },
        RsvpIntent::Commit => unreachable!("Commit returns before loading the view"),
    }
}

/// Load only the safe Gatehouse projection. A missing attendee OR event is one unavailable link.
async fn load_view(state: &AppState, token: &str) -> Result<Option<RsvpView>, StoreError> {
    let attendee = match state.store.get_attendee_by_token(token).await? {
        Some(attendee) => attendee,
        None => return Ok(None),
    };
    let event = match state
        .store
        .get_event(&attendee.owner_sub, &attendee.event_id)
        .await?
    {
        Some(event) => event,
        None => return Ok(None),
    };
    let settings = state.store.get_settings(&attendee.owner_sub).await?;
    let off = calendar::tz_offset_minutes(&settings.timezone);
    Ok(Some(RsvpView {
        title: event.title,
        when: calendar::fmt_event_when_at(event.starts_at, event.ends_at, event.all_day, off),
        current_status: attendee.status,
    }))
}

fn gate_plaque(view: &RsvpView) -> String {
    format!(
        "<div class=\"gate__plaque\">\
           <h1>{title}</h1>\
           <p class=\"gate__when\">{when}</p>\
         </div>",
        title = esc(&view.title),
        when = esc(&view.when),
    )
}

fn render_choice(view: &RsvpView, csrf: &str) -> String {
    let choice = |reply: &str, class: &str, label: &str, consequence: &str| {
        format!(
            "<form method=\"post\" class=\"gate__choice\">\
               <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
               <input type=\"hidden\" name=\"intent\" value=\"confirm\">\
               <input type=\"hidden\" name=\"reply\" value=\"{reply}\">\
               <button class=\"btn {class}\" type=\"submit\">\
                 <strong>{label}</strong><span>{consequence}</span>\
               </button>\
             </form>",
            csrf = esc(csrf),
        )
    };
    format!(
        "<section class=\"gate gate--choice\">\
           <p class=\"gate__brand\">Steadholme Almanac</p>\
           {plaque}\
           <div class=\"gate__current\"><span>Current response</span>{tag}</div>\
           <div class=\"gate__choices\" aria-label=\"Choose a response\">\
             {accepted}{tentative}{declined}\
           </div>\
         </section>",
        plaque = gate_plaque(view),
        tag = status_tag(&view.current_status),
        accepted = choice(
            "accepted",
            "btn-primary",
            "Accept",
            "The organizer sees your response",
        ),
        tentative = choice(
            "tentative",
            "btn-secondary",
            "Maybe",
            "Tentative; you can change it later",
        ),
        declined = choice(
            "declined",
            "btn-danger",
            "Decline",
            "The organizer sees your response",
        ),
    )
}

fn render_confirmation(view: &RsvpView, reply: RsvpReply, csrf: &str, proof: &str) -> String {
    format!(
        "<section class=\"gate gate--confirm\">\
           <p class=\"gate__brand\">Steadholme Almanac</p>\
           {plaque}\
           <div class=\"state state--confirm\">\
             <h2>Confirm: {reply} for {title}</h2>\
             <p>{when}</p>\
           </div>\
           <div class=\"gate__actions\">\
             <form method=\"post\">\
               <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
               <input type=\"hidden\" name=\"intent\" value=\"commit\">\
               <input type=\"hidden\" name=\"reply\" value=\"{status}\">\
               <input type=\"hidden\" name=\"confirmation_proof\" value=\"{proof}\">\
               <button class=\"btn btn-primary\" type=\"submit\">Confirm</button>\
             </form>\
             <form method=\"post\">\
               <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
               <input type=\"hidden\" name=\"intent\" value=\"back\">\
               <button class=\"btn btn-secondary\" type=\"submit\">Back</button>\
             </form>\
           </div>\
         </section>",
        plaque = gate_plaque(view),
        reply = reply.label(),
        title = esc(&view.title),
        when = esc(&view.when),
        csrf = esc(csrf),
        status = reply.status(),
        proof = esc(proof),
    )
}

fn not_found(csrf: Option<&str>) -> Response {
    let body = public_shell(
        "Invitation not available",
        "<section class=\"gate gate--failure state state--not-found\">\
           <h1>This invitation link is not available.</h1>\
         </section>",
    );
    privacy_html(StatusCode::NOT_FOUND, body, csrf)
}

fn unavailable(csrf: Option<&str>) -> Response {
    let body = public_shell(
        "Invitation unavailable",
        "<section class=\"gate gate--failure state state--unavailable\">\
           <h1>Something went wrong.</h1>\
           <p>Please try the link again shortly.</p>\
         </section>",
    );
    privacy_html(StatusCode::SERVICE_UNAVAILABLE, body, csrf)
}

fn invalid_form() -> Response {
    let body = public_shell(
        "Check your response",
        "<section class=\"gate gate--failure state state--invalid\">\
           <h1>Choose a response and try again.</h1>\
         </section>",
    );
    privacy_html(StatusCode::BAD_REQUEST, body, None)
}

fn csrf_expired() -> Response {
    let body = public_shell(
        "Form expired",
        "<section class=\"gate gate--failure state state--csrf\">\
           <h1>This form expired.</h1>\
           <p>Reload the invitation link and try again.</p>\
         </section>",
    );
    privacy_html(StatusCode::FORBIDDEN, body, None)
}

fn confirmation_expired() -> Response {
    let body = public_shell(
        "Confirmation expired",
        "<section class=\"gate gate--failure state state--invalid\">\
           <h1>Review this response again.</h1>\
           <p>The confirmation is missing, changed, or expired. Return to the invitation link and choose your response again.</p>\
         </section>",
    );
    privacy_html(StatusCode::BAD_REQUEST, body, None)
}

fn privacy_redirect(token: &str) -> Response {
    let location = format!("/rsvp/{token}");
    let Ok(location) = HeaderValue::from_str(&location) else {
        return unavailable(None);
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    response.headers_mut().insert(
        HeaderNameExt::referrer_policy(),
        HeaderValue::from_static(REFERRER_POLICY),
    );
    response
}

fn privacy_html(status: StatusCode, body: String, csrf: Option<&str>) -> Response {
    let mut response = (status, Html(body)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    response.headers_mut().insert(
        HeaderNameExt::referrer_policy(),
        HeaderValue::from_static(REFERRER_POLICY),
    );
    if let Some(csrf) = csrf {
        if let Ok(value) = HeaderValue::from_str(&auth::csrf_cookie(csrf)) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// Keep the non-standard but well-established response header name in one checked constructor.
struct HeaderNameExt;

impl HeaderNameExt {
    fn referrer_policy() -> axum::http::HeaderName {
        axum::http::HeaderName::from_static("referrer-policy")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_values_are_stable_store_keywords() {
        assert_eq!(RsvpReply::Accepted.status(), "accepted");
        assert_eq!(RsvpReply::Tentative.status(), "tentative");
        assert_eq!(RsvpReply::Declined.status(), "declined");
    }

    #[test]
    fn gate_copy_never_contains_capability_or_attendee_identity() {
        let view = RsvpView {
            title: "Planning <day>".to_string(),
            when: "2099-01-01 09:00".to_string(),
            current_status: "needs-action".to_string(),
        };
        let html = render_choice(&view, "csrf-value");
        assert!(!html.contains("action="));
        assert!(!html.contains("capability"));
        assert!(html.contains("Planning &lt;day&gt;"));
    }
}
