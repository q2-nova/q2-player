//! One-shot browser input over the player's private WPA2 link.
use ring::rand::{SecureRandom, SystemRandom};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELPER: &str = "/usr/libexec/q2-network/portal-phone-ap";
const READY: &str = "/run/q2-portal-phone/ready";
const RADIO_ERROR: &str = "Couldn’t start phone Wi-Fi. Check Wi-Fi and try again.";
const HEADER_MAX: usize = 4096;
const BODY_MAX: usize = 8192;

struct AccessPoint(Child);

impl AccessPoint {
    fn running(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for AccessPoint {
    fn drop(&mut self) {
        if self.running() {
            // We own this unreaped child. TERM lets its EXIT trap restore wlan1.
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(8);
            while self.running() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn query(action: &str) -> Result<String, String> {
    let mut child = Command::new(HELPER)
        .arg(action)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RADIO_ERROR)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let output = child.wait_with_output().map_err(|_| RADIO_ERROR)?;
                return String::from_utf8(output.stdout).map_err(|_| RADIO_ERROR.into());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RADIO_ERROR.into());
            }
        }
    }
}

fn channel(frequency: u32) -> Option<u32> {
    match frequency {
        2412..=2472 if (frequency - 2407) % 5 == 0 => Some((frequency - 2407) / 5),
        5180..=5885 if (frequency - 5000) % 5 == 0 => Some((frequency - 5000) / 5),
        _ => None,
    }
}

fn choose_channel(station: &str, permitted: &str) -> Result<u32, String> {
    let allowed = || {
        permitted
            .split_whitespace()
            .filter_map(|f| f.parse::<u32>().ok())
    };
    let selected = if station.trim() == "none" {
        allowed().find(|f| *f < 2500).or_else(|| allowed().next())
    } else {
        station
            .trim()
            .parse()
            .ok()
            .filter(|f| allowed().any(|a| a == *f))
    };
    selected.and_then(channel).ok_or_else(|| {
        "Phone pairing can’t use this Wi-Fi channel. Pair before joining this network.".into()
    })
}

fn choose_address(routes: &str) -> Option<Ipv4Addr> {
    let ranges: Vec<_> = routes
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 8 || fields[0] == "lo" {
                return None;
            }
            let network = u32::from_str_radix(fields[1], 16).ok()?.swap_bytes();
            let mask = u32::from_str_radix(fields[7], 16).ok()?.swap_bytes();
            (mask != 0).then_some((network, mask))
        })
        .collect();
    [
        "192.168.241.1",
        "192.168.242.1",
        "192.168.243.1",
        "10.42.241.1",
        "172.31.241.1",
    ]
    .into_iter()
    .filter_map(|s| s.parse::<Ipv4Addr>().ok())
    .find(|address| {
        ranges.iter().all(|(network, mask)| {
            let common = mask & 0xffff_ff00;
            u32::from(*address) & common != network & common
        })
    })
}

