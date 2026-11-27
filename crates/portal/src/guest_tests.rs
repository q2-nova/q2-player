use super::*;
use std::collections::VecDeque;

const PAGE: &str = "http://portal.example/welcome";
const TERMS: &str = "Wi-Fi is for personal use. Keep your devices secure.";
const FORM: &str = r#"
<main>
  <section id="terms">Wi-Fi is for personal use. Keep your devices secure.</section>
  <form action="/connect" method="post">
    <input name="csrf_token" type="hidden" value="fresh&amp;one">
    <label for="ok">I agree to the terms</label>
    <input id="ok" name="terms" type="checkbox" required>
    <label><input name="marketing" type="checkbox">Send me offers</label>
    <button type="submit" name="action" value="join">Agree &amp; connect</button>
  </form>
</main>
"#;

#[derive(Default)]
struct Fake {
    responses: VecDeque<(&'static str, &'static str, Result<Response, String>)>,
    writes: Vec<String>,
}

impl Fake {
    fn reply(
        &mut self,
        method: &'static str,
        url: &'static str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) {
        self.responses.push_back((
            method,
            url,
            Ok(Response {
                status,
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
                body: body.as_bytes().to_vec(),
            }),
        ));
    }

    fn failed(&mut self, method: &'static str, url: &'static str) {
        self.responses
            .push_back((method, url, Err("fake timeout".into())));
    }

    fn html(&mut self, url: &'static str, body: &str) {
        self.reply(
            "GET",
            url,
            200,
            &[("Content-Type", "text/html; charset=UTF-8")],
            body,
        );
    }

    fn redirect(&mut self, url: &'static str, target: &'static str) {
        self.reply("GET", url, 302, &[("Location", target)], "");
    }
}

impl Transport for Fake {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, String> {
        let (expected_method, expected_url, response) = self
            .responses
            .pop_front()
            .expect("unexpected portal request");
        assert_eq!((method, url), (expected_method, expected_url));
        if method == "POST" {
            assert!(headers.contains(&("Content-Type", "application/x-www-form-urlencoded")));
            self.writes.push(String::from_utf8(body.to_vec()).unwrap());
        }
        response
    }
}

fn offer(http: &mut Fake) -> Offer {
    match detect(http, None).unwrap() {
        Detection::Guest(offer) => offer,
        _ => panic!("expected a guest offer"),
    }
}

#[test]
fn guest_form_uses_fresh_session_fields_only_after_consent() {
    let mut http = Fake::default();
    http.redirect(PROBE, PAGE);
    http.html(PAGE, FORM);
    let offer = offer(&mut http);
    assert!(http.writes.is_empty());
    http.reply(
        "POST",
        "http://portal.example/connect",
        303,
        &[("location", PROBE)],
        "",
    );
    accept(&mut http, offer).unwrap();
    assert_eq!(http.writes, ["csrf_token=fresh%26one&terms=on&action=join"]);

    http.reply("GET", PROBE, 200, &[], "success\n");
    assert!(matches!(
        detect(&mut http, None).unwrap(),
        Detection::Online
    ));
    assert!(http.responses.is_empty());
}

