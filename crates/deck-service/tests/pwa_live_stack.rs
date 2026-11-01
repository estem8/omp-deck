//! The live stack of the phone app's Playwright gate (`pwa/e2e/live/`, `bun run live`): the
//! real relay serving the built app (`--web-dir pwa/dist`) on loopback, a real gateway linked
//! to it, and real omp in a sandbox project driven by a scripted model
//! ([`support::fake_model`]).
//!
//! It opens a pairing window, writes the app's pairing link and its control endpoint to
//! `$OMPDECK_LIVE_URL_FILE` (JSON), and stays up until `$OMPDECK_LIVE_STOP_FILE` exists. The
//! control endpoint (plain HTTP on loopback) lets the spec:
//! - `GET /pairing`: the fingerprint the desktop's approval dialog shows (`null` until a device
//!   asks), and `POST /approve` with the fingerprint the phone shows: approved with
//!   View+Control+Manage when the two match, `409` otherwise;
//! - `GET /mobile`: the paired devices (permissions, online, push);
//! - `GET /requests?after=N`: what the model received (`N` = requests already seen);
//! - `GET /png`: a 200×200 PNG fixture;
//! - `GET /push-subscription`: a subscription (loopback endpoint and keys) for the browser's
//!   stubbed `PushManager`, and `GET /pushes`: every push its receiver got, decrypted;
//! - `POST /relay-restart`: stops the relay (every connection closes) and serves again on the
//!   same port and room, answered once it serves.
//!
//! `$OMPDECK_LIVE_RELAY_PORT` fixes the relay's port (the app origin); default any free port.
//!
//! A phone paired with two desktops needs a second gateway on the same relay. That is a second
//! process of this test (its own sandbox, namespace, omp and scripted model):
//! - the first one, started with `OMPDECK_LIVE_SECOND_ROOM=1`, also enrolls a second relay room
//!   and publishes its key as `second_room_key` in the URL file;
//! - the second one, started with `OMPDECK_LIVE_JOIN_RELAY=<relay url>` and
//!   `OMPDECK_LIVE_JOIN_KEY=<that room key>`, starts no relay: its gateway links to the first
//!   one's relay in that room, and its URL file carries its own pairing link and control endpoint
//!   (the `origin` and `relay` are the first one's).
//!
//! `#[ignore]`d: run by `bun run live` and `bun run phone` with `--ignored`; skipped when omp is
//! not installed.

mod support;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use deck_remote::relay::RoomKey;
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{
    Done, Mutation, PairingState, Permissions, RelayLink, SecretText,
};
use serde_json::{Value, json};
use support::device::{OMP_CONFIG, Relay, STEP, Sandbox, run, wait_for, wait_mobile};
use support::fake_model::{self as fm, FakeModel, Reply};
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint as _;
use web_push_native::{Auth, p256};

/// Longer than the turn-finished threshold (`notifications::tracker::MIN_BUSY`, 3 s).
const SLOW_TURN: Duration = Duration::from_millis(4500);
/// Long enough for the phone gate to leave the conversation and background the app by taps.
const LONG_TURN: Duration = Duration::from_secs(20);
/// Long enough for the phone gate to switch to the other desktop by taps before the turn ends.
const SWITCH_TURN: Duration = Duration::from_secs(30);
/// How long the steered turn's tool call streams (time to steer while busy).
const STEER_STREAM: Duration = Duration::from_secs(8);
/// The stack never outlives a runner that died without writing the stop file.
const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// omp passes an image through unchanged when each side is at least 200 px.
const PNG_SIDE: u32 = 200;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- the scripted model -------------------------------------------------------------------------

