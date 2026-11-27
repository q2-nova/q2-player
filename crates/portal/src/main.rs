//! A single operation per process. Credentials travel only through private pipes.
mod delta;
mod guest;
mod http;
mod phone;
mod trace_view;

use q2_portal_trace::Trace;

use guest::Detection;
use http::{Client, Link};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

fn send(message: Value) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &message).map_err(|_| "Player disconnected")?;
    out.write_all(b"\n")
        .and_then(|_| out.flush())
        .map_err(|_| "Player disconnected".into())
}

fn input(stream: &mut impl BufRead) -> Result<Value, String> {
    let mut bytes = Vec::new();
    let n = std::io::Read::take(stream, 8193)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Player disconnected")?;
    if n == 0 || n > 8192 || bytes.last() != Some(&b'\n') {
        return Err("Sign-in cancelled".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "Invalid sign-in request".into())
}

fn capport(lease: &str, link: &Link, resolver_inode: u64) -> Option<String> {
    let mut fields = lease.strip_suffix('\n')?.split('\n');
    if fields.next()? != "q2-capport-v2"
        || fields.next()?.parse::<Ipv4Addr>().ok()? != link.ip
        || fields.next()?.parse::<u64>().ok()? != resolver_inode
        || !fields.next()?.eq_ignore_ascii_case(&link.bssid)
        || fields.next()? != link.ssid
    {
        return None;
    }
    let encoded = fields.next()?;
    if encoded.len() > 510 || fields.next().is_some() {
        return None;
    }
    let bytes = encoded.as_bytes().chunks_exact(2);
    if !bytes.remainder().is_empty() {
        return None;
    }
    let uri: Vec<u8> = bytes
        .map(|v| u8::from_str_radix(std::str::from_utf8(v).ok()?, 16).ok())
        .collect::<Option<_>>()?;
    String::from_utf8(uri)
        .ok()
        .filter(|v| v.starts_with("https://"))
}

fn lease_endpoint(link: &Link) -> Option<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open("/run/q2-network/portal-lease")
        .ok()?;
    let lease = file.metadata().ok()?;
    let resolver = std::fs::metadata("/run/resolv.conf").ok()?;
    if !lease.is_file() || lease.modified().ok()? < resolver.modified().ok()? {
        return None;
    }
    let mut body = String::new();
    if file.take(1025).read_to_string(&mut body).ok()? > 1024 {
        return None;
    }
    capport(&body, link, resolver.ino())
}

fn classified(d: Detection) -> &'static str {
    match d {
        Detection::Online => "online",
        Detection::Delta => "delta",
        Detection::Guest(_) => "guest",
        Detection::Unsupported => "unsupported",
        Detection::Unknown => "unknown",
    }
}

fn credential_fields(message: &mut Value) -> Result<(String, String), String> {
    let Value::String(user) = message["user"].take() else {
        return Err("Enter your SkyMiles account".into());
    };
    let Value::String(password) = message["password"].take() else {
        return Err("Enter your SkyMiles password".into());
    };
    Ok((user, password))
}

