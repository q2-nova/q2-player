use crate::http::{Response, Transport};
use scraper::{ElementRef, Html, Selector};
use std::time::{Duration, Instant};
use url::Url;

const PROBE: &str = "http://detectportal.firefox.com/success.txt";
const CAPPORT_MEDIA: &str = "application/captive+json";
const MAX_HTML_BYTES: usize = 64 * 1024;
const MAX_HTML_NODES: usize = 4096;
const MAX_HTML_DEPTH: usize = 64;
const MAX_FORM_BYTES: usize = 4096;
const MAX_URL_BYTES: usize = 4096;
const MAX_REDIRECTS: usize = 4;
const OFFER_LIFE: Duration = Duration::from_secs(120);

pub enum Detection {
    Online,
    Delta,
    Guest(Offer),
    Unsupported,
    Unknown,
}

/// A single-use snapshot of the form displayed to the traveler. Keep it with
/// the transport that detected it, scoped to the selected Wi-Fi association.
pub struct Offer {
    page: Url,
    action: Url,
    method: &'static str,
    fields: Vec<(String, String)>,
    expires: Instant,
}

fn usable_url(mut url: Url) -> Option<Url> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
        || url.as_str().len() > MAX_URL_BYTES
    {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.origin() == b.origin()
}

fn resolved(page: &Url, value: &str) -> Option<Url> {
    usable_url(page.join(value).ok()?)
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    let mut matching = response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name));
    let (_, value) = matching.next()?;
    matching.next().is_none().then_some(value.as_str())
}

fn media_type(response: &Response, expected: &str) -> bool {
    header(response, "content-type")
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn is_delta(url: &Url) -> bool {
    match url.host_str() {
        Some("wifi.delta.com" | "signin.delta.com") => {
            url.scheme() == "https" && url.port().is_none()
        }
        Some("deltawifi.com" | "www.deltawifi.com") => url.port().is_none(),
        _ => false,
    }
}

fn redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

enum Capport {
    Online,
    Portal(Url),
    Captive,
    Unavailable,
}

fn capport_state(http: &mut impl Transport, endpoint: &str) -> Option<Capport> {
    let endpoint = usable_url(Url::parse(endpoint).ok()?)?;
    if endpoint.scheme() != "https" {
        return None;
    }
    let response = http
        .request(
            "GET",
            endpoint.as_str(),
            &[("Accept", CAPPORT_MEDIA), ("Cache-Control", "no-store")],
            &[],
        )
        .ok()?;
    if response.status != 200 || !media_type(&response, CAPPORT_MEDIA) || response.body.len() > 8192
    {
        return None;
    }
    let data: serde_json::Value = serde_json::from_slice(&response.body).ok()?;
    match data.get("captive")?.as_bool()? {
        false => Some(Capport::Online),
        true => Some(
            data.get("user-portal-url")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Url::parse(value).ok())
                .and_then(usable_url)
                .filter(|url| url.scheme() == "https")
                .map_or(Capport::Captive, Capport::Portal),
        ),
    }
}

fn get_page(http: &mut impl Transport, page: &Url) -> Result<Response, String> {
    http.request(
        "GET",
        page.as_str(),
        &[
            ("Accept", "text/html, text/plain"),
            ("Cache-Control", "no-store"),
        ],
        &[],
    )
}

fn probe_succeeded(response: &Response) -> bool {
    response.status == 200 && response.body == b"success\n"
}