/// Each prompt names its scenario with a marker; a turn's first request gets the scenario's
/// tool call, later requests (after the tool result or a steer) a text.
fn script(notes: PathBuf) -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            // A side request (the session title).
            return Reply::text("Live fixture");
        }
        let user = fm::last_user_text(req);
        let tools: Vec<String> = fm::after_last_user(req)
            .iter()
            .filter(|m| fm::role(m) == "tool")
            .map(fm::text)
            .collect();
        if tools.is_empty() && user.contains("ASK-MARK") {
            return Reply::tool_call(
                "ask",
                &json!({ "questions": [{
                    "id": "q1",
                    "question": "Pick a color?",
                    "options": [{ "label": "red" }, { "label": "blue" }],
                }]}),
            );
        }
        if tools.is_empty() && user.contains("STEER-MARK") {
            return Reply::slow_tool_call(
                "read",
                &json!({ "path": notes.to_string_lossy() }),
                STEER_STREAM,
            );
        }
        if user.contains("ASK-MARK") {
            let got: String = tools.join(" ").chars().take(120).collect();
            return Reply::text(&format!("ASK-REPLY the tool said: {got}"));
        }
        if user.contains("PUSH-MARK") {
            return Reply::slow_text("PUSH-REPLY finished after a while", SLOW_TURN);
        }
        if user.contains("PUSH-LONG-MARK") {
            return Reply::slow_text("PUSH-REPLY finished after a long while", LONG_TURN);
        }
        if user.contains("PUSH-SWITCH-MARK") {
            return Reply::slow_text("SWITCH-REPLY finished on the other desktop", SWITCH_TURN);
        }
        if user.contains("IMG-MARK") {
            let n = fm::last_user(req).map_or(0, |m| fm::images(m).len());
            return Reply::text(&format!("IMG-REPLY saw {n} image(s)"));
        }
        let head: String = without_reminders(&user).trim().chars().take(40).collect();
        Reply::text(&format!("LIVE-REPLY to: {head}"))
    }
}

/// `text` without omp's `<system-…>…</system-…>` blocks (a reminder before a session's first
/// prompt, a notice around a steer): what the user typed.
fn without_reminders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<system-") {
        out.push_str(&rest[..start]);
        let block = &rest[start..];
        let Some(name_end) = block.find('>') else {
            return out;
        };
        let close = format!("</{}>", &block[1..name_end]);
        match block.find(&close) {
            Some(end) => rest = &block[end + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// A model request as the spec reads it: no system prompt, texts cut to 400 chars, the newest
/// user message's images in full.
fn request_summary(index: usize, req: &Value) -> Value {
    let messages: Vec<Value> = fm::messages(req)
        .iter()
        .filter(|m| fm::role(m) != "system")
        .map(|m| {
            let text: String = fm::text(m).chars().take(400).collect();
            json!({ "role": fm::role(m), "text": text })
        })
        .collect();
    json!({
        "index": index,
        "tools": fm::has_tools(req),
        "last_user": fm::last_user_text(req),
        "images": fm::last_user(req).map(fm::images).unwrap_or_default(),
        "messages": messages,
    })
}

/// A `side`×`side` 8-bit grayscale PNG (stored deflate blocks), a gradient.
fn gradient_png(side: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for b in bytes {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        let crc = crc32(&out[start..]);
        out.extend(crc.to_be_bytes());
    }
    let mut raw = Vec::new();
    for y in 0..side {
        raw.push(0); // filter: none
        raw.extend((0..side).map(|x| ((x * 3 + y) % 256) as u8));
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        zlib.extend(len.to_le_bytes());
        zlib.extend((!len).to_le_bytes());
        zlib.extend(*block);
    }
    zlib.extend(((b << 16) | a).to_be_bytes());
    let mut ihdr = Vec::new();
    ihdr.extend(side.to_be_bytes());
    ihdr.extend(side.to_be_bytes());
    ihdr.extend([8, 0, 0, 0, 0]); // 8-bit grayscale, deflate, no filter, no interlace
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

// ---- a loopback push service --------------------------------------------------------------------

/// The browser subscription's keys (the browser's own are replaced by the stub).
struct Ua {
    secret: p256::SecretKey,
    auth: [u8; 16],
}

impl Ua {
    fn new() -> Ua {
        loop {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("random");
            if let Ok(secret) = p256::SecretKey::from_slice(&bytes) {
                let mut auth = [0u8; 16];
                getrandom::fill(&mut auth).expect("random");
                return Ua { secret, auth };
            }
        }
    }

    fn p256dh(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.secret.public_key().to_encoded_point(false).as_bytes())
    }

    fn decrypt(&self, body: Vec<u8>) -> Result<Vec<u8>, String> {
        web_push_native::decrypt(body, &self.secret, &Auth::clone_from_slice(&self.auth))
            .map_err(|e| format!("{e:?}"))
    }
}

/// One HTTP/1.1 request: method, target and body.
fn read_request(stream: &TcpStream) -> std::io::Result<(String, String, Vec<u8>)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("").to_owned();
    let target = words.next().unwrap_or("").to_owned();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok((method, target, body))
}

fn respond(mut stream: &TcpStream, status: &str, kind: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

/// The pushes the receiver got: (path, encrypted body), oldest first.
type Pushes = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Every POST it receives (path, body), answered `201`.
fn push_receiver(got: Pushes) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the push receiver");
    let port = listener.local_addr().expect("push address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            if let Ok((_, path, body)) = read_request(&stream) {
                respond(&stream, "201 Created", "text/plain", b"");
                lock(&got).push((path, body));
            }
        }
    });
    port
}

