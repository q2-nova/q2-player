use super::*;
use std::collections::VecDeque;

const IP: Ipv4Addr = Ipv4Addr::new(172, 19, 0, 81);

struct Portal {
    responses: VecDeque<Result<Response, String>>,
    password_submissions: usize,
    activation_submissions: usize,
    pkce: Option<String>,
    cookie: Option<String>,
}

fn response(body: Value) -> Result<Response, String> {
    Ok(Response {
        status: 200,
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
    })
}

fn local_device(id: &str, policy: &str) -> Result<Response, String> {
    response(json!({ "deviceId": id, "deviceLanIp": IP.to_string(), "devicePolicy": policy }))
}

fn flight_details() -> Result<Response, String> {
    response(json!({ "flightId": "fixture-flight" }))
}

fn state(status: &str) -> Result<Response, String> {
    response(json!({ "id": "fixture-flow", "status": status }))
}

impl Portal {
    fn new(responses: impl IntoIterator<Item = Result<Response, String>>) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            password_submissions: 0,
            activation_submissions: 0,
            pkce: None,
            cookie: None,
        }
    }

    fn attempt(&mut self) -> Result<(), String> {
        let result = sign_in(
            self,
            IP,
            || Ok(("fixture-user".into(), "fixture-%&password".into())),
            |_| {},
        );
        assert!(self.responses.is_empty());
        result
    }
}

impl Transport for Portal {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, String> {
        let endpoint = url::Url::parse(url).unwrap();
        let path = endpoint.path();
        let header = |name| {
            headers
                .iter()
                .find_map(|&(key, value)| (key == name).then_some(value))
        };
        assert_eq!(endpoint.scheme(), "https");
        assert!(endpoint.username().is_empty());
        assert_eq!(endpoint.port(), None);
        assert_eq!(header("Cache-Control"), Some("no-store"));
        match path {
            "/api/device-details" | "/api/flight-data" => {
                assert_eq!(endpoint.host_str(), Some("wifi.delta.com"));
                assert_eq!(method, "GET");
            }
            "/as/authorization.oauth2" => {
                assert_eq!(endpoint.host_str(), Some("signin.delta.com"));
                assert_eq!(header("X-XSRF-Header"), Some("PingFederate"));
                let query: std::collections::HashMap<_, _> = endpoint.query_pairs().collect();
                assert_eq!(query["client_id"], CLIENT_ID);
                assert_eq!(query["code_challenge_method"], "S256");
                self.pkce = Some(query["code_challenge"].to_string());
            }
            "/pf-ws/authn/flows/fixture-flow" => {
                assert_eq!(endpoint.host_str(), Some("signin.delta.com"));
                assert_eq!(method, "POST");
                let payload: Value = serde_json::from_slice(body).unwrap();
                match header("Content-Type").unwrap() {
                    "application/vnd.pingidentity.checkUsernamePassword+json" => {
                        self.password_submissions += 1;
                        assert_eq!(
                            payload,
                            json!({
                                "username": "fixture-user", "password": "fixture-%&password"
                            })
                        );
                    }
                    "application/vnd.pingidentity.rememberDevice+json" => {
                        assert_eq!(payload, json!({ "rememberUser": false }));
                    }
                    "application/vnd.pingidentity.submitContinue+json" => {
                        assert_eq!(payload, json!({ "continue": true }));
                    }
                    _ => panic!("unexpected identity action"),
                }
            }
            "/as/token.oauth2" => {
                assert_eq!(endpoint.host_str(), Some("signin.delta.com"));
                assert_eq!(method, "POST");
                let fields: std::collections::HashMap<_, _> =
                    url::form_urlencoded::parse(body).collect();
                assert_eq!(fields["code"], "fixture-code-&");
                assert_eq!(fields["grant_type"], "authorization_code");
                let verifier = &fields["code_verifier"];
                assert_eq!(verifier.len(), 64);
                assert_eq!(self.pkce.as_deref(), Some(challenge(verifier).as_str()));
            }
            "/api/portableElectronicDevice/v1/flights/fixture-flight/devices/fixture-device/sessions" =>
            {
                assert_eq!(endpoint.host_str(), Some("wifi.delta.com"));
                assert_eq!(method, "POST");
                assert_eq!(header("UserAuth"), Some("fixture-token"));
                assert_eq!(header("X-XSRF-TOKEN"), self.cookie.as_deref());
                assert!(self.cookie.is_some());
                assert_eq!(
                    serde_json::from_slice::<Value>(body).unwrap(),
                    json!({
                        "type": "freeWithSkyMiles"
                    })
                );
                self.activation_submissions += 1;
            }
            _ => panic!("unexpected portal endpoint"),
        }
        self.responses
            .pop_front()
            .expect("unexpected portal request")
    }

