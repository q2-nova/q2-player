// Wire protocol reference: chiesennegs/delta-wifi, MIT,
// commit 8758e1ed2812b5f3c34e52118de737e15ed446b9.

use crate::http::{Response, Transport};
use ring::{
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{Value, json};
use std::{net::Ipv4Addr, time::Duration};

const PORTAL: &str = "https://wifi.delta.com";
const SIGNIN: &str = "https://signin.delta.com";
const CLIENT_ID: &str = "FEVhKePVS3FfeBfxjBVNHmmy";
const PREMIUM: &str = "DalPedPremium";
const INTERACTION: &str =
    "Delta requires interaction. Sign in through Delta’s portal on the device you want to connect.";
const VIDEO_INTERACTION: &str = "Delta showed a video. This player still needs Wi-Fi access.";
const UNCONFIRMED: &str = "Delta has not confirmed access for this player.";
const DEVICE_CHANGED: &str = "Delta reported a different device. Sign-in stopped.";

fn request(
    http: &mut impl Transport,
    method: &str,
    endpoint: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Response, String> {
    let mut all = vec![
        ("Accept", "application/json"),
        ("Cache-Control", "no-store"),
        ("Origin", PORTAL),
        ("Referer", "https://wifi.delta.com/"),
    ];
    all.extend_from_slice(headers);
    // Transport errors can include request details. Only show fixed messages.
    let response = http
        .request(method, endpoint, &all, body)
        .map_err(|_| "Could not securely reach Delta’s sign-in service.".to_string())?;
    match response.status {
        200..=299 => Ok(response),
        300..=399 => Err(INTERACTION.into()),
        401 | 403 => Err("Delta denied sign-in or access. Check your SkyMiles details.".into()),
        429 => Err("Delta is limiting sign-in attempts. Try again later.".into()),
        _ => Err("Delta could not process the request.".into()),
    }
}

fn object(response: Response) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(&response.body).map_err(|_| INTERACTION)?;
    if !value.is_object() {
        return Err(INTERACTION.into());
    }
    Ok(value)
}

fn id(value: &Value) -> Result<&str, String> {
    let value = value.as_str().ok_or(INTERACTION)?;
    if value.is_empty()
        || value.len() > 256
        || matches!(value, "." | ".." | "null")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        return Err(INTERACTION.into());
    }
    Ok(value)
}

fn basic(policy: &str) -> bool {
    matches!(
        policy,
        "DalPedDefault" | "DalDppPedDefault" | "DalPedCna" | "DalPedBasic" | "DalDppPedCna"
    )
}

struct Device {
    id: String,
    policy: String,
}

fn device(http: &mut impl Transport, local_ip: Ipv4Addr) -> Result<Device, String> {
    let data = object(request(
        http,
        "GET",
        "https://wifi.delta.com/api/device-details",
        &[],
        &[],
    )?)?;
    if data["deviceLanIp"].as_str().and_then(|ip| ip.parse().ok()) != Some(local_ip) {
        return Err(DEVICE_CHANGED.into());
    }
    Ok(Device {
        id: id(&data["deviceId"])?.to_owned(),
        policy: data["devicePolicy"].as_str().ok_or(INTERACTION)?.into(),
    })
}

pub fn admitted(http: &mut impl Transport, local_ip: Ipv4Addr) -> Result<bool, String> {
    let state = device(http, local_ip)?;
    if state.policy == PREMIUM {
        return Ok(true);
    }
    if basic(&state.policy) {
        return Ok(false);
    }
    Err("Delta reported an unknown access policy.".into())
}

fn flight(http: &mut impl Transport) -> Result<String, String> {
    let data = object(request(
        http,
        "GET",
        "https://wifi.delta.com/api/flight-data",
        &[],
        &[],
    )?)?;
    Ok(id(&data["flightId"])?.to_owned())
}