/// RFC 6585 section 6.1 advertises the operator's sign-in page in the head
/// of the intercepted probe response. Only navigate an immediate handoff.
fn probe_refresh(probe: &Url, body: &[u8]) -> Option<Url> {
    let document = parse_html(body)?;
    let selector = select("head > meta[http-equiv]");
    let mut refreshes = document.select(&selector).filter(|tag| {
        tag.attr("http-equiv")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("refresh"))
    });
    let tag = refreshes.next()?;
    if refreshes.next().is_some() {
        return None;
    }
    let (delay, destination) = tag.attr("content")?.split_once(';')?;
    let (key, raw) = destination.split_once('=')?;
    if delay.trim() != "0" || !key.trim().eq_ignore_ascii_case("url") {
        return None;
    }
    let raw = raw.trim();
    let raw = match raw.as_bytes().first()? {
        b'\'' => raw.strip_prefix('\'')?.strip_suffix('\'')?,
        b'"' => raw.strip_prefix('"')?.strip_suffix('"')?,
        _ => raw,
    };
    // A relative reference could resolve against the intercepted origin or
    // an HTML base element. Only an explicit operator origin identifies it.
    let authority = raw.split_once(':')?.1.strip_prefix("//")?;
    if authority.starts_with(['/', '\\']) {
        return None;
    }
    let target = usable_url(Url::parse(raw).ok()?)?;
    (!same_origin(probe, &target)).then_some(target)
}

pub fn detect(http: &mut impl Transport, capport: Option<&str>) -> Result<Detection, String> {
    let capport = capport
        .and_then(|endpoint| capport_state(http, endpoint))
        .unwrap_or(Capport::Unavailable);
    let probe = Url::parse(PROBE).expect("static portal probe URL");
    let (mut page, mut captive, https_only) = match capport {
        Capport::Online => {
            // Admission and access to the wider network are separate facts.
            return Ok(
                if get_page(http, &probe).is_ok_and(|reply| probe_succeeded(&reply)) {
                    Detection::Online
                } else {
                    Detection::Unknown
                },
            );
        }
        Capport::Portal(page) => (page, true, true),
        Capport::Captive => (probe.clone(), true, false),
        Capport::Unavailable => (probe.clone(), false, false),
    };

    for _ in 0..=MAX_REDIRECTS {
        if page != probe && is_delta(&page) {
            return Ok(Detection::Delta);
        }
        let Ok(response) = get_page(http, &page) else {
            return Ok(if captive {
                Detection::Unsupported
            } else {
                Detection::Unknown
            });
        };

        if redirect(response.status) || response.status == 511 {
            if let Some(next) =
                header(&response, "location").and_then(|value| resolved(&page, value))
            {
                if (https_only || page.scheme() == "https") && next.scheme() != "https" {
                    return Ok(Detection::Unsupported);
                }
                page = next;
                captive = true;
                continue;
            }
            if response.status != 511 {
                return Ok(Detection::Unsupported);
            }
            captive = true;
        }

        if page == probe && probe_succeeded(&response) {
            return Ok(if captive {
                Detection::Unsupported
            } else {
                Detection::Online
            });
        }

        if matches!(response.status, 200 | 511) && media_type(&response, "text/html") {
            // A replacement response at the probe URL is captive evidence.
            // Only a portal's own origin can supply an actionable form.
            if page == probe {
                if let Some(next) = probe_refresh(&probe, &response.body) {
                    page = next;
                    captive = true;
                    continue;
                }
            } else if !same_origin(&page, &probe) {
                match parse_offer(page, &response.body) {
                    Ok(offer) => {
                        http.decision("recognized_guest_offer");
                        return Ok(Detection::Guest(offer));
                    }
                    Err(reason) => http.decision(reason),
                }
            }
            return Ok(Detection::Unsupported);
        }
        return Ok(if captive {
            Detection::Unsupported
        } else {
            Detection::Unknown
        });
    }
    Ok(Detection::Unsupported)
}

fn select(query: &str) -> Selector {
    Selector::parse(query).expect("static portal selector")
}

pub(super) fn parse_html(body: &[u8]) -> Option<Html> {
    if body.len() > MAX_HTML_BYTES {
        return None;
    }
    let document = Html::parse_document(std::str::from_utf8(body).ok()?);
    let root = document.root_element();
    for (index, node) in root.descendants().enumerate() {
        if index >= MAX_HTML_NODES || node.ancestors().nth(MAX_HTML_DEPTH).is_some() {
            return None;
        }
    }
    Some(document)
}

