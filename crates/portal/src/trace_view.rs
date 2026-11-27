//! Bounded inert projections for field diagnostics. Guest site names and UI
//! labels may be private venue information. Never serialize an input value.
use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};
use url::Url;

const MAX_BODY: usize = 128 * 1024;
const MAX_HTML_BODY: usize = 512 * 1024;
const MAX_OUTLINE_SIZE: usize = 32 * 1024;
const MAX_OUTLINE_ITEMS: usize = 144;
const MAX_ITEMS: usize = 24;
const MAX_BODY_VIEW: usize = 8 * 1024;
const FIELDS: &[&str] = &[
    "accept",
    "agree",
    "agreement",
    "auth",
    "client_id",
    "choice",
    "code",
    "code_challenge",
    "code_verifier",
    "continue",
    "csrf",
    "csrf_token",
    "email",
    "grant_type",
    "location",
    "name",
    "password",
    "redirect_uri",
    "rememberUser",
    "response_mode",
    "response_type",
    "scope",
    "session",
    "state",
    "terms",
    "token",
    "type",
    "username",
];
const KEYS: &[&str] = &[
    "access_token",
    "action",
    "actions",
    "authorizeResponse",
    "captive",
    "code",
    "deviceId",
    "deviceLanIp",
    "devicePolicy",
    "error",
    "errors",
    "expires_in",
    "flightId",
    "id",
    "message",
    "refresh_token",
    "status",
    "token_type",
    "user-portal-url",
];
const STATES: &[&str] = &[
    "COMPLETED",
    "FAILED",
    "IS_USER_REMEMBERED",
    "MFA_REQUIRED",
    "REMEMBERED_DEVICE_DETAILS",
    "SHOULD_REMEMBER_USER",
    "USERNAME_PASSWORD_REQUIRED",
];
const MEDIA_TYPES: &[&str] = &[
    "application/captive+json",
    "application/json",
    "application/xhtml+xml",
    "application/x-www-form-urlencoded",
    "text/html",
    "text/plain",
];

fn known(value: &str, choices: &'static [&'static str]) -> &'static str {
    choices
        .iter()
        .copied()
        .find(|choice| *choice == value)
        .unwrap_or("other")
}

fn identifier(value: &str) -> Option<&str> {
    let alpha = value.bytes().filter(u8::is_ascii_alphabetic).count();
    let digits = value.bytes().filter(u8::is_ascii_digit).count();
    (value.len() <= 40
        && alpha > 0
        && digits <= 4
        && !value.split(['-', '_', '.']).any(|part| part.len() > 23)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-[]".contains(&byte)))
    .then_some(value)
}

fn path_shape(path: &str) -> Vec<&str> {
    let mut redact_next = false;
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .take(12)
        .map(|segment| {
            let hide = redact_next;
            redact_next = matches!(
                segment.to_ascii_lowercase().as_str(),
                "account"
                    | "accounts"
                    | "auth"
                    | "code"
                    | "device"
                    | "devices"
                    | "flight"
                    | "flights"
                    | "flows"
                    | "session"
                    | "sessions"
                    | "state"
                    | "ticket"
                    | "tickets"
                    | "token"
                    | "tokens"
                    | "user"
                    | "users"
            );
            if hide {
                ":opaque"
            } else {
                identifier(segment).unwrap_or(":opaque")
            }
        })
        .collect()
}

fn address(url: &Url) -> Value {
    let origin = url
        .host_str()
        .filter(|host| host.len() <= 253)
        .unwrap_or("other");
    let path = url.path();
    let route = match (origin, path) {
        (_, "/") => "/",
        ("wifi.delta.com", "/api/device-details") => "/api/device-details",
        ("wifi.delta.com", "/api/flight-data") => "/api/flight-data",
        ("signin.delta.com", "/as/authorization.oauth2") => "/as/authorization.oauth2",
        ("signin.delta.com", "/as/token.oauth2") => "/as/token.oauth2",
        ("detectportal.firefox.com", "/success.txt") => "/success.txt",
        ("wifi.delta.com", route)
            if route.starts_with("/api/portableElectronicDevice/v1/flights/")
                && route.ends_with("/sessions") =>
        {
            "/api/portableElectronicDevice/v1/flights/:flight/devices/:device/sessions"
        }
        ("signin.delta.com", route) if route.starts_with("/pf-ws/authn/flows/") => {
            "/pf-ws/authn/flows/:flow"
        }
        (_, "/generate_204" | "/hotspot-detect.html") => {
            known(path, &["/generate_204", "/hotspot-detect.html"])
        }
        _ => "other",
    };
    let query: Vec<_> = url
        .query_pairs()
        .take(MAX_ITEMS)
        .map(|(name, _)| known(&name, FIELDS))
        .collect();
    json!({
        "scheme": known(url.scheme(), &["http", "https"]),
        "host": if matches!(url.scheme(), "http" | "https") { origin } else { "other" },
        "port": url.port(),
        "route": route,
        "path_shape": if route == "other" && matches!(url.scheme(), "http" | "https")
            && origin != "signin.delta.com" { path_shape(path) } else { Vec::new() },
        "path_segments": path.split('/').filter(|segment| !segment.is_empty()).count(),
        "query_fields": query,
        "has_fragment": url.fragment().is_some(),
    })
}

fn method(value: &str) -> &'static str {
    known(
        &value.to_ascii_uppercase(),
        &["GET", "POST", "PUT", "HEAD", "OPTIONS", "DELETE"],
    )
}

