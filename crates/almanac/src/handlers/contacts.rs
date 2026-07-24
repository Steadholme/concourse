//! Address-book handlers: the contacts list with an inline add form, plus per-contact edit and
//! delete.
//!
//! Like the calendar, every contact is OWNED by the gateway-injected `X-Auth-Subject` and all
//! store calls are scoped to it; POSTs are double-submit CSRF checked and all stored text is
//! HTML-escaped on output.

use axum::extract::{
    rejection::{FormRejection, PathRejection},
    Path, State,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::error::AppError;
use crate::handlers::events::require_csrf;
use crate::handlers::{form_or_invalid, html_with_csrf_cookie, path_or_invalid};
use crate::render::{self, esc, layout};
use crate::store::Contact;
use crate::{now_ms, AppState};

/// The contact form body. The owner is NEVER taken from here.
#[derive(Debug, Deserialize)]
pub struct ContactForm {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub notes: String,
}

/// CSRF-only body for the delete forms.
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// GET /contacts  — the address book + add form
// ---------------------------------------------------------------------------

pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let contacts = state.store.list_contacts(&owner).await?;
    let csrf = auth::new_csrf_token();

    let content = format!(
        "{}<div class=\"view-contacts\">{}</div>",
        render::subnav("contacts"),
        render_contacts(&contacts, &csrf, None, false)
    );
    let html = layout("Contacts", &headers, &content);
    Ok(html_with_csrf_cookie(html, &csrf))
}

fn render_contacts(
    contacts: &[Contact],
    csrf: &str,
    submitted: Option<&ContactForm>,
    name_error: bool,
) -> String {
    let count = contacts.len();
    let head = format!(
        "<div class=\"page-head\">\
           <div><h1>Contacts</h1><p class=\"muted\">{count} contact{plural}</p></div>\
         </div>",
        count = count,
        plural = if count == 1 { "" } else { "s" },
    );

    let name = submitted.map(|form| form.name.as_str()).unwrap_or_default();
    let email = submitted
        .map(|form| form.email.as_str())
        .unwrap_or_default();
    let phone = submitted
        .map(|form| form.phone.as_str())
        .unwrap_or_default();
    let notes = submitted
        .map(|form| form.notes.as_str())
        .unwrap_or_default();
    let error_summary = contact_error_summary(name_error);
    let name_aria = if name_error {
        " aria-invalid=\"true\" aria-describedby=\"contact-name-error\""
    } else {
        ""
    };
    let name_error_html = if name_error {
        "<p class=\"field-error\" id=\"contact-name-error\">A contact needs a name.</p>"
    } else {
        ""
    };
    let add = format!(
        "<form class=\"card contact-add\" method=\"post\" action=\"/contacts/new\">\
           <h2>Add a contact</h2>\
           {error_summary}\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <div class=\"contact-add__grid\">\
             <div class=\"editor__field\"><label for=\"name\">Name</label>\
               <input id=\"name\" type=\"text\" name=\"name\" value=\"{name}\" autocomplete=\"off\" maxlength=\"200\" required{name_aria}>\
               {name_error_html}</div>\
             <div class=\"editor__field\"><label for=\"email\">Email</label>\
               <input id=\"email\" type=\"text\" name=\"email\" value=\"{email}\" autocomplete=\"off\" maxlength=\"200\"></div>\
             <div class=\"editor__field\"><label for=\"phone\">Phone</label>\
               <input id=\"phone\" type=\"text\" name=\"phone\" value=\"{phone}\" autocomplete=\"off\" maxlength=\"100\"></div>\
           </div>\
           <div class=\"editor__field\"><label for=\"notes\">Notes</label>\
             <textarea id=\"notes\" name=\"notes\" rows=\"2\">{notes}</textarea></div>\
           <div class=\"editor__actions\"><button class=\"btn btn-primary\" type=\"submit\">Add contact</button></div>\
         </form>",
        error_summary = error_summary,
        csrf = esc(csrf),
        name = esc(name),
        name_aria = name_aria,
        name_error_html = name_error_html,
        email = esc(email),
        phone = esc(phone),
        notes = esc(notes),
    );

    let list = if contacts.is_empty() {
        "<div class=\"list-empty\">No contacts yet. Add the first one above.</div>".to_string()
    } else {
        let rows: String = contacts
            .iter()
            .map(|c| render_contact_row(c, csrf))
            .collect();
        format!(
            "<table class=\"contact-table\">\
               <thead><tr><th>Name</th><th>Email</th><th>Phone</th><th></th></tr></thead>\
               <tbody>{rows}</tbody>\
             </table>",
        )
    };

    format!("{head}{add}<section class=\"card\">{list}</section>")
}