fn visible(element: ElementRef<'_>) -> bool {
    std::iter::once(element)
        .chain(element.ancestors().filter_map(ElementRef::wrap))
        .all(|ancestor| {
            let value = ancestor.value();
            !matches!(
                value.name(),
                "head" | "script" | "style" | "template" | "noscript"
            ) && value.attr("hidden").is_none()
                && value.attr("inert").is_none()
                && !value
                    .attr("aria-hidden")
                    .is_some_and(|flag| flag.eq_ignore_ascii_case("true"))
                && value.attr("style").is_none()
        })
}

fn text(element: ElementRef<'_>) -> String {
    let mut output = String::new();
    for node in element.descendants() {
        if let (Some(content), Some(parent)) = (
            node.value().as_text(),
            node.parent().and_then(ElementRef::wrap),
        ) {
            if visible(parent) {
                output.push_str(content);
                output.push(' ');
            }
        }
    }
    output
}

fn normalized(value: &str) -> Option<String> {
    if value.chars().any(|ch| {
        (ch.is_control() && !ch.is_ascii_whitespace())
            || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return None;
    }
    Some(value.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn consent_label(label: &str) -> bool {
    normalized(label).is_some_and(|label| {
        matches!(
            label.trim_end_matches(['.', '!']).to_lowercase().as_str(),
            "i agree to the terms"
                | "i agree to the terms and conditions"
                | "i accept the terms"
                | "i accept the terms and conditions"
                | "i agree to the terms of use"
                | "i accept the terms of use"
                | "sutinku su taisyklėmis"
                | "sutinku su naudojimosi sąlygomis"
        )
    })
}

fn connect_label(label: &str, checkbox: bool) -> bool {
    normalized(label).is_some_and(|label| {
        let label = label.to_lowercase();
        matches!(
            label.as_str(),
            "agree and connect"
                | "accept and connect"
                | "agree & connect"
                | "accept & connect"
                | "accept terms and connect"
                | "accept terms and conditions and connect"
        ) || (checkbox
            && matches!(
                label.as_str(),
                "connect" | "connect to wi-fi" | "connect to wifi" | "get online"
            ))
    })
}

fn checkbox_label(document: &Html, input: ElementRef<'_>) -> Option<String> {
    if let Some(id) = input.value().id() {
        if document
            .select(&select("[id]"))
            .filter(|element| element.value().id() == Some(id))
            .take(2)
            .count()
            != 1
        {
            return None;
        }
    }
    let label_selector = select("label");
    let mut labels = document.select(&label_selector).filter(|label| {
        visible(*label)
            && match label.value().attr("for") {
                Some(target) => input.value().id() == Some(target),
                None => input.ancestors().any(|node| node.id() == label.id()),
            }
    });
    let label = labels.next()?;
    labels.next().is_none().then(|| text(label))
}

// LAWA's March 2021 public board report shows this free option and its
// consent notice together. Match those visible semantics only when supplied
// by a regular form; the report does not establish a Boingo wire protocol.
fn free_offer_label(label: &str, form: ElementRef<'_>) -> bool {
    normalized(label).is_some_and(|label| label.eq_ignore_ascii_case("Free Unlimited Wi-Fi"))
        && normalized(&text(form)).is_some_and(|value| {
            value
                .to_ascii_lowercase()
                .contains("by selecting, you agree to the terms and conditions")
        })
}

fn requires_ad(page: ElementRef<'_>) -> bool {
    normalized(&text(page)).is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        [
            "watch ad to connect",
            "watch an ad to connect",
            "watch a video to connect",
            "view an ad to connect",
            "view an advertisement to connect",
            "must watch",
            "must view",
            "skip ad",
            "skip sponsor message",
        ]
        .iter()
        .any(|phrase| value.contains(phrase))
    })
}

fn hidden_session_field(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "_csrf"
            | "_token"
            | "csrf"
            | "csrf_token"
            | "csrfmiddlewaretoken"
            | "authenticity_token"
            | "xsrf_token"
            | "token"
            | "nonce"
            | "session"
            | "session_id"
            | "sessionid"
            | "landing_url"
            | "continue_url"
            | "return_url"
            | "redirect_url"
    )
}