fn content_type(value: &str) -> &'static str {
    let media = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media.starts_with("application/vnd.pingidentity.") && media.ends_with("+json") {
        return "pingidentity-json";
    }
    known(&media, MEDIA_TYPES)
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn schema(value: &Value, depth: usize) -> Value {
    if depth >= 3 {
        return json!({"type": json_type(value)});
    }
    if let Some(object) = value.as_object() {
        let fields: Vec<_> = object
            .iter()
            .filter_map(|(key, field)| {
                let known_key = KEYS.contains(&key.as_str()) || FIELDS.contains(&key.as_str());
                let key = if known_key {
                    key.as_str()
                } else {
                    identifier(key)?
                };
                let shape = match key {
                    "status" => json!({
                        "type": json_type(field),
                        "state": known(field.as_str().unwrap_or(""), STATES),
                    }),
                    // Only known structural fields are traversed. Unknown objects
                    // can be dictionaries keyed by account, challenge or token.
                    "action" | "actions" | "authorizeResponse" => schema(field, depth + 1),
                    _ => json!({"type": json_type(field)}),
                };
                Some((key, shape))
            })
            .take(MAX_ITEMS)
            .collect();
        return json!({
            "type": "object",
            "fields": fields,
            "unknown_fields": object.keys().filter(|key| !KEYS.contains(&key.as_str()) && !FIELDS.contains(&key.as_str())).count(),
        });
    }
    if let Some(array) = value.as_array() {
        return json!({"type": "array", "length": array.len()});
    }
    json!({"type": json_type(value)})
}

fn select(query: &str) -> Selector {
    Selector::parse(query).expect("static trace selector")
}

fn semantic(text: &str) -> Vec<&'static str> {
    let words: String = text
        .chars()
        .take(256)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let words = words.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
    let words = format!(" {words} ");
    [
        (" accept ", "accept"),
        (" agree ", "agree"),
        (" terms ", "terms"),
        (" conditions ", "conditions"),
        (" free ", "free"),
        (" guest ", "guest"),
        (" continue ", "continue"),
        (" connect ", "connect"),
        (" sign in ", "sign-in"),
        (" log in ", "sign-in"),
        (" password ", "password"),
        (" email ", "email"),
        (" video ", "video"),
        (" advertisement ", "advertisement"),
        (" skip ", "skip"),
        (" error ", "error"),
        (" unavailable ", "unavailable"),
        (" failed ", "failed"),
        (" verification ", "verification"),
        (" captive ", "captive"),
    ]
    .into_iter()
    .filter_map(|(term, label)| words.contains(term).then_some(label))
    .collect()
}