fn random_hex() -> Result<String, String> {
    let mut random = [0u8; 32];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "Could not start a secure Delta sign-in.")?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn challenge(verifier: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let hash = digest(&SHA256, verifier.as_bytes());
    let mut encoded = String::with_capacity(43);
    for chunk in hash.as_ref().chunks(3) {
        let a = chunk[0];
        let b = chunk[1];
        encoded.push(ALPHABET[usize::from(a >> 2)] as char);
        encoded.push(ALPHABET[usize::from(((a & 3) << 4) | (b >> 4))] as char);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[usize::from(((b & 15) << 2) | (c >> 6))] as char);
        if chunk.len() == 3 {
            encoded.push(ALPHABET[usize::from(c & 63)] as char);
        }
    }
    encoded
}

fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish()
}

fn ping(
    http: &mut impl Transport,
    method: &str,
    endpoint: &str,
    content_type: &str,
    body: &[u8],
) -> Result<Value, String> {
    object(request(
        http,
        method,
        endpoint,
        &[
            ("Content-Type", content_type),
            ("X-XSRF-Header", "PingFederate"),
        ],
        body,
    )?)
}

fn flow_post(
    http: &mut impl Transport,
    state: &Value,
    action: &str,
    data: Value,
) -> Result<Value, String> {
    let flow = id(&state["id"])?;
    ping(
        http,
        "POST",
        &format!("{SIGNIN}/pf-ws/authn/flows/{flow}"),
        &format!("application/vnd.pingidentity.{action}+json"),
        data.to_string().as_bytes(),
    )
}

fn status(state: &Value) -> Result<&str, String> {
    if !state["error"].is_null() || !state["errors"].is_null() {
        return Err("Delta returned a sign-in error.".into());
    }
    state["status"].as_str().ok_or_else(|| INTERACTION.into())
}

fn authenticate(
    http: &mut impl Transport,
    credentials: impl FnOnce() -> Result<(String, String), String>,
) -> Result<String, String> {
    let verifier = random_hex()?;
    let query = form(&[
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("scope", "openid loyaltyProfileLite.profileLite.read"),
        ("response_mode", "pi.flow"),
        ("code_challenge_method", "S256"),
        ("code_challenge", &challenge(&verifier)),
        ("v", "1"),
    ]);
    let mut state = ping(
        http,
        "GET",
        &format!("{SIGNIN}/as/authorization.oauth2?{query}"),
        "application/vnd.pingidentity.initiateOAuthAuthorization+json",
        &[],
    )?;
    // Delta may request IS_USER_REMEMBERED before showing the credential prompt.
    // The native client has no confirmed contract for that device-information step.
    if status(&state)? != "USERNAME_PASSWORD_REQUIRED" {
        return Err(INTERACTION.into());
    }
    // Validate the transaction before asking the traveler to type.
    id(&state["id"])?;
    let (user, password) = credentials()?;
    if user.is_empty() || password.is_empty() {
        return Err("Enter your SkyMiles username and password.".into());
    }
    state = flow_post(
        http,
        &state,
        "checkUsernamePassword",
        json!({ "username": user, "password": password }),
    )?;
    if status(&state)? == "SHOULD_REMEMBER_USER" {
        state = flow_post(
            http,
            &state,
            "rememberDevice",
            json!({ "rememberUser": false }),
        )?;
        if status(&state)? == "REMEMBERED_DEVICE_DETAILS" {
            state = flow_post(http, &state, "submitContinue", json!({ "continue": true }))?;
        }
    }
    match status(&state)? {
        "COMPLETED" => {}
        "USERNAME_PASSWORD_REQUIRED" => {
            return Err("Delta did not accept these SkyMiles details.".into());
        }
        _ => return Err(INTERACTION.into()),
    }
    let code = state["authorizeResponse"]["code"]
        .as_str()
        .filter(|code| !code.is_empty())
        .ok_or(INTERACTION)?;
    let payload = form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", &verifier),
        ("client_id", CLIENT_ID),
    ]);
    let tokens = ping(
        http,
        "POST",
        "https://signin.delta.com/as/token.oauth2",
        "application/x-www-form-urlencoded",
        payload.as_bytes(),
    )?;
    if !tokens["expires_in"]
        .as_f64()
        .is_some_and(|expiry| expiry > 0.0)
    {
        return Err(INTERACTION.into());
    }
    // Only an HTTP-header-safe bearer token may be forwarded to wifi.delta.com.
    tokens["access_token"]
        .as_str()
        .filter(|token| !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()))
        .map(str::to_owned)
        .ok_or_else(|| INTERACTION.into())
}