// Published LTG Link guest form signature. This only interprets controls
// present in the current page and uses the same explicit consent as any
// other portal. See Jonah Brüchert's freewifid provider example, 2025-08-29.
fn ltg_signature(action: &Url) -> bool {
    action.scheme() == "http"
        && action.host_str() == Some("192.168.1.100")
        && action.port() == Some(8880)
        && action.path() == "/guest/s/default/login"
}

fn parse_offer(page: Url, body: &[u8]) -> Result<Offer, &'static str> {
    let document = parse_html(body).ok_or("unsupported_html_encoding_or_size")?;
    if document
        .select(&select(
            "base[href], [form], [onsubmit], iframe, object, embed, video, audio, meta[http-equiv=refresh i], .g-recaptcha, .h-captcha, .cf-turnstile",
        ))
        .next()
        .is_some()
    {
        return Err("embedded_content_or_scripted_form");
    }
    if requires_ad(document.root_element()) {
        return Err("sponsored_step");
    }

    let form_selector = select("form");
    let forms: Vec<_> = document.select(&form_selector).collect();
    let [first, rest @ ..] = forms.as_slice() else {
        return Err("no_html_form");
    };
    if rest.is_empty() {
        return offer_from_form(&document, page, *first, false);
    }

    // A documented free choice may share a screen with login and purchase
    // forms. Every other form must be a distinct, clearly labeled alternative.
    let mut offers = forms.iter().filter_map(|form| {
        offer_from_form(&document, page.clone(), *form, true)
            .ok()
            .map(|offer| (*form, offer))
    });
    let (free, offer) = offers.next().ok_or("no_distinct_free_form")?;
    if offers.next().is_some() {
        return Err("ambiguous_free_forms");
    }

    let mut actions = vec![offer.action.clone()];
    for sibling in forms.iter().copied().filter(|form| form.id() != free.id()) {
        if sibling.parent().map(|n| n.id()) != free.parent().map(|n| n.id()) {
            return Err("separate_form_containers");
        }
        let action = alternative_action(&page, sibling).ok_or("unrecognized_sibling_form")?;
        if actions.contains(&action) {
            return Err("duplicate_form_action");
        }
        actions.push(action);
    }
    Ok(offer)
}

fn submit_caption(control: ElementRef<'_>) -> Option<String> {
    if control.value().name() == "button" {
        Some(text(control))
    } else {
        Some(control.value().attr("value")?.to_owned())
    }
}

fn inline_behavior(element: ElementRef<'_>) -> bool {
    element
        .value()
        .attrs()
        .any(|(name, _)| name.starts_with("on"))
}

fn alternative_action(page: &Url, form: ElementRef<'_>) -> Option<Url> {
    if !visible(form)
        || inline_behavior(form)
        || form
            .descendants()
            .filter_map(ElementRef::wrap)
            .any(inline_behavior)
        || !form.value().attr("method")?.eq_ignore_ascii_case("post")
        || form
            .value()
            .attr("enctype")
            .is_some_and(|value| !value.eq_ignore_ascii_case("application/x-www-form-urlencoded"))
        || form
            .select(&select(
                "[formaction], [formmethod], [formenctype], [formnovalidate], input[type=image i]",
            ))
            .next()
            .is_some()
    {
        return None;
    }
    let action = resolved(page, form.value().attr("action")?)?;
    if !same_origin(page, &action) {
        return None;
    }
    let selector = select("button:not([type]), button[type=submit i], input[type=submit i]");
    let mut submits = form.select(&selector);
    let submit = submits.next()?;
    if submits.next().is_some() || !visible(submit) || disabled(submit) {
        return None;
    }
    let label = normalized(&submit_caption(submit)?)?.to_ascii_lowercase();
    let password_selector = select("input[type=password i]:not([disabled])");
    let mut passwords = form.select(&password_selector);
    let account = matches!(label.as_str(), "log in" | "sign in")
        && passwords
            .next()
            .is_some_and(|control| visible(control) && !disabled(control))
        && passwords.next().is_none();
    let paid = matches!(
        label.as_str(),
        "buy day pass" | "boingo day pass" | "boingo unlimited"
    );
    (account || paid).then_some(action)
}