fn marked_hidden(element: ElementRef<'_>) -> bool {
    std::iter::once(element)
        .chain(element.ancestors().filter_map(ElementRef::wrap))
        .any(|ancestor| {
            let node = ancestor.value();
            let style = node
                .attr("style")
                .unwrap_or("")
                .replace(' ', "")
                .to_ascii_lowercase();
            matches!(node.name(), "script" | "style" | "template")
                || node.attr("hidden").is_some()
                || node
                    .attr("aria-hidden")
                    .is_some_and(|flag| flag.eq_ignore_ascii_case("true"))
                || style.contains("display:none")
                || style.contains("visibility:hidden")
        })
}

fn caption(element: ElementRef<'_>) -> Value {
    if marked_hidden(element) {
        return Value::Null;
    }
    let node = element.value();
    let mut text: String = element
        .descendants()
        .filter_map(|child| {
            let text = child.value().as_text()?;
            let parent = child.parent().and_then(ElementRef::wrap)?;
            if parent.value().name() == "textarea" || marked_hidden(parent) {
                return None;
            }
            Some(text.text.as_ref())
        })
        .flat_map(str::chars)
        .take(192)
        .collect();
    for attr in ["aria-label", "title"] {
        if let Some(label) = node.attr(attr) {
            text.push(' ');
            text.extend(label.chars().take(96));
        }
    }
    if node.name() == "input"
        && node
            .attr("type")
            .is_some_and(|kind| kind.eq_ignore_ascii_case("submit"))
    {
        if let Some(value) = node.attr("value") {
            let approved = known(
                &value.trim().to_ascii_lowercase(),
                &[
                    "accept",
                    "agree",
                    "connect",
                    "continue",
                    "get connected",
                    "get online",
                    "i accept",
                    "i agree",
                    "log in",
                    "next",
                    "sign in",
                    "skip",
                ],
            );
            if approved != "other" {
                text.push(' ');
                text.push_str(approved);
            }
        }
    }
    let labels = semantic(&text);
    let words: Vec<_> = text
        .split_whitespace()
        .take(32)
        .map(|word| {
            // Common account, URL and token shapes can occur in rendered errors.
            if word.len() > 30
                || word.contains('@')
                || word.contains("://")
                || word.bytes().filter(u8::is_ascii_digit).count() >= 5
            {
                "[redacted]"
            } else {
                word
            }
        })
        .collect();
    json!({"text": words.join(" "), "labels": labels})
}

fn target(base: Option<&Url>, href: Option<&str>) -> Value {
    href.and_then(|href| {
        base.and_then(|base| base.join(href).ok())
            .or_else(|| Url::parse(href).ok())
    })
    .filter(|url| matches!(url.scheme(), "http" | "https"))
    .map(|url| address(&url))
    .unwrap_or(Value::Null)
}

// Signed or personalized asset identifiers can also be short alphabetic paths.
fn asset_target(base: Option<&Url>, href: Option<&str>) -> Value {
    let mut view = target(base, href);
    if let Some(parts) = view["path_shape"].as_array_mut() {
        for part in parts {
            if !matches!(part.as_str(), Some(
                "assets" | "static" | "public" | "dist" | "js" | "css" | "media"
                | "main.js" | "app.js" | "runtime.js" | "bundle.js" | "vendor.js"
                | "main.css" | "style.css" | "styles.css"
            )) {
                *part = json!(":opaque");
            }
        }
    }
    view
}