    fn cookie(&self, url: &str, name: &str) -> Option<String> {
        assert!(url.starts_with(PORTAL));
        assert_eq!(name, "XSRF-TOKEN");
        self.cookie.clone()
    }

    fn set_cookie(&mut self, url: &str, name: &str, value: &str) -> Result<(), String> {
        assert_eq!(url, PORTAL);
        assert_eq!(name, "XSRF-TOKEN");
        assert_eq!(value.len(), 64);
        self.cookie = Some(value.into());
        Ok(())
    }
}

fn basic_device() -> Result<Response, String> {
    local_device("fixture-device", "DalPedDefault")
}

fn auth_responses() -> [Result<Response, String>; 7] {
    [
        basic_device(),
        flight_details(),
        state("USERNAME_PASSWORD_REQUIRED"),
        state("SHOULD_REMEMBER_USER"),
        state("REMEMBERED_DEVICE_DETAILS"),
        response(json!({
            "status": "COMPLETED", "authorizeResponse": { "code": "fixture-code-&" }
        })),
        response(json!({ "expires_in": 3600, "access_token": "fixture-token" })),
    ]
}

#[test]
fn signs_in_and_observes_admission_after_provider_policy_changes() {
    let mut portal = Portal::new(auth_responses().into_iter().chain([
        basic_device(),
        flight_details(),
        Ok(Response {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        }),
        basic_device(),
        local_device("fixture-device", PREMIUM),
    ]));
    assert_eq!(portal.attempt(), Ok(()));
    assert_eq!(portal.password_submissions, 1);
    assert_eq!(portal.activation_submissions, 1);
}

#[test]
fn treats_existing_admission_as_separate_from_internet() {
    let mut portal = Portal::new([local_device("fixture-device", PREMIUM)]);
    assert_eq!(portal.attempt(), Ok(()));
    assert_eq!(portal.password_submissions, 0);
    assert_eq!(portal.activation_submissions, 0);
}

#[test]
fn collecting_credentials_requires_a_valid_ordinary_prompt() {
    for replies in [
        vec![local_device("fixture-device", PREMIUM)],
        vec![
            basic_device(),
            flight_details(),
            state("IS_USER_REMEMBERED"),
        ],
        vec![
            basic_device(),
            flight_details(),
            response(json!({
                "id": "//other.example/", "status": "USERNAME_PASSWORD_REQUIRED"
            })),
        ],
    ] {
        let mut portal = Portal::new(replies);
        let _ = sign_in(
            &mut portal,
            IP,
            || panic!("unexpected credential prompt"),
            |_| {},
        );
        assert!(portal.responses.is_empty());
        assert_eq!(portal.password_submissions, 0);
        assert_eq!(portal.activation_submissions, 0);
    }

    let mut portal = Portal::new([
        basic_device(),
        flight_details(),
        state("USERNAME_PASSWORD_REQUIRED"),
    ]);
    let cancelled = sign_in(&mut portal, IP, || Err("Sign-in cancelled".into()), |_| {});
    assert_eq!(cancelled, Err("Sign-in cancelled".into()));
    assert!(portal.responses.is_empty());
    assert_eq!(portal.password_submissions, 0);
    assert_eq!(portal.activation_submissions, 0);
}

#[test]
fn refuses_interaction_and_foreign_origin() {
    for first in [
        state("IS_USER_REMEMBERED"),
        state("MFA_REQUIRED"),
        response(json!({
            "id": "//other.example/", "status": "USERNAME_PASSWORD_REQUIRED"
        })),
        Ok(Response {
            status: 307,
            headers: vec![("Location".into(), "https://other.example/".into())],
            body: Vec::new(),
        }),
    ] {
        let mut portal = Portal::new([basic_device(), flight_details(), first]);
        assert_eq!(portal.attempt(), Err(INTERACTION.into()));
        assert_eq!(portal.password_submissions, 0);
        assert_eq!(portal.activation_submissions, 0);
    }
}