fn random_hex<const N: usize>() -> Result<String, String> {
    let mut bytes = [0_u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Couldn’t start secure phone pairing.")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn listen_on_ap(address: Ipv4Addr) -> io::Result<TcpListener> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let interface = b"wlan1\0";
    let status = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            interface.as_ptr().cast(),
            interface.len() as libc::socklen_t,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    let reuse = 1 as libc::c_int;
    if unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&reuse as *const libc::c_int).cast(),
            std::mem::size_of_val(&reuse) as libc::socklen_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut endpoint: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    endpoint.sin_family = libc::AF_INET as libc::sa_family_t;
    endpoint.sin_port = 80_u16.to_be();
    endpoint.sin_addr.s_addr = u32::from_ne_bytes(address.octets());
    if unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&endpoint as *const libc::sockaddr_in).cast(),
            std::mem::size_of_val(&endpoint) as libc::socklen_t,
        )
    } != 0
        || unsafe { libc::listen(fd, 4) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let listener = TcpListener::from(socket);
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Call only after the traveler's explicit request. The two QR payloads join
/// WPA2 and open the local entry page. The returned details belong to the caller.
pub fn pair(
    mut announce: impl FnMut(&str, &str) -> Result<(), String>,
) -> Result<(String, String), String> {
    let channel = choose_channel(&query("station-frequency")?, &query("frequencies")?)?;
    let routes = fs::read_to_string("/proc/net/route").map_err(|_| RADIO_ERROR)?;
    let address =
        choose_address(&routes).ok_or("No separate Wi-Fi address is available for pairing.")?;
    let ssid = format!("Q2-SignIn-{}", random_hex::<4>()?);
    let passphrase = format!("q2-{}", random_hex::<16>()?);
    let token = random_hex::<32>()?;
    let mut ap = AccessPoint(
        Command::new(HELPER)
            .args([
                "watch",
                &address.to_string(),
                &channel.to_string(),
                &std::process::id().to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| RADIO_ERROR)?,
    );
    {
        let mut input = ap.0.stdin.take().ok_or(RADIO_ERROR)?;
        writeln!(input, "{ssid}\n{passphrase}").map_err(|_| RADIO_ERROR)?;
    }
    let ready_identity = format!("{} {ssid}", ap.0.id());
    let ready_deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if !ap.running() || Instant::now() >= ready_deadline {
            return Err(RADIO_ERROR.into());
        }
        if fs::read_to_string(READY).is_ok_and(|ready| ready.trim() == ready_identity) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let listener = listen_on_ap(address).map_err(|_| RADIO_ERROR)?;
    let host = address.to_string();
    let origin = format!("http://{host}");
    let wifi_qr = format!("WIFI:T:WPA;S:{ssid};P:{passphrase};;");
    let page_qr = format!("{origin}/#{token}");
    announce(&wifi_qr, &page_qr)?;

    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        if !ap.running() {
            return Err("Phone Wi-Fi closed. Start pairing again.".into());
        }
        match listener.accept() {
            Ok((mut connection, _)) => {
                let cutoff = deadline.min(Instant::now() + Duration::from_secs(2));
                if let Ok(Some(details)) = serve(&mut connection, &host, &origin, &token, cutoff) {
                    // Allow the phone to receive the final response before the AP goes away.
                    thread::sleep(Duration::from_millis(400));
                    return Ok((details.username, details.password));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(40));
            }
            Err(_) => return Err("Phone Wi-Fi closed. Start pairing again.".into()),
        }
    }
    Err("Phone pairing expired. Start pairing again.".into())
}

struct Details {
    username: String,
    password: String,
}

impl Details {
    fn valid(&self) -> bool {
        !self.username.trim().is_empty()
            && self.username.len() <= 254
            && !self.password.is_empty()
            && self.password.len() <= 512
            && !self.username.chars().any(char::is_control)
            && !self.password.chars().any(char::is_control)
    }
}

fn parse_details(body: &[u8]) -> Option<Details> {
    let mut fields: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_slice(body).ok()?;
    if fields.len() != 2 {
        return None;
    }
    let serde_json::Value::String(username) = fields.remove("username")? else {
        return None;
    };
    let serde_json::Value::String(password) = fields.remove("password")? else {
        return None;
    };
    Some(Details { username, password })
}

struct Bounded<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for Bounded<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

fn parse_header(request: &mut impl BufRead) -> io::Result<String> {
    let mut header = String::new();
    while header.len() < HEADER_MAX {
        let mut line = String::new();
        if request
            .take((HEADER_MAX - header.len()) as u64)
            .read_line(&mut line)?
            == 0
            || !line.ends_with("\r\n")
        {
            break;
        }
        if line == "\r\n" {
            return Ok(header);
        }
        header.push_str(&line);
    }
    Err(io::ErrorKind::InvalidData.into())
}

fn respond(stream: &mut impl Write, result: &str, mime: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {result}\r\nContent-Length: {}\r\nContent-Type: {mime}; charset=utf-8\r\n\
        Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\
        Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'\r\n\
        Connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn serve(
    stream: &mut TcpStream,
    host: &str,
    origin: &str,
    token: &str,
    deadline: Instant,
) -> io::Result<Option<Details>> {
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut input = BufReader::new(Bounded { stream, deadline });
    let header = parse_header(&mut input)?;
    let mut lines = header.split_terminator("\r\n");
    let request = lines.next().unwrap_or_default();
    let Some(mut fields) = headers(lines) else {
        return Ok(None);
    };
    let request_host = fields.remove("host").unwrap_or_default();
    if request_host != host && request_host.strip_suffix(":80") != Some(host) {
        respond(
            stream,
            "403 Forbidden",
            "text/plain",
            "Open the Q2 QR code.",
        )?;
        return Ok(None);
    }
    if fields.contains_key("transfer-encoding") {
        return Ok(None);
    }
    if request == "POST /entry HTTP/1.1" {
        let Some(length) = entry_length(&fields, origin, token) else {
            respond(
                stream,
                "403 Forbidden",
                "text/plain",
                "Open the Q2 QR code.",
            )?;
            return Ok(None);
        };
        let mut body = vec![0; length];
        input.read_exact(&mut body)?;
        let details = parse_details(&body);
        body.fill(0);
        if let Some(details) = details.filter(Details::valid) {
            respond(stream, "200 OK", "text/plain", "Details sent.")?;
            return Ok(Some(details));
        }
        respond(
            stream,
            "400 Bad Request",
            "text/plain",
            "Check both sign-in fields.",
        )?;
        return Ok(None);
    }
    let asset = match request {
        "GET / HTTP/1.1" => Some(("text/html", include_str!("phone/index.html"))),
        "GET /entry.js HTTP/1.1" => Some(("text/javascript", include_str!("phone/entry.js"))),
        "GET /style.css HTTP/1.1" => Some(("text/css", include_str!("phone/style.css"))),
        _ => None,
    };
    if let Some((mime, body)) = asset {
        respond(stream, "200 OK", mime, body)?;
    } else {
        respond(stream, "404 Not Found", "text/plain", "")?;
    }
    Ok(None)
}

fn headers<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Option<std::collections::BTreeMap<String, &'a str>> {
    let mut fields = std::collections::BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || fields
                .insert(name.to_ascii_lowercase(), value.trim())
                .is_some()
        {
            return None;
        }
    }
    Some(fields)
}