enum Activation {
    Submitted,
    VideoPage,
}

fn activation(response: Response) -> Result<Activation, String> {
    if response.body.iter().all(u8::is_ascii_whitespace)
        || serde_json::from_slice::<Value>(&response.body).is_ok()
    {
        return Ok(Activation::Submitted);
    }
    let html = response.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.split(';').next().is_some_and(|kind| {
                kind.trim().eq_ignore_ascii_case("text/html")
                    || kind.trim().eq_ignore_ascii_case("application/xhtml+xml")
            })
    });
    if html {
        // A static response cannot establish whether a Skip control is visible,
        // enabled, and optional in the portal. Leave the actual page to a browser.
        let document = crate::guest::parse_html(&response.body).ok_or(INTERACTION)?;
        let video = scraper::Selector::parse("video").expect("valid video selector");
        if document.select(&video).next().is_some() {
            return Ok(Activation::VideoPage);
        }
    }
    Err(INTERACTION.into())
}

/// Verify Delta's on-board policy for this player. Media and Internet checks are separate.
pub fn login(
    http: &mut impl Transport,
    local_ip: Ipv4Addr,
    credentials: impl FnOnce() -> Result<(String, String), String>,
) -> Result<(), String> {
    sign_in(http, local_ip, credentials, std::thread::sleep)
}

fn sign_in(
    http: &mut impl Transport,
    local_ip: Ipv4Addr,
    credentials: impl FnOnce() -> Result<(String, String), String>,
    wait: impl Fn(Duration),
) -> Result<(), String> {
    let initial = device(http, local_ip)?;
    if initial.policy == PREMIUM {
        return Ok(());
    }
    if !basic(&initial.policy) {
        return Err("Delta reported an unknown access policy.".into());
    }
    let flight_id = flight(http)?;
    let endpoint = format!(
        "{PORTAL}/api/portableElectronicDevice/v1/flights/{flight_id}/devices/{}/sessions",
        initial.id
    );
    let token = authenticate(http, credentials)?;
    let current = device(http, local_ip)?;
    if current.id != initial.id || flight(http)? != flight_id {
        return Err(DEVICE_CHANGED.into());
    }
    if current.policy == PREMIUM {
        return Ok(());
    }
    if !basic(&current.policy) {
        return Err("Delta reported an unknown access policy.".into());
    }
    let csrf = match http.cookie(&endpoint, "XSRF-TOKEN") {
        Some(value) => value,
        None => {
            let value = random_hex()?;
            http.set_cookie(PORTAL, "XSRF-TOKEN", &value)
                .map_err(|_| "Could not prepare the Delta access request.")?;
            value
        }
    };
    if csrf.is_empty() || !csrf.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(INTERACTION.into());
    }
    let activation = activation(request(
        http,
        "POST",
        &endpoint,
        &[
            ("Content-Type", "application/json"),
            ("X-XSRF-TOKEN", &csrf),
            ("UserAuth", &token),
        ],
        br#"{"type":"freeWithSkyMiles"}"#,
    )?)?;
    // An aircraft can activate access asynchronously while its web UI shows
    // a video. The activation API can acknowledge that same request with JSON.
    // Check immediately, then at approximately 2, 10, 20 and 30 seconds.
    for delay in [0, 2, 8, 10, 10] {
        if delay > 0 {
            wait(Duration::from_secs(delay));
        }
        let observed = device(http, local_ip)?;
        if observed.id != initial.id {
            return Err(DEVICE_CHANGED.into());
        }
        if observed.policy == PREMIUM {
            return Ok(());
        }
        if !basic(&observed.policy) {
            break;
        }
    }
    Err(match activation {
        Activation::VideoPage => VIDEO_INTERACTION,
        Activation::Submitted => UNCONFIRMED,
    }
    .into())
}

#[cfg(test)]
#[path = "delta/tests.rs"]
mod tests;