fn contact_error_summary(name_error: bool) -> String {
    if !name_error {
        return String::new();
    }
    "<section class=\"error-summary\" role=\"alert\" aria-labelledby=\"contact-errors-title\">\
       <h2 id=\"contact-errors-title\">Check the contact details</h2>\
       <ul><li><a href=\"#name\">A contact needs a name.</a></li></ul>\
     </section>"
        .to_string()
}

fn render_contact_row(c: &Contact, csrf: &str) -> String {
    let notes = if c.notes.trim().is_empty() {
        String::new()
    } else {
        format!("<div class=\"contact-notes\">{}</div>", esc(&c.notes))
    };
    format!(
        "<tr>\
           <td class=\"c-name\" data-label=\"Name\"><a href=\"/contacts/edit/{id}\">{name}</a>{notes}</td>\
           <td class=\"c-email\" data-label=\"Email\">{email}</td>\
           <td class=\"c-phone\" data-label=\"Phone\">{phone}</td>\
           <td class=\"c-actions\" data-label=\"Actions\">\
             <a class=\"btn btn-secondary btn-sm\" href=\"/contacts/edit/{id}\">Edit</a>\
             <form class=\"inline-del\" method=\"post\" action=\"/contacts/delete/{id}\" onsubmit=\"return confirm('Delete this contact?')\">\
               <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
               <button class=\"btn btn-ghost btn-sm\" type=\"submit\">Delete</button>\
             </form>\
           </td>\
         </tr>",
        id = esc(&c.id),
        name = esc(&c.name),
        notes = notes,
        email = esc(&c.email),
        phone = esc(&c.phone),
        csrf = esc(csrf),
    )
}

// ---------------------------------------------------------------------------
// POST /contacts/new  — create a contact
// ---------------------------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<ContactForm>, FormRejection>,
) -> Result<Response, AppError> {
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let (name, email, phone, notes) = match parse_contact_form(&form) {
        Ok(parsed) => parsed,
        Err(()) => {
            let contacts = state.store.list_contacts(&owner).await?;
            let content = format!(
                "{}<div class=\"view-contacts\">{}</div>",
                render::subnav("contacts"),
                render_contacts(&contacts, &form.csrf_token, Some(&form), true)
            );
            let mut response =
                html_with_csrf_cookie(layout("Contacts", &headers, &content), &form.csrf_token);
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(response);
        }
    };

    state
        .store
        .upsert_contact(Contact {
            id: auth::random_hex(),
            owner_sub: owner,
            name,
            email,
            phone,
            notes,
            created_at: now_ms(),
        })
        .await?;
    Ok(Redirect::to("/contacts").into_response())
}

// ---------------------------------------------------------------------------
// GET /contacts/edit/{id}  — the edit form
// ---------------------------------------------------------------------------

pub async fn edit_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let owner = auth::owner_subject(&headers);
    let contact = state
        .store
        .get_contact(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That contact does not exist.".to_string()))?;

    let csrf = auth::new_csrf_token();
    let content = format!(
        "{}<div class=\"view-contacts view-editor\">{}</div>",
        render::subnav("contacts"),
        render_edit_form(&contact, &csrf, false)
    );
    Ok(html_with_csrf_cookie(
        layout("Edit contact", &headers, &content),
        &csrf,
    ))
}