// ---- the control endpoint -----------------------------------------------------------------------

/// What the main thread publishes for the control endpoint.
#[derive(Default)]
struct Published {
    /// The approval dialog's fingerprint while a device waits.
    approval: Option<String>,
    mobile: Value,
    /// The gateway's controllers (`/live`): what a stuck step waits on.
    live: Value,
}

/// An approval asked for with the phone's fingerprint, answered by the main thread.
type ApproveAsk = (String, mpsc::Sender<Result<(), String>>);

/// A relay restart asked for; answered once the relay serves again.
type RestartAsk = mpsc::Sender<()>;

struct Control {
    published: Arc<Mutex<Published>>,
    approve: mpsc::Sender<ApproveAsk>,
    restart: mpsc::Sender<RestartAsk>,
    model: Arc<FakeModel>,
    png: Vec<u8>,
    ua: Ua,
    push_endpoint: String,
    pushes: Pushes,
}

impl Control {
    fn serve(self) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the control endpoint");
        let port = listener.local_addr().expect("control address").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let Ok((method, target, body)) = read_request(&stream) else {
                    continue;
                };
                let (status, kind, body) = self.answer(&method, &target, body);
                respond(&stream, status, kind, &body);
            }
        });
        port
    }

    fn answer(&self, method: &str, target: &str, body: Vec<u8>) -> (&'static str, &str, Vec<u8>) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let json = |v: Value| ("200 OK", "application/json", v.to_string().into_bytes());
        match (method, path) {
            ("GET", "/pairing") => json(json!({ "fingerprint": lock(&self.published).approval })),
            ("POST", "/approve") => {
                let (tx, rx) = mpsc::channel();
                let phone = String::from_utf8_lossy(&body).trim().to_owned();
                if self.approve.send((phone, tx)).is_err() {
                    return (
                        "503 Service Unavailable",
                        "text/plain",
                        b"stopping".to_vec(),
                    );
                }
                match rx.recv_timeout(STEP * 2) {
                    Ok(Ok(())) => json(json!({ "approved": true })),
                    Ok(Err(e)) => ("409 Conflict", "text/plain", e.into_bytes()),
                    Err(_) => ("504 Gateway Timeout", "text/plain", b"no answer".to_vec()),
                }
            }
            ("POST", "/relay-restart") => {
                let (tx, rx) = mpsc::channel();
                if self.restart.send(tx).is_err() {
                    return (
                        "503 Service Unavailable",
                        "text/plain",
                        b"stopping".to_vec(),
                    );
                }
                match rx.recv_timeout(STEP) {
                    Ok(()) => json(json!({ "restarted": true })),
                    Err(_) => ("504 Gateway Timeout", "text/plain", b"no answer".to_vec()),
                }
            }
            ("GET", "/mobile") => json(lock(&self.published).mobile.clone()),
            ("GET", "/live") => json(lock(&self.published).live.clone()),
            ("GET", "/requests") => {
                let after = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("after="))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0usize);
                let requests: Vec<Value> = self
                    .model
                    .since(after)
                    .iter()
                    .enumerate()
                    .map(|(i, r)| request_summary(after + i, r))
                    .collect();
                json(json!({ "count": after + requests.len(), "requests": requests }))
            }
            ("GET", "/png") => ("200 OK", "image/png", self.png.clone()),
            ("GET", "/push-subscription") => json(json!({
                "endpoint": self.push_endpoint,
                "p256dh": self.ua.p256dh(),
                "auth": URL_SAFE_NO_PAD.encode(self.ua.auth),
            })),
            ("GET", "/pushes") => {
                let pushes: Vec<Value> = lock(&self.pushes)
                    .iter()
                    .map(|(path, body)| match self.ua.decrypt(body.clone()) {
                        Ok(plain) => json!({
                            "path": path,
                            "payload": serde_json::from_slice::<Value>(&plain)
                                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&plain))),
                        }),
                        Err(e) => json!({ "path": path, "error": e }),
                    })
                    .collect();
                json(json!(pushes))
            }
            _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
        }
    }
}