fn outline(doc: &Html, base: Option<&Url>, forms: &[ElementRef<'_>]) -> Value {
    let title = doc.select(&select("title")).next().map(caption_title);
    let items = select(concat!(
        "h1,h2,h3,h4,h5,h6,p,legend,label,form,button,input,select,textarea,a[href],",
        "[role=button],[role=alert],[role=status],[role=dialog],[onclick],noscript,",
        "script[src],link[rel=stylesheet][href],video,iframe",
    ));
    let mut nodes = Vec::new();
    let mut bytes = 0;
    let mut truncated = false;
    for element in doc.select(&items) {
        let node = element.value();
        let name = node.name();
        let hidden = marked_hidden(element)
            || node
                .attr("type")
                .is_some_and(|value| value.eq_ignore_ascii_case("hidden"));
        let asset = matches!(name, "script" | "link" | "iframe" | "video");
        let target_attr = match name {
            "script" | "iframe" | "video" => "src",
            "link" | "a" => "href",
            "form" => "action",
            _ => "formaction",
        };
        let detail = json!({
            "tag": known(name, &[
                "h1","h2","h3","h4","h5","h6","p","legend","label","form","button","input",
                "select","textarea","a","script","link","noscript","video","iframe","div","span",
            ]),
            "id": node.attr("id").and_then(identifier),
            "role": node.attr("role").filter(|role| matches!(*role, "button" | "status" | "alert" | "dialog")),
            "class": node.attr("class").unwrap_or("").split_ascii_whitespace().filter_map(identifier).take(6).collect::<Vec<_>>(),
            "form": form_index(element, forms),
            "type": node.attr("type").map(|value| known(&value.to_ascii_lowercase(),
                &["button","checkbox","email","file","hidden","module","password","radio","submit","tel","text"])),
            "name": node.attr("name").and_then(identifier),
            "method": node.attr("method").or_else(|| node.attr("formmethod")).map(method),
            "target": if asset { asset_target(base, node.attr(target_attr)) } else { target(base, node.attr(target_attr)) },
            "disabled": node.attr("disabled").is_some() || node.attr("aria-disabled") == Some("true"),
            "inline_handler": node.attr("onclick").is_some(),
            "hidden": hidden,
            "copy": if hidden || asset || matches!(name, "form" | "select" | "textarea") {
                Value::Null
            } else {
                caption(element)
            },
        });
        bytes += detail.to_string().len() + 1;
        if nodes.len() == MAX_OUTLINE_ITEMS || bytes > MAX_OUTLINE_SIZE {
            truncated = true;
            break;
        }
        nodes.push(detail);
    }
    json!({"title": title, "nodes": nodes, "truncated": truncated})
}

fn caption_title(element: ElementRef<'_>) -> Value {
    caption(element)["text"].clone()
}

fn form_index(control: ElementRef<'_>, forms: &[ElementRef<'_>]) -> Option<usize> {
    if let Some(id) = control.value().attr("form") {
        forms
            .iter()
            .position(|form| form.value().attr("id") == Some(id))
    } else {
        control
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find_map(|ancestor| forms.iter().position(|form| form.id() == ancestor.id()))
    }
}