fn disabled(element: ElementRef<'_>) -> bool {
    // Reject the entire disabled fieldset, including its first legend,
    // whose special browser semantics are unnecessary for these forms.
    element.value().attr("disabled").is_some()
        || element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|ancestor| {
                ancestor.value().name() == "fieldset" && ancestor.value().attr("disabled").is_some()
            })
}

fn offer_from_form(
    document: &Html,
    page: Url,
    form: ElementRef<'_>,
    require_free: bool,
) -> Result<Offer, &'static str> {
    if !visible(form)
        || inline_behavior(form)
        || (require_free
            && form
                .descendants()
                .filter_map(ElementRef::wrap)
                .any(inline_behavior))
    {
        return Err("hidden_or_scripted_form");
    }
    let value = form.value();
    let action =
        resolved(&page, value.attr("action").unwrap_or("")).ok_or("invalid_form_action")?;
    if !same_origin(&page, &action) {
        return Err("foreign_form_action");
    }
    let method = match value
        .attr("method")
        .unwrap_or("get")
        .to_ascii_lowercase()
        .as_str()
    {
        "get" => "GET",
        "post" => "POST",
        _ => return Err("unsupported_form_method"),
    };
    if value
        .attr("enctype")
        .is_some_and(|value| !value.eq_ignore_ascii_case("application/x-www-form-urlencoded"))
    {
        return Err("unsupported_form_encoding");
    }

    let mut fields = Vec::new();
    let mut submit = None;
    let mut agreement = None;
    let ltg = method == "POST" && ltg_signature(&action);

    for control in form.select(&select("input, button, textarea, select")) {
        let value = control.value();
        if disabled(control) || value.attr("onclick").is_some() || value.attr("onchange").is_some()
        {
            return Err("disabled_or_scripted_control");
        }
        let name = value.attr("name").filter(|name| !name.is_empty());
        let kind = value.attr("type").unwrap_or(if value.name() == "button" {
            "submit"
        } else {
            "text"
        });
        let field = value.attr("value");
        match (value.name(), kind.to_ascii_lowercase().as_str()) {
            ("input", "hidden") => {
                let name = name.ok_or("unnamed_hidden_field")?;
                // A venue may include its own location in a free access form.
                // Defer admitting it until the submit and consent text are known.
                if !hidden_session_field(name) && !name.eq_ignore_ascii_case("location") {
                    return Err("unknown_hidden_field");
                }
                fields.push((name.to_owned(), field.unwrap_or("").to_owned()));
            }
            ("input", "checkbox") => {
                let label = checkbox_label(document, control);
                if label.as_deref().is_some_and(consent_label) {
                    if !visible(control) || agreement.is_some() || value.attr("checked").is_some() {
                        return Err("ambiguous_consent_checkbox");
                    }
                    agreement = Some((name.ok_or("unnamed_consent_field")?, field.unwrap_or("on")));
                } else if value.attr("required").is_some() || value.attr("checked").is_some() {
                    return Err("other_required_or_checked_option");
                }
                // Unselected optional choices are never submitted.
            }
            ("input" | "button", "submit") => {
                if !visible(control)
                    || submit.is_some()
                    || ["formaction", "formmethod", "formenctype", "formnovalidate"]
                        .iter()
                        .any(|attr| value.attr(attr).is_some())
                {
                    return Err("ambiguous_submit_button");
                }
                let label = submit_caption(control).ok_or("missing_submit_caption")?;
                submit = Some((name, field, label));
            }
            ("textarea", _) if value.attr("readonly").is_some() && name.is_none() => {}
            _ => return Err("unsupported_input_control"),
        }
    }

    let (name, field, label) = submit.ok_or("missing_submit_button")?;
    let known_ltg = ltg
        && normalized(&label)
            .ok_or("invalid_button_caption")?
            .eq_ignore_ascii_case("PRISIJUNGTI")
        && name == Some("accept")
        && field == Some("PRISIJUNGTI")
        && agreement == Some(("checkbox", "on"))
        && fields
            .iter()
            .any(|(key, value)| key == "landing_url" && !value.is_empty());
    // The Allied Telesis TQ5403 guide, Figure 54, documents a click-through
    // POST back to the displayed terms page with one submit input labeled
    // "Agree". The agreement text is irrelevant to the form shape.
    let simple_agreement = method == "POST"
        && value.attr("action").is_none()
        && normalized(&label).is_some_and(|label| label.eq_ignore_ascii_case("Agree"))
        && document
            .select(&select("title"))
            .next()
            .is_some_and(|title| {
                normalized(&title.text().collect::<String>())
                    .is_some_and(|title| title.eq_ignore_ascii_case("Terms of Service"))
            })
        && form
            .select(&select("button, label, input[type=checkbox i], textarea"))
            .next()
            .is_none();
    let free_offer = method == "POST" && free_offer_label(&label, form);
    if !free_offer
        && fields
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("location"))
    {
        return Err("unexpected_location_field");
    }
    if !(free_offer
        || (!require_free
            && (known_ltg || simple_agreement || connect_label(&label, agreement.is_some()))))
    {
        return Err("unrecognized_consent_label");
    }
    if let Some((name, value)) = agreement {
        fields.push((name.to_owned(), value.to_owned()));
    }
    if let Some(name) = name {
        fields.push((name.to_owned(), field.unwrap_or("").to_owned()));
    }
    for (index, (name, _)) in fields.iter().enumerate() {
        if fields[..index].iter().any(|(prior, _)| prior == name) {
            return Err("duplicate_field_name");
        }
    }
    if fields.len() > 16 || encoded_fields(&fields).len() > MAX_FORM_BYTES {
        return Err("form_size_limit");
    }

    let expires = Instant::now() + OFFER_LIFE;
    Ok(Offer {
        page,
        action,
        method,
        fields,
        expires,
    })
}