// ---- the stack ----------------------------------------------------------------------------------

fn env_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set {name}"))
}

/// Written whole, then renamed into place: the runner never reads half a file.
fn write_atomically(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).expect("write the url file");
    std::fs::rename(&tmp, path).expect("publish the url file");
}

fn mobile_summary(m: &deck_service::gateway::protocol::MobileView) -> Value {
    json!({
        "link": format!("{:?}", m.link),
        "fingerprint": m.fingerprint,
        "devices": m.devices.iter().map(|d| json!({
            "name": d.name,
            "platform": d.platform,
            "fingerprint": d.fingerprint,
            "control": d.permissions.control,
            "manage": d.permissions.manage,
            "online": d.online,
            "push": d.push,
        })).collect::<Vec<_>>(),
    })
}

#[test]
#[ignore = "the phone app's live gates: run by `bun run live` / `bun run phone` in pwa/"]
fn pwa_live_stack() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let url_file = env_path("OMPDECK_LIVE_URL_FILE");
    let stop_file = env_path("OMPDECK_LIVE_STOP_FILE");
    // The second desktop of a two-desktop run joins the first one's relay instead of serving one.
    let joined = std::env::var("OMPDECK_LIVE_JOIN_RELAY").ok().map(|url| {
        let key = std::env::var("OMPDECK_LIVE_JOIN_KEY").expect("set OMPDECK_LIVE_JOIN_KEY");
        (
            url,
            RoomKey::parse(&key).expect("OMPDECK_LIVE_JOIN_KEY is a room key"),
        )
    });
    let web_dir = std::env::var_os("OMPDECK_LIVE_WEB_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pwa/dist"),
        PathBuf::from,
    );
    if joined.is_none() {
        let web_dir = web_dir.canonicalize().expect("the built app (pwa/dist)");
        assert!(
            web_dir.join("index.html").is_file(),
            "{} has no index.html: build the app first",
            web_dir.display()
        );
    }

    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("pwa", &config);
    // SAFETY: still single-threaded (no server, relay or client started yet).
    unsafe {
        std::env::set_var("OMP_DECK_PUSH_TEST_LOOPBACK", "1");
    }
    sb.report_failures();
    // The project the phone lists (and starts its session in).
    sb.seed_session("seed-pwa-live");
    let notes = sb.proj().join("notes.txt");
    std::fs::write(&notes, "steer target\n").expect("notes");
    let model = Arc::new(FakeModel::start(script(notes)));
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let pushes = Arc::new(Mutex::new(Vec::new()));
    let push_port = push_receiver(pushes.clone());
    // A fixed port keeps one app origin across runs (`bun run phone`: the phone's Chrome keeps
    // its site data and permission per origin).
    let port = std::env::var("OMPDECK_LIVE_RELAY_PORT")
        .ok()
        .map_or(0, |p| p.parse().expect("OMPDECK_LIVE_RELAY_PORT is a port"));
    // Enrolled before the relay starts: it loads its rooms once at startup and only reloads on a
    // hello for a room it does not know.
    let second_room = (joined.is_none() && std::env::var_os("OMPDECK_LIVE_SECOND_ROOM").is_some())
        .then(|| {
            deck_relay::rooms::enroll(&sb.root.join("relay"), "second desktop")
                .expect("enroll the second room")
                .0
        });
    let mut relay = match &joined {
        Some(_) => None,
        None => Some(Relay::start_serving(
            sb.root.join("relay"),
            web_dir.canonicalize().expect("the built app (pwa/dist)"),
            port,
        )),
    };
    let (relay_url, room_key) = match (&relay, &joined) {
        (Some(r), _) => (r.url(), r.key.encode()),
        (None, Some((url, key))) => (url.clone(), key.encode()),
        (None, None) => unreachable!("a stack either serves a relay or joins one"),
    };
    let origin = relay_url.replacen("ws", "http", 1);

    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    run(
        &client,
        Mutation::RelaySet {
            url: relay_url.clone(),
            room_key: SecretText::new(room_key),
        },
    )
    .expect("relay set");
    wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
    let (pairing, link) = match run(&client, Mutation::PairingBegin).expect("pairing begins") {
        Done::PairingStarted { pairing, link, .. } => (pairing, link),
        other => panic!("pairing-begin result: {other:?}"),
    };

    let published = Arc::new(Mutex::new(Published::default()));
    let (approve, asks) = mpsc::channel::<ApproveAsk>();
    let (restart, restarts) = mpsc::channel::<RestartAsk>();
    let control = Control {
        published: published.clone(),
        approve,
        restart,
        model: model.clone(),
        png: gradient_png(PNG_SIDE),
        ua: Ua::new(),
        push_endpoint: format!("http://127.0.0.1:{push_port}/push/phone"),
        pushes,
    }
    .serve();
    write_atomically(
        &url_file,
        &json!({
            "link": link.expose(),
            "origin": origin,
            "relay": relay_url,
            "control": format!("http://127.0.0.1:{control}"),
            "project": "~/proj",
            "second_room_key": second_room.as_ref().map(RoomKey::encode),
        })
        .to_string(),
    );
    eprintln!(
        "pwa live stack: up (ns {}); stop with {}",
        sb.ns,
        stop_file.display()
    );

    let started = Instant::now();
    while !stop_file.exists() && started.elapsed() < MAX_LIFETIME {
        let mobile = client.state().mobile.get();
        {
            let mut p = lock(&published);
            p.approval = mobile.as_ref().and_then(|m| match &m.pairing {
                Some(v) if v.pairing == pairing => match &v.state {
                    PairingState::Approval { fingerprint, .. } => Some(fingerprint.clone()),
                    PairingState::Waiting => None,
                },
                _ => None,
            });
            if let Some(m) = &mobile {
                p.mobile = mobile_summary(m);
            }
            if let Some(controllers) = client.state().controllers.get() {
                p.live = controllers
                    .iter()
                    .map(|c| {
                        json!({
                            "label": c.label,
                            "activity": format!("{:?}", c.activity),
                            "busy": c.busy,
                            "ctl_expect": c.ctl_expect,
                            "live_control": c.live_control,
                            "model": c.current_model,
                            "session_path": c.session_path,
                            "views": c.views,
                            "requests": format!("{:?}", c.requests),
                        })
                    })
                    .collect();
            }
        }
        while let Ok((phone, answer)) = asks.try_recv() {
            let shown = lock(&published).approval.clone();
            let result = match shown {
                None => Err("no device waits for approval".to_owned()),
                Some(desktop) if desktop != phone => Err(format!(
                    "fingerprints differ: desktop {desktop:?}, phone {phone:?}"
                )),
                Some(_) => run(
                    &client,
                    Mutation::PairingApprove {
                        pairing: pairing.clone(),
                        permissions: Permissions {
                            control: true,
                            manage: true,
                        },
                    },
                )
                .map(|_| ()),
            };
            eprintln!("pwa live stack: approval {result:?}");
            let _ = answer.send(result);
        }
        while let Ok(answer) = restarts.try_recv() {
            match relay.as_mut() {
                Some(relay) => {
                    relay.restart();
                    eprintln!("pwa live stack: relay restarted on port {}", relay.port);
                    let _ = answer.send(());
                }
                // Not answered: the caller gets a gateway timeout, the relay is the other stack's.
                None => {
                    eprintln!("pwa live stack: relay-restart refused (this stack joined a relay)")
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("pwa live stack: stopping");
    drop(client);
    drop(relay);
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