fn html(base: Option<&Url>, bytes: &[u8]) -> Value {
    let doc = Html::parse_document(&String::from_utf8_lossy(bytes));
    let form_selector = select("form");
    let forms: Vec<_> = doc.select(&form_selector).take(MAX_ITEMS).collect();
    let form_views: Vec<_> = forms
        .iter()
        .map(|form| {
            let node = form.value();
            json!({"method": method(node.attr("method").unwrap_or("GET")),
               "action": target(base, node.attr("action").or(Some(""))),
               "id": node.attr("id").and_then(identifier),
               "hidden": marked_hidden(*form)})
        })
        .collect();
    let control_selector = select("input,button,select,textarea");
    let label_selector = select("label");
    let labels: Vec<_> = doc.select(&label_selector).take(MAX_ITEMS).collect();
    let controls: Vec<_> = doc
        .select(&control_selector)
        .take(MAX_ITEMS)
        .map(|control| {
            let node = control.value();
            let owner = form_index(control, &forms);
            let kind = node
                .attr("type")
                .unwrap_or(match node.name() {
                    "input" => "text",
                    "button" => "submit",
                    _ => node.name(),
                })
                .to_ascii_lowercase();
            let kind = known(
                &kind,
                &[
                    "text", "email", "password", "hidden", "submit", "button", "checkbox", "radio",
                    "number", "tel", "url", "search", "select", "textarea", "file",
                ],
            );
            let hidden = kind == "hidden" || marked_hidden(control);
            let labels: Vec<_> = labels
                .iter()
                .filter(|label| {
                    node.attr("id")
                        .is_some_and(|id| label.value().attr("for") == Some(id))
                        || control
                            .ancestors()
                            .any(|ancestor| ancestor.id() == label.id())
                })
                .filter(|_| !hidden)
                .take(4)
                .map(|label| caption(*label))
                .collect();
            let constraints: Vec<_> = [
                "required",
                "disabled",
                "readonly",
                "pattern",
                "maxlength",
                "minlength",
                "min",
                "max",
                "step",
                "checked",
                "autocomplete",
            ]
            .into_iter()
            .filter(|&name| node.attr(name).is_some())
            .collect();
            json!({
                "form": owner, "tag": known(node.name(), &["input","button","select","textarea"]),
                "kind": kind, "name": node.attr("name").and_then(identifier),
                "id": node.attr("id").and_then(identifier),
                "hidden": hidden, "constraints": constraints, "labels": labels,
                "caption": if hidden || matches!(kind, "textarea" | "password" | "select") {
                    Value::Null
                } else { caption(control) },
                "submit_action": target(base, node.attr("formaction")),
                "submit_method": node.attr("formmethod").map(method),
            })
        })
        .collect();
    let links: Vec<_> = doc
        .select(&select("a[href]"))
        .filter(|el| !marked_hidden(*el))
        .take(MAX_ITEMS)
        .map(|el| json!({"target": target(base, el.value().attr("href")), "caption": caption(el)}))
        .collect();
    let visible_text = doc
        .select(&select(
            "p, h1, h2, h3, legend, [role=alert], [role=status], noscript",
        ))
        .filter(|element| !marked_hidden(*element))
        .take(MAX_ITEMS)
        .map(caption)
        .filter(|caption| {
            caption["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        })
        .collect::<Vec<_>>();
    let landmarks = [
        "script",
        "script[src]",
        "noscript",
        "iframe",
        "video",
        "form",
        "[role=alert]",
        "[role=dialog]",
    ]
    .into_iter()
    .map(|query| (query, doc.select(&select(query)).take(MAX_ITEMS).count()))
    .collect::<Vec<_>>();
    let form_count = doc.select(&form_selector).count();
    let control_count = doc.select(&control_selector).count();
    let mut view = json!({"forms": form_views, "controls": controls, "links": links,
                          "visible_labels": visible_text, "landmarks": landmarks,
                          "form_count": form_count, "control_count": control_count,
                          "forms_truncated": form_count > MAX_ITEMS,
                          "controls_truncated": control_count > MAX_ITEMS});
    if view.to_string().len() > MAX_BODY_VIEW {
        view["truncated"] = Value::Bool(true);
        // Preserve the top-level HTTP metadata, page landmarks and the earliest
        // controls even on a long document. First trim repeated tail content.
        for key in ["visible_labels", "links", "controls", "forms"] {
            while view.to_string().len() > MAX_BODY_VIEW {
                if view[key].as_array_mut().and_then(Vec::pop).is_none() {
                    break;
                }
            }
        }
    }
    view["forms_truncated"] = json!(
        view["forms"]
            .as_array()
            .is_some_and(|items| items.len() != form_count)
    );
    view["controls_truncated"] = json!(
        view["controls"]
            .as_array()
            .is_some_and(|items| items.len() != control_count)
    );
    view["outline"] = outline(&doc, base, &forms);
    view
}

fn payload(url: Option<&Url>, media: &str, body: &[u8], response: bool) -> Value {
    let html_response = response && matches!(media, "text/html" | "application/xhtml+xml");
    let limit = if html_response {
        MAX_HTML_BODY
    } else {
        MAX_BODY
    };
    if body.len() > limit {
        return json!({"omitted": "size", "limit_bytes": limit});
    }
    match media {
        "application/x-www-form-urlencoded" if !response => json!({
            "fields": url::form_urlencoded::parse(body).take(MAX_ITEMS)
                .map(|(name, _)| known(&name, FIELDS)).collect::<Vec<_>>()
        }),
        "application/json" | "application/captive+json" | "pingidentity-json" => {
            let view = serde_json::from_slice(body)
                .map(|value| schema(&value, 0))
                .unwrap_or(Value::Null);
            if view.to_string().len() > MAX_BODY_VIEW {
                json!({"omitted": "projection-size", "type": json_type(&view)})
            } else {
                view
            }
        }
        "text/html" | "application/xhtml+xml" if response => {
            if url.is_none_or(|url| {
                known(url.host_str().unwrap_or(""), &["signin.delta.com"]) != "other"
            }) {
                json!({"omitted": "identity-or-invalid-url"})
            } else {
                html(url, body)
            }
        }
        _ => Value::Null,
    }
}

pub fn request(verb: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Value {
    let parsed = Url::parse(url).ok();
    let media = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map_or("other", |(_, media)| content_type(media));
    json!({"method": method(verb), "url": parsed.as_ref().map(address),
           "content_type": media, "body_bytes": body.len(),
           "body": payload(parsed.as_ref(), media, body, false)})
}

pub fn response_headers(url: &str, status: u16, headers: &[(String, String)]) -> Value {
    let parsed = Url::parse(url).ok();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let media = header("content-type").map_or("other", content_type);
    json!({"url": parsed.as_ref().map(address), "status": status, "content_type": media,
           "content_length": header("content-length").and_then(|n| n.parse::<u64>().ok()),
           "location": target(parsed.as_ref(), header("location"))})
}

pub fn response(url: &str, status: u16, headers: &[(String, String)], body: &[u8]) -> Value {
    let mut record = response_headers(url, status, headers);
    let parsed = Url::parse(url).ok();
    let media = record["content_type"].as_str().unwrap_or("other");
    let view = payload(parsed.as_ref(), media, body, true);
    record["body_bytes"] = json!(body.len());
    record["body"] = view;
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_views_never_retain_values_or_opaque_routes() {
        let url =
            "https://signin.delta.com/pf-ws/authn/flows/CANARY?code=CANARY&opaque=CANARY#CANARY";
        let output = request(
            "POST",
            url,
            &[
                ("Cookie", "CANARY"),
                ("Authorization", "Bearer CANARY"),
                ("UserAuth", "CANARY"),
                ("Content-Type", "application/vnd.pingidentity.login+json"),
            ],
            br#"{"username":"CANARY","password":"CANARY","opaqueExtension":"CANARY"}"#,
        );
        assert_eq!(output["url"]["route"], "/pf-ws/authn/flows/:flow");
        assert_eq!(output["body"]["unknown_fields"], 1);
        assert!(!output.to_string().contains("CANARY"));
        let output = response(
            url,
            302,
            &[
                ("Set-Cookie".into(), "CANARY".into()),
                (
                    "Location".into(),
                    "https://partner.example/session/0123456789abcdef?extra=CANARY#CANARY".into(),
                ),
                ("Content-Type".into(), "application/json".into()),
            ],
            br#"{"status":"USERNAME_PASSWORD_REQUIRED","id":"CANARY","error":{"CANARY":"CANARY"}}"#,
        );
        assert!(output.to_string().contains("USERNAME_PASSWORD_REQUIRED"));
        assert!(!output.to_string().contains("CANARY"));
    }

    #[test]
    fn form_shape_keeps_known_names_without_any_submitted_values() {
        let output = request(
            "POST",
            "https://CANARY:CANARY@airport.example/guest/session/CANARY?session=CANARY",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            b"username=CANARY&password=CANARY&csrf=CANARY",
        );
        assert_eq!(
            output["url"]["path_shape"],
            json!(["guest", "session", ":opaque"])
        );
        assert_eq!(
            output["body"]["fields"],
            json!(["username", "password", "csrf"])
        );
        assert!(!output.to_string().contains("CANARY"));
    }

    #[test]
    fn guest_projection_preserves_association_and_hides_provider_strings() {
        let output = response(
            "https://airport.example/path",
            200,
            &[("Content-Type".into(), "text/html".into())],
            br#"<form id="access" method="post" action="/connect"></form>
                <label for="field">Airport Terms &amp; Conditions</label>
                <input form="access" id="field" name="terms" type="checkbox" required>
                <input form="access" name="form_csrf" type="hidden" value="CANARY">
                <a href="/welcome#CANARY">Continue<script>CANARY</script></a>
                <script src="/CANARY"></script>"#,
        );
        assert_eq!(output["body"]["controls"][0]["form"], 0);
        assert_eq!(
            output["body"]["controls"][0]["labels"][0]["labels"],
            json!(["terms", "conditions"])
        );
        assert_eq!(output["body"]["controls"][1]["kind"], "hidden");
        assert_eq!(
            output["body"]["links"][0]["caption"]["labels"],
            json!(["continue"])
        );
        assert!(!output.to_string().contains("CANARY"));
    }

    #[test]
    fn limits_projected_page_size_without_losing_http_metadata() {
        let markup = format!("<form>{}</form>", (0..40).map(|index| {
            format!("<label for='choice-{index}'>{}</label><button id='choice-{index}'>{}</button>",
                    "airport terms apply ".repeat(24), "accept to connect ".repeat(24))
        }).collect::<String>());
        let output = response(
            "https://airport.example/start?session=CANARY",
            200,
            &[("Content-Type".into(), "text/html".into())],
            markup.as_bytes(),
        );
        assert_eq!(output["status"], 200);
        assert_eq!(output["url"]["host"], "airport.example");
        assert_eq!(output["body"]["controls"][0]["form"], 0);
        assert_eq!(output["body"]["truncated"], true);
        assert!(output.to_string().len() < 64 * 1024);
        assert!(!output.to_string().contains("CANARY"));
    }

    #[test]
    fn large_js_only_guest_page_has_inert_outline_and_assets() {
        let mut html = String::from(
            r#"<title>Airport Guest Portal</title>
            <div role="button" id="guest-start" onclick="CANARY">Get Online</div>
            <script src="https://cdn.airport.example/assets/main.js?secret=CANARY"></script>
            <script>CANARY</script><textarea>CANARY</textarea>
            <input type="hidden" name="token" value="CANARY">"#,
        );
        html.push_str(&"<!-- page padding -->".repeat(8000));
        html.push_str(&"<form><button>Continue</button></form>".repeat(MAX_ITEMS + 2));
        assert!(html.len() > MAX_BODY);
        let view = response(
            "https://airport.example/welcome",
            200,
            &[("Content-Type".into(), "text/html".into())],
            html.as_bytes(),
        );
        let body = &view["body"];
        assert_eq!(body["outline"]["title"], "Airport Guest Portal");
        assert_eq!(body["forms_truncated"], true);
        let nodes = body["outline"]["nodes"].as_array().unwrap();
        assert!(nodes.iter().any(|node| node["role"] == "button"
            && node["inline_handler"] == true
            && node["copy"]["text"] == "Get Online"));
        assert!(nodes.iter().any(|node| node["tag"] == "script"
            && node["target"]["host"] == "cdn.airport.example"
            && node["target"]["path_shape"] == json!(["assets", "main.js"])));
        assert!(!view.to_string().contains("CANARY"));
    }

    #[test]
    fn drops_large_and_identity_documents() {
        let headers = [("Content-Type".into(), "text/html".into())];
        let output = response(
            "https://signin.delta.com/",
            200,
            &headers,
            b"<form>CANARY</form>",
        );
        assert_eq!(output["body"]["omitted"], "identity-or-invalid-url");
        let output = response(
            "https://wifi.delta.com/",
            200,
            &headers,
            &vec![b'x'; MAX_HTML_BODY + 1],
        );
        assert_eq!(output["body"]["omitted"], "size");
    }
}
