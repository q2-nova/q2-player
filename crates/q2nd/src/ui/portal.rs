//! Player-side ownership of the finite sign-in helper. All process I/O stays off the UI loop.
use super::{UiMsg, UiTx};
use crate::net::WifiState;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::oneshot,
    task::JoinHandle,
};
use zeroize::Zeroizing;

const HELPER: &str = "/usr/libexec/q2-network/q2-portal";
const READY_LIFE: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, PartialEq)]
pub enum State {
    Idle,
    Checking,
    Online,
    Delta,
    Guest,
    Terms,
    Unsupported,
    Unknown,
    Working,
    Credentials,
    Pairing { ap: String, page: String },
    Prepared,
    Failed(String),
}

#[derive(PartialEq, Clone)]
struct Link {
    ssid: String,
    ip: String,
    bssid: String,
}

enum Reply {
    Accept,
    Credentials(Zeroizing<String>, Zeroizing<String>),
}

#[derive(serde::Serialize)]
struct CredentialReply<'a> {
    op: &'static str,
    user: &'a str,
    password: &'a str,
}

struct Credentials {
    user: Zeroizing<String>,
    password: Zeroizing<String>,
    expires: Instant,
}

pub enum Event {
    State(State),
    Prepared(String, String),
    Expire(u64),
}

// A helper can be killed while waiting for input or a server. Keep the
// player's reason next to the helper's last durable request in that case.
struct Attempt {
    id: String,
    finished: AtomicBool,
}

impl Attempt {
    fn finish(self: &Arc<Self>, reason: &'static str) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            let attempt = Arc::clone(self);
            crate::rt::spawn_blocking(move |_| {
                q2_portal_trace::player_outcome(&attempt.id, reason)
            });
        }
    }
}

pub struct Portal {
    pub state: State,
    link: Option<Link>,
    attempt: Option<Arc<Attempt>>,
    generation: u64,
    task: Option<JoinHandle<()>>,
    reply: Option<oneshot::Sender<Reply>>,
    prepared: Option<Credentials>,
    expiry: Option<JoinHandle<()>>,
    ticket: u64,
    tx: UiTx,
    simulated: bool,
    pairing: bool,
    awaiting_prepared_prompt: bool,
}

impl Portal {
    pub fn new(tx: UiTx, simulated: bool) -> Self {
        Self {
            state: State::Idle,
            link: None,
            attempt: None,
            generation: 0,
            task: None,
            reply: None,
            prepared: None,
            expiry: None,
            ticket: 0,
            tx,
            simulated,
            pairing: false,
            awaiting_prepared_prompt: false,
        }
    }

    pub fn observe(&mut self, wifi: &WifiState) {
        let link = wifi
            .ssid
            .as_ref()
            .zip(wifi.ip.as_ref())
            .zip(wifi.bssid.as_ref())
            .map(|((ssid, ip), bssid)| Link {
                ssid: ssid.clone(),
                ip: ip.clone(),
                bssid: bssid.clone(),
            });
        if !wifi.enabled {
            self.link = None;
            self.cancel_with("wifi_off");
            return;
        }
        if link == self.link {
            return;
        }
        self.link = link;
        if self.pairing {
            return;
        }
        self.cancel_with("roam");
        if self.link.is_some() {
            self.check();
        }
    }

    fn stop(&mut self, reason: &'static str) {
        if let Some(attempt) = &self.attempt {
            attempt.finish(reason);
        }
        self.generation += 1;
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.reply.take();
        self.pairing = false;
        self.awaiting_prepared_prompt = false;
    }

    pub fn cancel(&mut self) {
        self.cancel_with("back");
    }
    fn cancel_with(&mut self, reason: &'static str) {
        self.stop(reason);
        self.state = if self.has_prepared() {
            State::Prepared
        } else {
            State::Idle
        };
    }

    pub fn interactive(&self) -> bool {
        matches!(
            self.state,
            State::Working | State::Credentials | State::Pairing { .. } | State::Terms
        )
    }