fn execute(stream: &mut impl BufRead) -> Result<(), String> {
    let command = input(stream)?;
    let op = match command["op"].as_str() {
        Some(op @ ("pair" | "check" | "guest" | "login" | "begin_login")) => op,
        _ => "unknown",
    };
    let trace = Trace::open(op);
    let input_route = match command["input_route"].as_str() {
        Some("prepared") => "prepared",
        Some("wheel") => "wheel",
        _ => "unspecified",
    };
    let previous = command["previous_attempt"]
        .as_str()
        .filter(|s| s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
    trace.event(
        "command",
        json!({"route": input_route, "previous_attempt":previous}),
    );
    let result = operate(stream, command, &trace);
    match &result {
        Ok(()) => trace.event("terminal", json!({"outcome":"finished"})),
        Err(message) => trace.event("terminal", json!({"outcome":"failed","reason":message})),
    }
    result
}

fn operate(stream: &mut impl BufRead, mut command: Value, trace: &Trace) -> Result<(), String> {
    if command["v"] != 1 {
        return Err("Wi-Fi helper needs an update".into());
    }
    let op = command["op"].as_str().ok_or("Unknown command")?;
    let send = |message: Value| {
        // Only the state name goes to the card. QR and prepared replies contain secrets.
        trace.event("state", json!({"state": message["state"]}));
        send(message)
    };
    if op == "pair" {
        let (user, password) =
            phone::pair(|ap, page| send(json!({"state":"qr","ap":ap,"page":page})))?;
        return send(json!({"state":"prepared","user":user,"password":password}));
    }
    let link = Link::current()?;
    trace.event(
        "association",
        json!({"ssid":link.ssid,"bssid":link.bssid,"ip":link.ip}),
    );
    if command["ssid"].as_str() != Some(&link.ssid)
        || command["ip"].as_str() != Some(&link.ip.to_string())
        || command["bssid"].as_str() != Some(&link.bssid)
    {
        return Err("Wi-Fi changed. Check the connection again.".into());
    }
    let mut http = Client::new(link.clone(), trace)?;
    let state = match op {
        "begin_login" | "login" => {
            let supplied = if op == "login" {
                Some(credential_fields(&mut command)?)
            } else {
                None
            };
            delta::login(&mut http, link.ip, || {
                if let Some(credentials) = supplied {
                    return Ok(credentials);
                }
                send(json!({"state":"credentials"}))?;
                let mut reply = input(stream)?;
                if reply["op"] != "credentials" {
                    return Err("Sign-in cancelled".into());
                }
                trace.event("action", json!({"action":"credentials_supplied"}));
                credential_fields(&mut reply)
            })?;
            // The airline's admission and usable wider connectivity are separate observations.
            match guest::detect(&mut http, None)? {
                Detection::Online => "online",
                _ => "admitted",
            }
        }
        "check" | "guest" => {
            let lease = lease_endpoint(&link);
            trace.event("discovery", json!({"capport_advertised":lease.is_some()}));
            let found = guest::detect(&mut http, lease.as_deref())?;
            if link.ssid.eq_ignore_ascii_case("DeltaWiFi.com") && op == "check" {
                // Even an unauthenticated aircraft may allow a common connectivity probe.
                match delta::admitted(&mut http, link.ip) {
                    Ok(false) => return send(json!({"state":"delta"})),
                    Ok(true) if !matches!(found, Detection::Online) => {
                        return send(json!({"state":"admitted"}));
                    }
                    Err(_) if matches!(found, Detection::Online) => {
                        return send(json!({"state":"unknown"}));
                    }
                    _ => {}
                }
            }
            match found {
                Detection::Guest(offer) if op == "guest" => {
                    send(json!({"state":"terms","text":"Terms & Conditions apply"}))?;
                    if input(stream)?["op"] != "accept" {
                        return Err("Sign-in cancelled".into());
                    }
                    trace.event("action", json!({"action":"agree"}));
                    guest::accept(&mut http, offer)?;
                    classified(guest::detect(&mut http, lease_endpoint(&link).as_deref())?)
                }
                d => classified(d),
            }
        }
        _ => return Err("Unknown Wi-Fi operation".into()),
    };
    send(json!({"state":state}))
}

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--license")) {
        print!("{}", include_str!("../DELTA-LICENSE"));
        return;
    }
    // No core dumps of credentials or access tokens.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0);
    }
    if let Err(message) = execute(&mut std::io::stdin().lock()) {
        let _ = send(json!({"state":"failed","message":message}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn option_114_belongs_to_current_lease() {
        let link = Link {
            ip: Ipv4Addr::new(192, 0, 2, 10),
            bssid: "02:00:00:00:00:AA".into(),
            ssid: "Generated Guest ".into(),
        };
        let encoded = "68747470733a2f2f76656e75652e74657374";
        let lease = format!(
            "q2-capport-v2\n{}\n33\n02:00:00:00:00:aa\n{}\n{encoded}\n",
            link.ip, link.ssid
        );
        assert_eq!(
            capport(&lease, &link, 33).as_deref(),
            Some("https://venue.test")
        );
        assert!(capport(&lease, &link, 34).is_none());
        assert!(capport(&format!("{}\n{encoded}\n", link.ip), &link, 33).is_none());
        for invalid in [
            lease.replace("192.0.2.10", "192.0.2.11"),
            lease.replace("00:aa", "00:ab"),
            lease.replace("Guest ", "Guest"),
            lease.replace(encoded, "0fg"),
            lease.replace(encoded, "2b"),
            format!("{lease}\n"),
        ] {
            assert!(capport(&invalid, &link, 33).is_none());
        }
    }
}