fn entry_length(
    fields: &std::collections::BTreeMap<String, &str>,
    origin: &str,
    token: &str,
) -> Option<usize> {
    let presented = fields.get("authorization")?.strip_prefix("Bearer ")?;
    // Use a fixed context comparison independent of the first rejected byte.
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
    let signed = ring::hmac::sign(&key, b"q2 portal phone entry");
    let candidate = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, presented.as_bytes());
    ring::hmac::verify(&candidate, b"q2 portal phone entry", signed.as_ref()).ok()?;
    if *fields.get("origin")? != origin || *fields.get("content-type")? != "application/json" {
        return None;
    }
    let length = fields.get("content-length")?.parse::<usize>().ok()?;
    (length > 0 && length <= BODY_MAX).then_some(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "192.168.241.1";
    const ORIGIN: &str = "http://192.168.241.1";
    const TOKEN: &str = "test-token";
    const BODY: &str = r#"{"username":"synthetic-account","password":"synthetic-password"}"#;

    #[test]
    fn keeps_station_on_its_actual_legal_channel() {
        let channels = "2412\n2437\n5220\n";
        assert_eq!(choose_channel("5220\n", channels), Ok(44));
        assert!(choose_channel("5240\n", channels).is_err());
        assert_eq!(choose_channel("none\n", channels), Ok(1));
        assert!(choose_channel("unavailable\n", channels).is_err());
    }

    #[test]
    fn avoids_the_whole_dhcp_subnet_when_a_more_specific_route_overlaps() {
        let routes = "Iface Destination Gateway Flags RefCnt Use Metric Mask\nwlan0 80F1A8C0 0 1 0 0 0 80FFFFFF\n";
        assert_eq!(
            choose_address(routes),
            Some("192.168.242.1".parse().unwrap())
        );
    }

    fn exchange(request: String) -> (String, Option<Details>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            serve(
                &mut stream,
                HOST,
                ORIGIN,
                TOKEN,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap()
        });
        client.write_all(request.as_bytes()).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        (response, worker.join().unwrap())
    }

    fn post(host: &str, origin: &str, authorization: &str) -> (String, Option<Details>) {
        exchange(format!(
            "POST /entry HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nAuthorization: Bearer {authorization}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{BODY}",
            BODY.len()
        ))
    }

    #[test]
    fn browser_post_requires_host_origin_and_pairing_token() {
        for (host, origin, token) in [
            ("wrong.test", ORIGIN, TOKEN),
            (HOST, "http://wrong.test", TOKEN),
            (HOST, ORIGIN, "wrong-token"),
        ] {
            let (response, details) = post(host, origin, token);
            assert!(response.starts_with("HTTP/1.1 403"));
            assert!(details.is_none());
        }
        let (response, details) = post(HOST, ORIGIN, TOKEN);
        let details = details.unwrap();
        assert_eq!(details.username, "synthetic-account");
        assert_eq!(details.password, "synthetic-password");
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(!response.contains("synthetic-"));
        assert!(response.contains("Cache-Control: no-store"));
    }

    #[test]
    fn captive_probes_do_not_trigger_a_redirect_or_show_the_form() {
        let (response, details) = exchange(format!(
            "GET /generate_204 HTTP/1.1\r\nHost: {HOST}\r\n\r\n"
        ));
        assert!(response.starts_with("HTTP/1.1 404"));
        assert!(!response.contains("Location:"));
        assert!(details.is_none());
    }
}