fn render_edit_form(c: &Contact, csrf: &str, name_error: bool) -> String {
    let error_summary = contact_error_summary(name_error);
    let name_aria = if name_error {
        " aria-invalid=\"true\" aria-describedby=\"contact-name-error\""
    } else {
        ""
    };
    let name_error_html = if name_error {
        "<p class=\"field-error\" id=\"contact-name-error\">A contact needs a name.</p>"
    } else {
        ""
    };
    let main = format!(
        "<form class=\"card editor\" method=\"post\" action=\"/contacts/edit/{id}\">\
           <div class=\"editor__head\"><h1>Edit contact</h1></div>\
           {error_summary}\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <div class=\"editor__field\"><label for=\"name\">Name</label>\
             <input id=\"name\" type=\"text\" name=\"name\" value=\"{name}\" autocomplete=\"off\" maxlength=\"200\" required{name_aria}>\
             {name_error_html}</div>\
           <div class=\"editor__row\">\
             <div class=\"editor__field\"><label for=\"email\">Email</label>\
               <input id=\"email\" type=\"text\" name=\"email\" value=\"{email}\" autocomplete=\"off\" maxlength=\"200\"></div>\
             <div class=\"editor__field\"><label for=\"phone\">Phone</label>\
               <input id=\"phone\" type=\"text\" name=\"phone\" value=\"{phone}\" autocomplete=\"off\" maxlength=\"100\"></div>\
           </div>\
           <div class=\"editor__field\"><label for=\"notes\">Notes</label>\
             <textarea id=\"notes\" name=\"notes\" rows=\"4\">{notes}</textarea></div>\
           <div class=\"editor__actions\">\
             <a class=\"btn btn-secondary\" href=\"/contacts\">Cancel</a>\
             <button class=\"btn btn-primary\" type=\"submit\">Save contact</button>\
           </div>\
         </form>",
        id = esc(&c.id),
        error_summary = error_summary,
        csrf = esc(csrf),
        name = esc(&c.name),
        name_aria = name_aria,
        name_error_html = name_error_html,
        email = esc(&c.email),
        phone = esc(&c.phone),
        notes = esc(&c.notes),
    );
    let danger = format!(
        "<form class=\"card danger-zone\" method=\"post\" action=\"/contacts/delete/{id}\" onsubmit=\"return confirm('Delete this contact?')\">\
           <div class=\"danger-zone__text\"><strong>Delete contact</strong><p class=\"muted\">This permanently removes the contact.</p></div>\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <button class=\"btn btn-danger\" type=\"submit\">Delete</button>\
         </form>",
        id = esc(&c.id),
        csrf = esc(csrf),
    );
    format!("{main}{danger}")
}

// ---------------------------------------------------------------------------
// POST /contacts/edit/{id}  — update a contact
// ---------------------------------------------------------------------------

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<ContactForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);

    let existing = state
        .store
        .get_contact(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That contact does not exist.".to_string()))?;
    let (name, email, phone, notes) = match parse_contact_form(&form) {
        Ok(parsed) => parsed,
        Err(()) => {
            let submitted = Contact {
                id: existing.id.clone(),
                owner_sub: owner,
                name: form.name.clone(),
                email: form.email.clone(),
                phone: form.phone.clone(),
                notes: form.notes.clone(),
                created_at: existing.created_at,
            };
            let content = format!(
                "{}<div class=\"view-contacts view-editor\">{}</div>",
                render::subnav("contacts"),
                render_edit_form(&submitted, &form.csrf_token, true)
            );
            let mut response =
                html_with_csrf_cookie(layout("Edit contact", &headers, &content), &form.csrf_token);
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(response);
        }
    };

    state
        .store
        .upsert_contact(Contact {
            id: existing.id,
            owner_sub: owner,
            name,
            email,
            phone,
            notes,
            created_at: existing.created_at,
        })
        .await?;
    Ok(Redirect::to("/contacts").into_response())
}

// ---------------------------------------------------------------------------
// POST /contacts/delete/{id}  — delete a contact
// ---------------------------------------------------------------------------

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<DeleteForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    state.store.delete_contact(&owner, &id).await?;
    Ok(Redirect::to("/contacts").into_response())
}

/// Validate + trim a contact form. Only the name is required.
fn parse_contact_form(form: &ContactForm) -> Result<(String, String, String, String), ()> {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err(());
    }
    Ok((
        name,
        form.email.trim().to_string(),
        form.phone.trim().to_string(),
        form.notes.trim().to_string(),
    ))
}