    fn start(&mut self, op: &'static str) {
        self.stop("superseded");
        let wire_op = if op == "login_prepared" {
            "begin_login"
        } else {
            op
        };
        let mut command = json!({"v":1,"op":wire_op});
        if op != "pair" {
            let Some(link) = &self.link else {
                self.state = State::Unknown;
                return;
            };
            command["ssid"] = Value::String(link.ssid.clone());
            command["ip"] = Value::String(link.ip.clone());
            command["bssid"] = Value::String(link.bssid.clone());
        }
        self.pairing = op == "pair";
        self.awaiting_prepared_prompt = op == "login_prepared";
        self.state = if op == "check" {
            State::Checking
        } else {
            State::Working
        };
        if self.simulated {
            return;
        }
        if let Some(previous) = &self.attempt {
            command["previous_attempt"] = json!(previous.id);
        }
        if op == "login_prepared" || op == "begin_login" {
            command["input_route"] = json!(if op == "login_prepared" {
                "prepared"
            } else {
                "wheel"
            });
        }
        let attempt = q2_portal_trace::new_id().map(|id| {
            Arc::new(Attempt {
                id,
                finished: AtomicBool::new(false),
            })
        });
        self.attempt = attempt.clone();
        let (ready, signal) = oneshot::channel();
        self.reply = Some(ready);
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task = Some(crate::rt::io().spawn(async move {
            let timeout = Duration::from_secs(match op {
                "pair" => 205,
                "login_prepared" => 110,
                "guest" => 170,       // Allow time to confirm the guest offer.
                "begin_login" => 410, // Allow five minutes of wheel entry plus provider requests.
                _ => 15,
            });
            let result = tokio::time::timeout(
                timeout,
                session(
                    HELPER,
                    command,
                    signal,
                    &tx,
                    generation,
                    attempt.as_ref().map(|a| a.id.as_str()),
                ),
            )
            .await;
            if let Some(attempt) = attempt {
                attempt.finish(match result {
                    Ok(Ok(())) => "completed",
                    Ok(Err(_)) => "helper_error",
                    Err(_) => "timeout",
                });
            }
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(message)) => message,
                Err(_) => "Wi-Fi sign-in timed out. Try again.",
            };
            let _ = tx.send(UiMsg::Portal(
                generation,
                Event::State(State::Failed(error.into())),
            ));
        }));
    }

    pub fn check(&mut self) {
        self.start("check");
    }
    pub fn guest(&mut self) {
        self.start("guest");
    }
    pub fn accept(&mut self) {
        if !matches!(self.state, State::Terms) {
            return;
        }
        if let Some(ready) = self.reply.take() {
            let _ = ready.send(Reply::Accept);
        }
        self.state = State::Working;
    }
    pub fn begin_login(&mut self) {
        self.start("begin_login");
    }
    pub fn submit_credentials(&mut self, user: Zeroizing<String>, password: Zeroizing<String>) {
        if self.state != State::Credentials {
            return;
        }
        if let Some(ready) = self.reply.take() {
            let _ = ready.send(Reply::Credentials(user, password));
        }
        self.state = State::Working;
    }
    pub fn pair(&mut self) {
        self.start("pair");
    }
    pub fn forget_prepared(&mut self) {
        self.prepared.take();
        if let Some(timer) = self.expiry.take() {
            timer.abort();
        }
        self.ticket += 1;
        if self.state == State::Prepared {
            self.state = State::Idle;
        }
    }
    pub fn has_prepared(&self) -> bool {
        self.prepared
            .as_ref()
            .is_some_and(|c| Instant::now() < c.expires)
    }
    pub fn login_prepared(&mut self) {
        if self.state != State::Delta {
            return;
        }
        if self.has_prepared() {
            self.start("login_prepared");
        } else {
            self.forget_prepared();
        }
    }

    fn answer_prepared_prompt(&mut self) {
        // Keep the original expiry and account if the helper has already left.
        // Once the helper accepts the reply it owns the only sign-in attempt.
        if self.has_prepared() {
            if let (Some(account), Some(ready)) = (&self.prepared, self.reply.take()) {
                let reply = Reply::Credentials(account.user.clone(), account.password.clone());
                if ready.send(reply).is_ok() {
                    self.forget_prepared();
                    self.state = State::Working;
                    return;
                }
            }
            self.stop("cancelled");
            self.state = State::Failed("Wi-Fi sign-in ended. Try again.".into());
        } else {
            self.stop("cancelled");
            self.forget_prepared();
            self.state = State::Failed("Phone setup expired. Prepare the account again.".into());
        }
    }

    #[cfg(test)]
    pub fn prepare_fixture(&mut self) {
        self.store("fixture".into(), "fixture".into());
    }

    fn store(&mut self, user: String, password: String) {
        self.forget_prepared();
        self.prepared = Some(Credentials {
            user: Zeroizing::new(user),
            password: Zeroizing::new(password),
            expires: Instant::now() + READY_LIFE,
        });
        self.state = State::Prepared;
        let tx = self.tx.clone();
        let ticket = self.ticket;
        self.expiry = Some(crate::rt::io().spawn(async move {
            tokio::time::sleep(READY_LIFE).await;
            let _ = tx.send(UiMsg::Portal(0, Event::Expire(ticket)));
        }));
    }
    pub fn update(&mut self, generation: u64, event: Event) -> bool {
        if let Event::Expire(ticket) = event {
            if ticket != self.ticket {
                return false;
            }
            self.forget_prepared();
            return true;
        }
        if generation != self.generation {
            return false;
        }
        match event {
            Event::State(State::Credentials) if self.awaiting_prepared_prompt => {
                self.awaiting_prepared_prompt = false;
                self.answer_prepared_prompt();
            }
            Event::State(state) => {
                if self.awaiting_prepared_prompt && state == State::Online {
                    self.forget_prepared();
                }
                self.awaiting_prepared_prompt = false;
                if !matches!(state, State::Pairing { .. }) {
                    self.pairing = false;
                }
                self.state = state;
            }
            Event::Prepared(user, password) => {
                self.pairing = false;
                self.store(user, password);
            }
            Event::Expire(_) => unreachable!(),
        }
        true
    }
}
impl Drop for Portal {
    fn drop(&mut self) {
        self.stop("shutdown");
        self.forget_prepared();
    }
}