#[test]
fn refuses_initial_ip_and_later_device_mismatch() {
    let mut wrong_ip = Portal::new([response(json!({
        "deviceId": "fixture-device", "deviceLanIp": "172.19.0.98", "devicePolicy": "DalPedDefault"
    }))]);
    assert_eq!(wrong_ip.attempt(), Err(DEVICE_CHANGED.into()));
    assert_eq!(wrong_ip.password_submissions, 0);

    let mut changed = Portal::new(
        auth_responses()
            .into_iter()
            .chain([local_device("another-device", "DalPedDefault")]),
    );
    assert_eq!(changed.attempt(), Err(DEVICE_CHANGED.into()));
    assert_eq!(changed.password_submissions, 1);
    assert_eq!(changed.activation_submissions, 0);
}

#[test]
fn never_retries_the_password_and_redacts_transport_details() {
    for failure in [
        state("USERNAME_PASSWORD_REQUIRED"),
        Err("fixture-user fixture-%&password".into()),
    ] {
        let mut portal = Portal::new([
            basic_device(),
            flight_details(),
            state("USERNAME_PASSWORD_REQUIRED"),
            failure,
        ]);
        let error = portal.attempt().unwrap_err();
        assert!(!error.contains("fixture"));
        assert_eq!(portal.password_submissions, 1);
        assert_eq!(portal.activation_submissions, 0);
    }
}

#[test]
fn activation_checks_actual_admission_without_fetching_media_or_controls() {
    let html = r#"<video controls src="/airline-film"></video><button disabled>Continue</button>"#;
    for video in [false, true] {
        let pending = if video {
            VIDEO_INTERACTION
        } else {
            UNCONFIRMED
        };
        for (observed, expected, seconds) in [
            (
                (0..5).map(|_| basic_device()).collect(),
                Err(pending.into()),
                30,
            ),
            (vec![local_device("fixture-device", PREMIUM)], Ok(()), 0),
            (
                vec![
                    basic_device(),
                    basic_device(),
                    basic_device(),
                    local_device("fixture-device", PREMIUM),
                ],
                Ok(()),
                20,
            ),
            (
                vec![basic_device(), local_device("another-device", PREMIUM)],
                Err(DEVICE_CHANGED.into()),
                2,
            ),
        ] {
            let acknowledgement = if video {
                Ok(Response {
                    status: 200,
                    headers: vec![("cOnTent-Type".into(), "text/html; charset=utf-8".into())],
                    body: html.as_bytes().to_vec(),
                })
            } else {
                response(json!({}))
            };
            let mut portal = Portal::new(
                auth_responses()
                    .into_iter()
                    .chain([basic_device(), flight_details(), acknowledgement])
                    .chain(observed),
            );
            let elapsed = std::cell::Cell::new(Duration::ZERO);
            let result = sign_in(
                &mut portal,
                IP,
                || Ok(("fixture-user".into(), "fixture-%&password".into())),
                |d| elapsed.set(elapsed.get() + d),
            );
            assert_eq!(result, expected);
            assert_eq!(elapsed.get(), Duration::from_secs(seconds));
            assert!(portal.responses.is_empty());
            assert_eq!(portal.password_submissions, 1);
            assert_eq!(portal.activation_submissions, 1);
        }
    }
}

#[test]
fn unknown_html_challenge_stops_before_following_links_or_checking_policy() {
    let mut portal =
        Portal::new(
            auth_responses().into_iter().chain([
                basic_device(),
                flight_details(),
                Ok(Response {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/html".into())],
                    body:
                        br#"<h1>Verify your identity</h1><script>const msg = "<video>";</script>"#
                            .to_vec(),
                }),
            ]),
        );
    assert_eq!(portal.attempt(), Err(INTERACTION.into()));
    assert_eq!(portal.activation_submissions, 1);
}

#[test]
fn post_login_redirects_stop_before_fetching_any_destination() {
    for target in [
        "/video-step",
        "https://untrusted.example/video",
        "//untrusted.example/",
    ] {
        let mut portal = Portal::new(auth_responses().into_iter().chain([
            basic_device(),
            flight_details(),
            Ok(Response {
                status: 303,
                headers: vec![("Location".into(), target.into())],
                body: Vec::new(),
            }),
        ]));
        assert_eq!(portal.attempt(), Err(INTERACTION.into()));
        assert_eq!(portal.activation_submissions, 1);
    }
}

#[test]
fn json_activation_success_still_checks_this_players_policy() {
    let mut portal = Portal::new(auth_responses().into_iter().chain([
        basic_device(),
        flight_details(),
        response(json!({"accepted": true})),
        local_device("fixture-device", PREMIUM),
    ]));
    assert_eq!(portal.attempt(), Ok(()));
    assert_eq!(portal.activation_submissions, 1);
}

#[test]
fn pkce_matches_the_rfc7636_example() {
    assert_eq!(
        challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
    );
}