fn encoded_fields(fields: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            fields
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .finish()
}

/// Submit a consented offer exactly once. The transport must check the
/// original association immediately before requests and keep session cookies in
/// memory. An HTTP response accepts the request, independently of admission.
pub fn accept(http: &mut impl Transport, offer: Offer) -> Result<(), String> {
    if Instant::now() >= offer.expires {
        return Err("Portal offer expired. Retry Wi-Fi sign-in.".into());
    }
    let body = encoded_fields(&offer.fields);
    let mut action = offer.action;
    let origin = offer.page.origin().ascii_serialization();
    let response = if offer.method == "GET" {
        action.set_query(Some(&body));
        if action.as_str().len() > MAX_URL_BYTES {
            return Err("Portal request is too large.".into());
        }
        http.request(
            "GET",
            action.as_str(),
            &[("Cache-Control", "no-store")],
            &[],
        )
    } else {
        http.request(
            "POST",
            action.as_str(),
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Cache-Control", "no-store"),
                ("Origin", origin.as_str()),
                ("Referer", offer.page.as_str()),
            ],
            body.as_bytes(),
        )
    }
    .map_err(|_| "Could not reach the Wi-Fi sign-in page.".to_owned())?;

    if (200..300).contains(&response.status) || redirect(response.status) {
        Ok(())
    } else {
        Err("The Wi-Fi portal rejected the request.".into())
    }
}

#[cfg(test)]
#[path = "guest_tests.rs"]
mod tests;