#[test]
fn intercepted_probe_can_advertise_the_portal_in_a_refresh_tag() {
    for status in [200, 511] {
        let mut http = Fake::default();
        let head =
            format!(r#"<head><meta http-equiv="Refresh" content="0; URL = '{PAGE}'"></head>"#);
        http.reply(
            "GET",
            PROBE,
            status,
            &[("Content-Type", "text/html")],
            &head,
        );
        http.html(PAGE, FORM);
        let _offer = offer(&mut http);
        assert!(http.writes.is_empty() && http.responses.is_empty());
    }
}

#[test]
fn refresh_only_follows_one_immediate_external_http_destination() {
    let probe = Url::parse(PROBE).unwrap();
    for value in [
        "10;url=https://operator.example",
        "0;url=/login",
        "0;url=http:operator.example/login",
        "0;url=https:///operator.example/login",
        "0;url=file:///login",
        "0;url=https://user@operator.example",
        "0;url='https://operator.example",
        "0;url=http://detectportal.firefox.com/form",
    ] {
        let head = format!(r#"<head><meta http-equiv="refresh" content="{value}"></head>"#);
        assert!(probe_refresh(&probe, head.as_bytes()).is_none());
    }
    let tag = format!(r#"<meta http-equiv="refresh" content="0;url={PAGE}">"#);
    for html in [
        format!("<head>{tag}{tag}</head>"),
        format!("<body>{tag}</body>"),
        format!("<head><template>{tag}</template></head>"),
    ] {
        assert!(probe_refresh(&probe, html.as_bytes()).is_none());
    }
}

#[test]
fn ambiguous_forms_make_no_submission() {
    for body in [
        FORM.replace("type=\"checkbox\">Send", "type=\"checkbox\" checked>Send"),
        FORM.replace("type=\"checkbox\">Send", "type=\"checkbox\" required>Send"),
        FORM.replace(
            "type=\"checkbox\" required",
            "type=\"checkbox\" hidden required",
        ),
        FORM.replace("csrf_token", "send_promotions"),
        FORM.replace("name=\"marketing\"", "name=\"password\"")
            .replace("\"checkbox\">Send", "\"password\">Send"),
        FORM.replace("/connect", "https://other.example/connect"),
        FORM.replace(
            "type=\"submit\"",
            "type=\"submit\" formaction=\"/alternate\"",
        ),
        FORM.replace("Agree &amp; connect", "Buy Day Pass"),
        format!("{FORM}<form method=post></form>"),
    ] {
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(PAGE, &body);
        assert!(matches!(
            detect(&mut http, None).unwrap(),
            Detection::Unsupported
        ));
        assert!(http.responses.is_empty());
        assert!(http.writes.is_empty());
    }
}

#[test]
fn back_or_expiry_cannot_submit() {
    for expired in [false, true] {
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(PAGE, FORM);
        let mut offer = offer(&mut http);
        if expired {
            offer.expires = Instant::now();
            assert!(accept(&mut http, offer).is_err());
        } else {
            drop(offer);
        }
        assert!(http.writes.is_empty());
        assert!(http.responses.is_empty());
    }
}

#[test]
fn ltg_uses_fresh_fields_and_visible_lithuanian_consent() {
    const LTG_PAGE: &str = "http://192.168.1.100:8880/guest/s/default/";
    const LTG_LOGIN: &str = "http://192.168.1.100:8880/guest/s/default/login";
    let body = r#"
      <p class="terms">Belaidis internetas skirtas keleivių asmeniniam naudojimui traukinyje.</p>
      <form method="post" action="login">
        <input name="landing_url" type="hidden" value="http://example.test/welcome">
        <label><input name="checkbox" required type="checkbox"> Sutinku su taisyklėmis</label>
        <input type="submit" value="PRISIJUNGTI" name="accept">
      </form>
    "#;
    let mut http = Fake::default();
    http.redirect(PROBE, LTG_PAGE);
    http.html(LTG_PAGE, body);
    let offer = offer(&mut http);
    http.reply("POST", LTG_LOGIN, 200, &[], "");
    accept(&mut http, offer).unwrap();
    assert_eq!(
        http.writes,
        ["landing_url=http%3A%2F%2Fexample.test%2Fwelcome&checkbox=on&accept=PRISIJUNGTI"]
    );

    http.redirect(PROBE, PAGE);
    http.html(PAGE, body);
    assert!(matches!(
        detect(&mut http, None).unwrap(),
        Detection::Unsupported
    ));
}

#[test]
fn capport_observes_tls_and_distinguishes_admission_from_failed_discovery() {
    const CAPPORT: &str = "https://portal.example/api";
    const HTTPS_PORTAL: &str = "https://portal.example/welcome";
    let mut http = Fake::default();
    http.reply(
        "GET",
        CAPPORT,
        200,
        &[("Content-Type", CAPPORT_MEDIA)],
        r#"{"captive":false}"#,
    );
    http.reply("GET", PROBE, 200, &[], "success\n");
    assert!(matches!(
        detect(&mut http, Some(CAPPORT)).unwrap(),
        Detection::Online
    ));
    http.reply(
        "GET",
        CAPPORT,
        200,
        &[("Content-Type", CAPPORT_MEDIA)],
        r#"{"captive":false}"#,
    );
    http.failed("GET", PROBE);
    assert!(matches!(
        detect(&mut http, Some(CAPPORT)).unwrap(),
        Detection::Unknown
    ));

    http.reply(
        "GET",
        CAPPORT,
        200,
        &[("Content-Type", CAPPORT_MEDIA)],
        r#"{"captive":true,"user-portal-url":"https://portal.example/welcome"}"#,
    );
    http.html(HTTPS_PORTAL, FORM);
    assert!(matches!(
        detect(&mut http, Some(CAPPORT)).unwrap(),
        Detection::Guest(_)
    ));

    http.reply(
        "GET",
        CAPPORT,
        200,
        &[("Content-Type", CAPPORT_MEDIA)],
        r#"{"captive":true,"user-portal-url":"http://portal.example/welcome"}"#,
    );
    http.reply("GET", PROBE, 200, &[], "success\n");
    assert!(matches!(
        detect(&mut http, Some(CAPPORT)).unwrap(),
        Detection::Unsupported
    ));

    http.failed("GET", CAPPORT);
    http.failed("GET", PROBE);
    assert!(matches!(
        detect(&mut http, Some(CAPPORT)).unwrap(),
        Detection::Unknown
    ));
    assert!(http.responses.is_empty());
}

#[test]
fn legacy_detects_delta_redirect_and_bounds_redirect_loops() {
    let mut http = Fake::default();
    http.redirect(PROBE, "https://wifi.delta.com/");
    assert!(matches!(detect(&mut http, None).unwrap(), Detection::Delta));

    http.redirect(PROBE, PAGE);
    for _ in 0..MAX_REDIRECTS {
        http.redirect(PAGE, PAGE);
    }
    assert!(matches!(
        detect(&mut http, None).unwrap(),
        Detection::Unsupported
    ));
    assert!(http.responses.is_empty());
}

#[test]
fn terms_documents_are_neither_required_nor_requested() {
    for terms in [
        "",
        r#"<a href="/terms">Terms and conditions</a>"#,
        r#"<a href="https://operator.example/policy.pdf">Terms of Service</a>"#,
    ] {
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(
            PAGE,
            &FORM.replace(&format!("<section id=\"terms\">{TERMS}</section>"), terms),
        );
        let offer = offer(&mut http);
        assert!(http.responses.is_empty());
        assert!(http.writes.is_empty());
        http.reply("POST", "http://portal.example/connect", 204, &[], "");
        accept(&mut http, offer).unwrap();
        assert_eq!(http.writes, ["csrf_token=fresh%26one&terms=on&action=join"]);
        assert!(http.responses.is_empty());
    }
}

// Only the visible caption and notice are based on the public LAWA report.
// The HTML structure, action and request field below are generated.
const FREE_SELECTION: &str = r#"
<form action="/connect" method="post">
  <input name="csrf_token" type="hidden" value="generated&amp;value">
  <button type="submit">Free Unlimited Wi-Fi</button>
  <p>By selecting, you agree to the <a href="/policy.pdf">terms and conditions</a>.</p>
</form>
<nav><a href="/account">Log In</a><a href="/paid">Buy Day Pass</a></nav>
"#;

#[test]
fn visible_free_selection_with_consent_posts_only_present_session_fields() {
    let mut http = Fake::default();
    http.redirect(PROBE, PAGE);
    http.html(PAGE, FREE_SELECTION);
    let offer = offer(&mut http);
    assert!(http.responses.is_empty() && http.writes.is_empty());
    http.reply(
        "POST",
        "http://portal.example/connect",
        303,
        &[("location", "/sponsor")],
        "",
    );
    accept(&mut http, offer).unwrap();
    assert_eq!(http.writes, ["csrf_token=generated%26value"]);
    assert!(http.responses.is_empty());
}

#[test]
fn free_caption_without_consent_or_with_an_extra_requirement_is_unsupported() {
    for body in [
        FREE_SELECTION.replace("By selecting, you agree to the ", "Review our "),
        FREE_SELECTION.replace("<p>", "<p hidden>"),
        FREE_SELECTION.replace("Free Unlimited Wi-Fi", "Watch Ad to Connect"),
        FREE_SELECTION.replace("Free Unlimited Wi-Fi", "Boingo Day Pass"),
        FREE_SELECTION.replace("<form ", "<form onsubmit=watchAd() "),
        FREE_SELECTION.replace("/connect", "https://checkout.example/subscribe"),
        FREE_SELECTION.replace("csrf_token", "price"),
        FREE_SELECTION.replace(
            "</form>",
            r#"<input type="email" name="email" required></form>"#,
        ),
        format!("{FREE_SELECTION}<aside>To use free Wi-Fi, watch an ad to connect.</aside>"),
        format!("{FREE_SELECTION}<video src=/sponsor.mp4></video>"),
        format!("{FREE_SELECTION}<form method=post><input type=password></form>"),
    ] {
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(PAGE, &body);
        assert!(matches!(
            detect(&mut http, None).unwrap(),
            Detection::Unsupported
        ));
        assert!(http.responses.is_empty() && http.writes.is_empty());
    }
}

// LAWA documents distinct choices on its screen. Form ownership, request
// paths, and field values here are generated, with colliding CSRF field names
// to detect an accidental read of sibling controls.
const ACCOUNT_AND_PURCHASE: &str = r#"
<form action="/account" method="post">
  <input name="csrf_token" type="hidden" value="account-only">
  <label>Email <input name="email" type="email" required></label>
  <label>Password <input name="password" type="password" required></label>
  <button type="submit">Log In</button>
</form>
<form action="/paid" method="post">
  <input name="csrf_token" type="hidden" value="purchase-only">
  <input name="plan" type="hidden" value="day-pass">
  <input type="submit" value="Buy Day Pass">
</form>
"#;

#[test]
fn one_free_form_among_separate_account_and_paid_forms_submits_no_sibling_fields() {
    let mut http = Fake::default();
    http.redirect(PROBE, PAGE);
    http.html(
        PAGE,
        &format!("<main>{ACCOUNT_AND_PURCHASE}{FREE_SELECTION}</main>"),
    );
    let offer = offer(&mut http);
    assert!(http.responses.is_empty() && http.writes.is_empty());
    http.reply("POST", "http://portal.example/connect", 204, &[], "");
    accept(&mut http, offer).unwrap();
    assert_eq!(http.writes, ["csrf_token=generated%26value"]);
    assert!(http.responses.is_empty());
}

#[test]
fn mixed_forms_must_have_one_free_offer_and_unambiguous_alternatives() {
    let mixed = format!("<main>{FREE_SELECTION}{ACCOUNT_AND_PURCHASE}</main>");
    for body in [
        format!("{mixed}{FREE_SELECTION}"),
        format!("<main>{FREE_SELECTION}{FORM}{ACCOUNT_AND_PURCHASE}</main>"),
        mixed.replace("Free Unlimited Wi-Fi", "Agree &amp; Connect"),
        mixed.replace(
            "<nav>",
            "<nav><input form=account name=token value=cross-owned>",
        ),
        mixed.replace(
            "action=\"/account\"",
            "action=\"https://other.example/login\"",
        ),
        mixed.replace("action=\"/account\"", "action=\"/connect\""),
        mixed.replace("action=\"/account\"", "onclick=login() action=\"/account\""),
        mixed.replace("action=\"/paid\"", "hidden action=\"/paid\""),
        mixed.replace(
            "type=\"submit\" value=\"Buy",
            "disabled type=\"submit\" value=\"Buy",
        ),
        mixed.replace("type=\"password\"", "type=\"text\""),
        mixed.replace(
            "action=\"/connect\"",
            "onkeydown=skip() action=\"/connect\"",
        ),
        mixed.replace("Free Unlimited Wi-Fi", "Watch Ad to Connect"),
    ] {
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(PAGE, &body);
        assert!(matches!(
            detect(&mut http, None).unwrap(),
            Detection::Unsupported
        ));
        assert!(http.responses.is_empty() && http.writes.is_empty());
    }
}

#[test]
fn failed_accept_is_not_retried_or_considered_admission() {
    let mut http = Fake::default();
    http.redirect(PROBE, PAGE);
    http.html(PAGE, FORM);
    let offer = offer(&mut http);
    http.failed("POST", "http://portal.example/connect");
    assert!(accept(&mut http, offer).is_err());
    assert_eq!(http.writes.len(), 1);
    assert!(http.responses.is_empty());
}

// Same structure as the Allied Telesis TQ5403 guide's click-through form,
// with a generated agreement and generated values.
const CLICK_THROUGH: &str = r#"
<!doctype html><title>Terms of Service</title>
<form method="post">
Wi-Fi is for personal use.<br>Keep your devices secure.
<br><br><input type="submit" value="Agree">
</form>
"#;

#[test]
fn inline_click_through_submits_the_visible_form_to_itself_after_consent() {
    for (submit, body) in [
        ("value=\"Agree\"", ""),
        ("name=\"consent\" value=\"Agree\"", "consent=Agree"),
    ] {
        let click_through = CLICK_THROUGH
            .replace(
                "Wi-Fi is for personal use.<br>Keep your devices secure.",
                "",
            )
            .replace(
                "<br><br>",
                r#"<a href="https://operator.example/policy.pdf">Terms of Service</a>"#,
            );
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(PAGE, &click_through.replace("value=\"Agree\"", submit));
        let reviewed = offer(&mut http);
        assert!(http.writes.is_empty());

        // Leaving the agreement behind has no effect on the portal.
        drop(reviewed);
        assert!(http.responses.is_empty());
        assert!(http.writes.is_empty());

        http.redirect(PROBE, PAGE);
        http.html(PAGE, &click_through.replace("value=\"Agree\"", submit));
        let reviewed_again = offer(&mut http);
        http.reply("POST", PAGE, 204, &[], "");
        accept(&mut http, reviewed_again).unwrap();
        assert_eq!(http.writes, [body]);
        assert!(http.responses.is_empty());
    }
}

#[test]
fn inline_click_through_rejects_hidden_or_ambiguous_agreements() {
    for body in [
        CLICK_THROUGH.replace("Terms of Service", "Newsletter"),
        CLICK_THROUGH.replace("method=\"post\"", "method=\"get\""),
        CLICK_THROUGH.replace("method=\"post\"", "method=\"post\" action=\"/confirm\""),
        CLICK_THROUGH.replace("<input type=\"submit\"", "<input hidden type=\"submit\""),
        CLICK_THROUGH.replace(
            "<br><br>",
            "<input type=\"checkbox\" name=\"marketing\" checked>",
        ),
    ] {
        let mut http = Fake::default();
        http.redirect(PROBE, PAGE);
        http.html(PAGE, &body);
        assert!(matches!(
            detect(&mut http, None).unwrap(),
            Detection::Unsupported
        ));
        assert!(http.responses.is_empty());
        assert!(http.writes.is_empty());
    }
}