// A cancelled session can be the last child for the lifetime of the player.
// Reap it explicitly, even if the caller cancels during the blocking spawn.
struct HelperChild(Option<tokio::process::Child>);

impl Drop for HelperChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if child.id().is_some() {
                // Signal immediately so a stale sign-in cannot continue.
                let _ = child.start_kill();
                crate::rt::io().spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
    }
}

async fn session(
    path: &'static str,
    command: Value,
    reply: oneshot::Receiver<Reply>,
    tx: &UiTx,
    generation: u64,
    attempt_id: Option<&str>,
) -> Result<(), &'static str> {
    let mut process = tokio::process::Command::new(path);
    if let Some(id) = attempt_id {
        process.env("Q2_PORTAL_ATTEMPT", id);
    }
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut owned = crate::rt::spawn_blocking(move |w| {
        crate::platform::process::spawn_reactor_child(w, process)
            .map(|child| HelperChild(Some(child)))
    })
    .await
    .map_err(|_| "Wi-Fi sign-in unavailable")?
    .map_err(|_| "Wi-Fi sign-in unavailable")?;
    let child = owned.0.as_mut().ok_or("Wi-Fi sign-in unavailable")?;
    let mut input = child.stdin.take().ok_or("Wi-Fi sign-in unavailable")?;
    let mut output = BufReader::new(child.stdout.take().ok_or("Wi-Fi sign-in unavailable")?);
    let expected = match command["op"].as_str() {
        Some("guest") => Some("terms"),
        Some("begin_login") => Some("credentials"),
        _ => None,
    };
    // Older bundled helpers may attach their policy text to the ready signal.
    let max_line = if expected == Some("terms") {
        2 * 16 * 1024 + 128
    } else {
        8192
    };
    let mut wire =
        Zeroizing::new(serde_json::to_vec(&command).map_err(|_| "Invalid sign-in request")?);
    drop(command);
    wire.push(b'\n');
    input
        .write_all(&wire)
        .await
        .map_err(|_| "Wi-Fi sign-in ended")?;
    drop(wire);
    let mut reply = Some(reply);
    loop {
        let mut line = Zeroizing::new(Vec::new());
        let n = (&mut output)
            .take(max_line as u64 + 1)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| "Wi-Fi sign-in ended")?;
        if n == 0 || n > max_line || line.last() != Some(&b'\n') {
            return Err("Wi-Fi sign-in ended");
        }
        let msg: Value =
            serde_json::from_slice(&line).map_err(|_| "Wi-Fi helper needs an update")?;
        let field = |key: &str| {
            msg[key]
                .as_str()
                .map(str::to_owned)
                .ok_or("Wi-Fi helper needs an update")
        };
        let kind = msg["state"]
            .as_str()
            .ok_or("Wi-Fi helper needs an update")?;
        let event = match kind {
            "prepared" => Event::Prepared(field("user")?, field("password")?),
            x => Event::State(match x {
                "qr" => State::Pairing {
                    ap: field("ap")?,
                    page: field("page")?,
                },
                "terms" => State::Terms,
                "credentials" => State::Credentials,
                "online" => State::Online,
                "delta" => State::Delta,
                "guest" => State::Guest,
                "unsupported" => State::Unsupported,
                "unknown" => State::Unknown,
                "admitted" => State::Failed("Signed in. Internet is unavailable.".into()),
                "failed" => State::Failed(field("message")?),
                _ => return Err("Wi-Fi helper needs an update"),
            }),
        };
        if !matches!(kind, "terms" | "qr" | "credentials") {
            drop(input);
            if !child.wait().await.is_ok_and(|s| s.success()) {
                return Err("Wi-Fi sign-in ended");
            }
            tx.send(UiMsg::Portal(generation, event))
                .map_err(|_| "Player closed")?;
            return Ok(());
        }
        if kind != "qr" && (expected != Some(kind) || reply.is_none()) {
            return Err("Wi-Fi helper needs an update");
        }
        tx.send(UiMsg::Portal(generation, event))
            .map_err(|_| "Player closed")?;
        if kind == "qr" {
            continue;
        }
        let pending = reply.take().ok_or("Wi-Fi sign-in ended")?;
        let answer = tokio::select! {
            answer = pending => answer.map_err(|_| "Sign-in cancelled")?,
            _ = child.wait() => return Err("Wi-Fi sign-in ended"),
        };
        let mut wire = match answer {
            Reply::Accept if kind == "terms" => Zeroizing::new(b"{\"op\":\"accept\"}".to_vec()),
            Reply::Credentials(user, password) if kind == "credentials" => Zeroizing::new(
                serde_json::to_vec(&CredentialReply {
                    op: "credentials",
                    user: &user,
                    password: &password,
                })
                .map_err(|_| "Invalid sign-in request")?,
            ),
            _ => return Err("Invalid sign-in request"),
        };
        wire.push(b'\n');
        input
            .write_all(&wire)
            .await
            .map_err(|_| "Wi-Fi sign-in ended")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_credentials_do_not_replace_cancelled_pairing() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut portal = Portal::new(tx, true);
        portal.pair();
        let pending = portal.generation;
        portal.cancel();
        assert!(!portal.update(pending, Event::Prepared("fixture".into(), "fixture".into())));
        assert!(!portal.has_prepared() && portal.state == State::Idle);
    }

    #[test]
    fn repeated_notification_does_not_reprobe_and_old_result_cannot_follow_a_roam() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut portal = Portal::new(tx, true);
        let mut wifi = WifiState {
            enabled: true,
            ssid: Some("Generated".into()),
            ip: Some("192.0.2.1".into()),
            bssid: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        };
        portal.observe(&wifi);
        let pending = portal.generation;
        assert!(portal.state == State::Checking);
        portal.observe(&wifi);
        assert_eq!(pending, portal.generation);
        wifi.bssid = Some("02:00:00:00:00:02".into());
        portal.observe(&wifi);
        assert!(!portal.update(pending, Event::State(State::Online)));
        assert!(portal.state == State::Checking);
    }

    #[test]
    fn deferred_credentials_use_the_waiting_session_once() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut portal = Portal::new(tx, true);
        portal.observe(&WifiState {
            enabled: true,
            ssid: Some("DeltaWiFi.com".into()),
            ip: Some("192.0.2.1".into()),
            bssid: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        });
        portal.begin_login();
        let attempt = portal.generation;
        let (reply, mut waiting) = oneshot::channel();
        portal.reply = Some(reply);
        let submit = |portal: &mut Portal| {
            portal.submit_credentials(
                Zeroizing::new("generated".into()),
                Zeroizing::new("only".into()),
            );
        };
        submit(&mut portal);
        assert!(matches!(
            waiting.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(portal.update(attempt, Event::State(State::Credentials)));
        submit(&mut portal);
        assert!(
            matches!(waiting.try_recv(), Ok(Reply::Credentials(user, password))
            if user.as_str() == "generated" && password.as_str() == "only")
        );
        assert_eq!(portal.generation, attempt);
        assert!(portal.state == State::Working);
        portal.cancel();
        assert!(!portal.update(attempt, Event::State(State::Credentials)));
        submit(&mut portal);
        assert!(portal.state == State::Idle);
    }

    #[test]
    fn prepared_prompt_is_bound_to_the_live_link_receiver_and_original_expiry() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut portal = Portal::new(tx, true);
        let mut wifi = WifiState {
            enabled: true,
            ssid: Some("DeltaWiFi.com".into()),
            ip: Some("192.0.2.1".into()),
            bssid: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        };
        portal.observe(&wifi);
        portal.prepare_fixture();
        let expiry = portal.prepared.as_ref().unwrap().expires;
        portal.state = State::Delta;
        portal.login_prepared();
        let stale = portal.generation;
        let (ready, mut old_receiver) = oneshot::channel();
        portal.reply = Some(ready);
        wifi.bssid = Some("02:00:00:00:00:02".into());
        portal.observe(&wifi);
        assert!(!portal.update(stale, Event::State(State::Credentials)));
        assert!(matches!(
            old_receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(portal.has_prepared());

        portal.state = State::Delta;
        portal.login_prepared();
        let (ready, receiver) = oneshot::channel();
        drop(receiver);
        portal.reply = Some(ready);
        assert!(portal.update(portal.generation, Event::State(State::Credentials)));
        assert!(matches!(portal.state, State::Failed(_)));
        assert_eq!(portal.prepared.as_ref().unwrap().expires, expiry);

        portal.state = State::Delta;
        portal.login_prepared();
        let (ready, mut receiver) = oneshot::channel();
        portal.reply = Some(ready);
        portal.prepared.as_mut().unwrap().expires = Instant::now();
        assert!(portal.update(portal.generation, Event::State(State::Credentials)));
        assert!(matches!(portal.state, State::Failed(_)));
        assert!(portal.prepared.is_none());
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    // Run the exact test binary in the isolated venue. There the helper and
    // generated supplicant status occupy their production paths.
    #[test]
    #[ignore = "requires tests/perf/portal_guest_journey.py"]
    fn generated_guest_supervisor_fixture() {
        const WPA_STATUS: &str = "/fixture/wpa-status/current";
        let runner = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut portal = Portal::new(tx, false);

        let read_wifi = || {
            let output = std::process::Command::new("/usr/sbin/wpa_cli")
                .args(["-i", "wlan0", "status"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let text = std::str::from_utf8(&output.stdout).unwrap();
            let field = |key| {
                text.lines()
                    .filter_map(|line| line.split_once('='))
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_owned())
            };
            WifiState {
                enabled: field("wpa_state").as_deref() == Some("COMPLETED"),
                ssid: field("ssid"),
                ip: field("ip_address"),
                bssid: field("bssid"),
                ..Default::default()
            }
        };
        let roam = |bssid: &str| {
            std::fs::write(
                WPA_STATUS,
                format!("wpa_state=COMPLETED\nssid=Generated Guest\nbssid={bssid}\nip_address=192.0.2.10\n"),
            )
            .unwrap();
        };
        let mut receive = |portal: &mut Portal| {
            runner.block_on(async {
                tokio::time::timeout(Duration::from_secs(7), async {
                    loop {
                        let UiMsg::Portal(generation, event) = rx.recv().await.unwrap() else {
                            panic!("unexpected message from portal");
                        };
                        if portal.update(generation, event) {
                            return;
                        }
                    }
                })
                .await
                .expect("portal state update");
            });
        };

        portal.observe(&read_wifi());
        receive(&mut portal);
        assert!(portal.state == State::Guest);
        portal.guest();
        receive(&mut portal);
        assert!(portal.state == State::Terms);

        // The same SSID and IP can remain after a roam. The newly observed
        // BSSID invalidates the displayed terms and their consent channel.
        let roamed_attempt = portal.attempt.as_ref().map(|attempt| attempt.id.clone());
        roam("02:00:00:00:00:02");
        portal.observe(&read_wifi());
        portal.accept();
        assert!(portal.state == State::Checking);
        receive(&mut portal);
        assert!(portal.state == State::Guest);

        // Exercise the transport's independent check when the supplicant
        // changes first and the player's Wi-Fi notification is still pending.
        portal.guest();
        receive(&mut portal);
        assert!(portal.state == State::Terms);
        roam("02:00:00:00:00:03");
        portal.accept();
        receive(&mut portal);
        assert!(matches!(portal.state, State::Failed(_)));

        // A new offer on the stable association remains usable.
        portal.observe(&read_wifi());
        receive(&mut portal);
        assert!(portal.state == State::Guest);
        portal.guest();
        receive(&mut portal);
        assert!(portal.state == State::Terms);
        portal.accept();
        receive(&mut portal);
        assert!(portal.state == State::Online);

        if std::env::var_os("Q2_PORTAL_TRACE_TEST").is_some() {
            let root = std::path::Path::new("/mnt/mmc/portal-debug");
            let aborted = root.join(roamed_attempt.unwrap()).join("player-outcome.json");
            let completed = root.join(&portal.attempt.as_ref().unwrap().id).join("player-outcome.json");
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !aborted.is_file() || !completed.is_file() {
                assert!(std::time::Instant::now() < deadline, "player trace outcome was not published");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    #[ignore = "requires the isolated generated wheel and TLS venue"]
    fn generated_wheel_login_supervisor_fixture() {
        use std::{fs, path::Path, thread};
        let scope = Path::new("/fixture/lifecycle");
        let scenario = fs::read_to_string(scope.join("case")).unwrap();
        let runner = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut portal = Portal::new(tx, false);
        let mut receive = |portal: &mut Portal| {
            runner.block_on(async {
                tokio::time::timeout(Duration::from_secs(7), async {
                    loop {
                        let UiMsg::Portal(generation, event) = rx.recv().await.unwrap() else {
                            panic!("unexpected portal notification");
                        };
                        if portal.update(generation, event) {
                            break;
                        }
                    }
                })
                .await
                .expect("portal notification");
            });
        };
        let current_helper = || {
            fs::read_dir("/proc/self/task")
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_to_string(entry.path().join("children")).ok())
                .flat_map(|line| {
                    line.split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok())
                        .collect::<Vec<_>>()
                })
                .find(|pid| {
                    // The helper disables ptrace and /proc/exe access. Its short
                    // process name and our direct parent relation identify it here.
                    fs::read_to_string(format!("/proc/{pid}/comm"))
                        .is_ok_and(|name| name.trim() == "q2-portal")
                })
        };
        let wait_for = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(4);
            while !condition() {
                assert!(Instant::now() < deadline, "helper lifecycle deadline");
                thread::sleep(Duration::from_millis(20));
            }
        };
        let submit = |portal: &mut Portal| {
            portal.submit_credentials(
                Zeroizing::new("fixture-user".into()),
                Zeroizing::new("fixture-%&password".into()),
            );
        };

        portal.observe(&WifiState {
            enabled: true,
            ssid: Some("DeltaWiFi.com".into()),
            ip: Some("192.0.2.10".into()),
            bssid: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        });
        receive(&mut portal);
        assert!(portal.state == State::Delta);
        portal.begin_login();
        let generation = portal.generation;
        receive(&mut portal);
        if scenario.trim() == "challenge" {
            assert!(matches!(portal.state, State::Failed(_)));
            assert!(current_helper().is_none());
            return;
        }
        assert!(portal.state == State::Credentials);
        let pid = current_helper().expect("waiting helper child");

        match scenario.trim() {
            "cancel" => {
                portal.cancel();
                submit(&mut portal);
                assert!(portal.state == State::Idle && portal.generation != generation);
                wait_for(&|| current_helper().is_none());
                assert!(
                    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| stat
                        .rsplit_once(") ")
                        .is_some_and(|(_, rest)| rest.starts_with("Z ")))
                );
            }
            "success" => {
                submit(&mut portal);
                assert!(portal.state == State::Working && portal.generation == generation);
                wait_for(&|| scope.join("password-post").is_file());
                assert_eq!(current_helper(), Some(pid));
                fs::write(scope.join("continue"), b"continue\n").unwrap();
                receive(&mut portal);
                assert!(portal.state == State::Online && portal.generation == generation);
                assert!(current_helper().is_none());
            }
            _ => panic!("unknown generated wheel fixture case"),
        }
    }
}
