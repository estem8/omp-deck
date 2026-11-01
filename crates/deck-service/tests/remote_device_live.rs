//! The slice-4 interactive paths end to end with a real omp driven by a scripted model
//! ([`support::fake_model`]), through the real relay and Noise device path:
//! - a structured UI request (`ask`) answered from the device, an approval denied from the
//!   device, and an approval answered in the terminal first (the device's later answer is
//!   `applied: false`);
//! - a steer and a follow-up composed while omp is busy, told apart by where omp delivers them;
//! - a compose with an image attachment reaching the model, and the transcript image fetched back
//!   byte for byte;
//! - a Web Push for a finished turn to a loopback receiver, decrypted, and none while the device
//!   reports itself visible;
//! - Control and Manage ops refused without the permission.
//!
//! Private HOME/XDG tree, own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use deck_remote::api::{
    self, Activity, ApiError, ApproveDecision, DoneView, Event, Feed, FeedValue, Item, LiveRef,
    LiveSession, Op, OutcomeView, Record, ReplyResult, Resp, UiAnswer, UiAskAnswer, UiKind,
    UiRequestView, ViewMode,
};
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{Mutation, Permissions, RelayLink, SecretText};
use serde_json::{Value, json};
use support::device::{
    Device, OMP_CONFIG, Relay, SLICE, STEP, Sandbox, accepted, event, pair, run, wait_for,
    wait_mobile,
};
use support::fake_model::{self as fm, FakeModel, Reply};
use web_push_native::p256::elliptic_curve::sec1::ToEncodedPoint as _;
use web_push_native::{Auth, p256};

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
        raw.extend((0..side).map(|x| ((x + y) % 256) as u8));
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

/// omp passes an image through unchanged when each side is at least 200 px and it is well
/// under its byte budget (`image-resize.ts`); smaller ones are re-encoded.
const PNG_SIDE: u32 = 200;
/// Longer than the turn-finished threshold (`notifications::tracker::MIN_BUSY`, 3 s).
const SLOW_TURN: Duration = Duration::from_millis(4500);
/// The session's first turn: long enough for omp to drop its file handle (title rewrite) mid-turn.
const FIRST_TURN: Duration = Duration::from_secs(15);
/// How soon after a turn the session id must be known (the sidebar scan runs every few seconds).
const ID_AFTER_TURN: Duration = Duration::from_secs(12);
/// How long the steered turn's tool call streams (time to steer and follow up while busy).
const STEER_STREAM: Duration = Duration::from_secs(8);
/// How long a model request or a turn may take.
const TURN: Duration = Duration::from_secs(60);

// ---- the scripted model -------------------------------------------------------------------------

/// Each prompt names its scenario with a marker; a turn's first request gets the scenario's
/// tool call, every later request (after the tool result, a steer, a follow-up) a short text.
fn script(notes: PathBuf) -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            // A side request (e.g. the session title).
            return Reply::text("Fake title");
        }
        let user = fm::last_user_text(req);
        let fresh = !fm::after_last_user(req)
            .iter()
            .any(|m| fm::role(m) == "tool");
        if fresh && user.contains("ASK-MARK") {
            return Reply::tool_call(
                "ask",
                &json!({ "questions": [{
                    "id": "q1",
                    "question": "Pick a color?",
                    "options": [{ "label": "red" }, { "label": "blue" }],
                }]}),
            );
        }
        if fresh && user.contains("DENY-MARK") {
            return Reply::tool_call("bash", &json!({ "command": "echo deny-run" }));
        }
        if fresh && user.contains("RACE-MARK") {
            return Reply::tool_call("bash", &json!({ "command": "echo race-run" }));
        }
        if fresh && user.contains("STEER-MARK") {
            return Reply::slow_tool_call(
                "read",
                &json!({ "path": notes.to_string_lossy() }),
                STEER_STREAM,
            );
        }
        if user.contains("FIRST-LONG-MARK") {
            return Reply::slow_text("first turn finished", FIRST_TURN);
        }
        if user.contains("PUSH-MARK") || user.contains("QUIET-MARK") {
            return Reply::slow_text("finished after a while", SLOW_TURN);
        }
        let head: String = user.chars().take(24).collect();
        Reply::text(&format!("ack {head}"))
    }
}

// ---- a loopback push service --------------------------------------------------------------------

/// A browser's subscription keys.
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

    fn decrypt(&self, body: Vec<u8>) -> Vec<u8> {
        web_push_native::decrypt(body, &self.secret, &Auth::clone_from_slice(&self.auth))
            .expect("the push decrypts with the subscription keys")
    }
}

/// Every POST it receives (path, body), answered `201`.
struct PushService {
    port: u16,
    got: mpsc::Receiver<(String, Vec<u8>)>,
}

impl PushService {
    fn start() -> PushService {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the push service");
        let port = listener.local_addr().expect("push address").port();
        let (tx, got) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 {
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
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                if tx.send((path, body)).is_err() {
                    return;
                }
            }
        });
        PushService { port, got }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/push/phone", self.port)
    }
}

// ---- driving ------------------------------------------------------------------------------------

/// The newest `live` feed value the device received, kept current by [`Watch::pump`].
struct Watch {
    live: LiveRef,
    latest: Option<LiveSession>,
}

impl Watch {
    /// Moves every queued `live` value out of `d` (the newest wins).
    fn absorb(&mut self, d: &mut Device) {
        let live = self.live.clone();
        let mut latest = self.latest.take();
        d.records.retain(|r| match event(r) {
            Some(Event::Feed {
                value: FeedValue::Live { sessions, .. },
                ..
            }) => {
                if let Some(s) = sessions.into_iter().find(|s| s.live == live) {
                    latest = Some(s);
                }
                false
            }
            _ => true,
        });
        self.latest = latest;
    }

    /// Services the device for `for_`.
    fn pump(&mut self, d: &mut Device, for_: Duration) {
        let end = Instant::now() + for_;
        while Instant::now() < end {
            d.poll(SLICE);
            self.absorb(d);
        }
    }

    /// Services the device until the newest value satisfies `f`.
    fn until(
        &mut self,
        d: &mut Device,
        what: &str,
        timeout: Duration,
        f: impl Fn(&LiveSession) -> bool,
    ) -> LiveSession {
        let end = Instant::now() + timeout;
        loop {
            self.absorb(d);
            if let Some(s) = self.latest.as_ref().filter(|s| f(s)) {
                return s.clone();
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}; the newest live value: {:#?}",
                self.latest
            );
            assert!(
                d.closed.is_none(),
                "closed ({:?}) waiting for {what}",
                d.closed
            );
            d.poll(SLICE);
        }
    }

    /// omp idle (its title), nothing queued, nothing pending; a short settle first lets a
    /// just-finished turn reach the feed.
    fn idle(&mut self, d: &mut Device) -> LiveSession {
        self.pump(d, Duration::from_millis(1500));
        self.until(d, "an idle session", TURN, |s| {
            s.activity == Activity::Idle && !s.busy && s.pending_ui.is_empty()
        })
    }

    /// omp is working on a turn.
    fn working(&mut self, d: &mut Device, what: &str) {
        self.until(d, what, STEP, |s| s.activity == Activity::Busy);
    }

    /// The first pending request of `kind`.
    fn pending(&mut self, d: &mut Device, kind: UiKind) -> UiRequestView {
        let s = self.until(d, &format!("a pending {kind:?} request"), TURN, |s| {
            s.pending_ui.iter().any(|u| u.kind == kind)
        });
        s.pending_ui
            .into_iter()
            .find(|u| u.kind == kind)
            .expect("pending")
    }
}

/// Services `d` until the model received a request (index `after` on) that `f` accepts.
fn model_request(
    d: &mut Device,
    w: &mut Watch,
    model: &FakeModel,
    after: usize,
    what: &str,
    f: impl Fn(&Value) -> bool,
) -> Value {
    let end = Instant::now() + TURN;
    loop {
        if let Some(r) = model.find(after, &f) {
            return r;
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for the model request {what}; requests since: {:#}",
            Value::Array(model.since(after))
        );
        d.poll(SLICE);
        w.absorb(d);
    }
}

/// Sends journaled `op` and returns its outcome.
fn journaled(d: &mut Device, w: &mut Watch, seq: &mut u64, op: Op) -> OutcomeView {
    let s = *seq;
    *seq += 1;
    assert!(!accepted(d.call(&mut [], Some(s), op)), "a fresh request");
    let o = d.outcome(&mut [], s, TURN);
    w.absorb(d);
    o
}

fn done(o: OutcomeView) -> DoneView {
    match o {
        OutcomeView::Done { result } => result,
        other => panic!("not done: {other:?}"),
    }
}

/// Composes `text` (with `attachments`) in `mode`; the outcome must be done.
fn compose(
    d: &mut Device,
    w: &mut Watch,
    seq: &mut u64,
    text: &str,
    attachments: Vec<String>,
    mode: api::ComposeMode,
) -> DoneView {
    let live = w.live.clone();
    done(journaled(
        d,
        w,
        seq,
        Op::Compose {
            live,
            text: text.into(),
            attachments,
            mode,
        },
    ))
}

fn answer_ui(d: &mut Device, w: &mut Watch, seq: &mut u64, request: &str, value: UiAnswer) -> bool {
    let live = w.live.clone();
    match done(journaled(
        d,
        w,
        seq,
        Op::AnswerUi {
            live,
            request: request.into(),
            value,
        },
    )) {
        DoneView::Answered { applied } => applied,
        other => panic!("answer_ui: {other:?}"),
    }
}

/// The tool messages after the newest user message.
fn tool_texts(req: &Value) -> Vec<String> {
    fm::after_last_user(req)
        .iter()
        .filter(|m| fm::role(m) == "tool")
        .map(fm::text)
        .collect()
}

fn not_allowed(r: ReplyResult, need: api::Class) {
    match r {
        ReplyResult::Err(ApiError::NotAllowed { need: n }) if n == need => {}
        other => panic!("expected not_allowed {{{need:?}}}: {other:?}"),
    }
}

/// Every chunk of `download`, decoded.
fn download_all(d: &mut Device, download: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        match d.call(
            &mut [],
            None,
            Op::Download {
                download: download.into(),
                offset: bytes.len() as u64,
            },
        ) {
            ReplyResult::Ok(Resp::Chunk { data, eof }) => {
                bytes.extend(STANDARD.decode(data).expect("chunk base64"));
                if eof {
                    return bytes;
                }
            }
            other => panic!("download: {other:?}"),
        }
    }
}

/// Types `bytes` into terminal view `view` and waits until the live session shows `done`.
/// Input refused `input_held` (omp is still receiving an injected command) is typed again once
/// the view's hold is released, as a client must; returns how often that happened.
fn type_keys(
    d: &mut Device,
    w: &mut Watch,
    view: u32,
    bytes: &[u8],
    done: impl Fn(&LiveSession) -> bool,
) -> usize {
    let mut refused = 0;
    loop {
        d.send(&[Record::Input {
            view,
            ctrl_c: false,
            bytes: bytes.to_vec(),
        }]);
        let end = Instant::now() + STEP;
        loop {
            w.pump(d, SLICE);
            if w.latest.as_ref().is_some_and(&done) {
                return refused;
            }
            let held = d.records.iter().any(|r| match event(r) {
                Some(Event::InputRefused { view: v, reason }) if v == view => {
                    assert_eq!(reason, api::InputRefusal::InputHeld, "input refused");
                    true
                }
                _ => false,
            });
            if held {
                refused += 1;
                assert!(refused <= 10, "the view's input stays held");
                d.records.retain(
                    |r| !matches!(event(r), Some(Event::InputRefused { view: v, .. }) if v == view),
                );
                d.take(
                    &mut [],
                    "the view's input released",
                    STEP,
                    |r| match event(r)? {
                        Event::TermState {
                            view: v,
                            input_held: false,
                        } if v == view => Some(()),
                        _ => None,
                    },
                );
                break;
            }
            assert!(Instant::now() < end, "typed {bytes:?} without effect");
        }
    }
}

// ---- the test -----------------------------------------------------------------------------------

#[test]
fn interactive_paths_with_a_real_omp() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let config = format!(
        "{OMP_CONFIG}modelRoles:\n  default: {}\ntools:\n  approvalMode: always-ask\n",
        fm::MODEL
    );
    let sb = Sandbox::new("rdl", &config);
    // SAFETY: still single-threaded (no server, relay or client started yet).
    unsafe {
        std::env::set_var("OMP_DECK_PUSH_TEST_LOOPBACK", "1");
    }
    sb.report_failures();
    sb.seed_session("seed-device-live");
    let notes = sb.proj().join("notes.txt");
    std::fs::write(&notes, "steer target\n").expect("notes");
    let model = FakeModel::start(script(notes));
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let pushes = PushService::start();
    let relay = Relay::start(sb.root.join("relay"));

    let (phone, viewer) = {
        let client = sb.client();
        wait_for("the gateway link", Duration::from_secs(30), || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .expect("relay set");
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        (
            pair(
                &client,
                &relay,
                "Phone",
                Permissions {
                    control: true,
                    manage: false,
                },
            ),
            pair(
                &client,
                &relay,
                "Viewer",
                Permissions {
                    control: false,
                    manage: false,
                },
            ),
        )
    };
    let project = api::project_id(sb.proj().to_string_lossy().as_bytes());

    let mut a = Device::connect(&relay, &phone, "Phone", true);
    let mut seq = a.welcome.next_seq;

    // -- a session in the project, idle under live control --
    let roots = match a.call(&mut [], None, Op::Browse { dir: None }) {
        ReplyResult::Ok(Resp::Dir { entries, .. }) => entries,
        other => panic!("browse roots: {other:?}"),
    };
    let home = roots.iter().find(|e| e.name == "~").expect("home root");
    let proj_dir = match a.call(
        &mut [],
        None,
        Op::Browse {
            dir: Some(home.dir.clone()),
        },
    ) {
        ReplyResult::Ok(Resp::Dir { entries, .. }) => {
            entries
                .into_iter()
                .find(|e| e.name == "proj")
                .expect("proj listed")
                .dir
        }
        other => panic!("browse home: {other:?}"),
    };
    let launch = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(launch),
        Op::NewSessionIn { dir: proj_dir }
    )));
    let live = match done(a.outcome(&mut [], launch, TURN)) {
        DoneView::Session { live } => live,
        other => panic!("new_session_in: {other:?}"),
    };
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let mut w = Watch {
        live: live.clone(),
        latest: None,
    };
    w.until(
        &mut a,
        "an idle live-control session",
        Duration::from_secs(90),
        |s| s.live_control && !s.busy && s.activity == Activity::Idle,
    );

    // -- (f) permissions: View-only refused Control, Control refused Manage --
    {
        let mut v = Device::connect(&relay, &viewer, "Viewer", true);
        let ops = [
            Op::Compose {
                live: live.clone(),
                text: "refused".into(),
                attachments: Vec::new(),
                mode: api::ComposeMode::Send,
            },
            Op::AnswerUi {
                live: live.clone(),
                request: "u-1".into(),
                value: UiAnswer::Cancel {},
            },
            Op::RunningKill {
                ids: vec!["r-0000000000000000".into()],
            },
        ];
        for (vseq, op) in (v.welcome.next_seq..).zip(ops) {
            let name = op.name();
            let r = v.call(&mut [&mut a], Some(vseq), op);
            eprintln!("(f) view-only {name}: {r:?}");
            not_allowed(r, api::Class::Control);
        }
        for op in [
            Op::OmpSet {
                key: "theme".into(),
                value: json!("dark"),
            },
            Op::WorktreeRemove {
                project: project.clone(),
                park: false,
            },
        ] {
            let name = op.name();
            let r = a.call(&mut [&mut v], Some(seq), op);
            seq += 1;
            eprintln!("(f) control-only {name}: {r:?}");
            not_allowed(r, api::Class::Manage);
        }
    }
    w.absorb(&mut a);

    // -- (g) a fresh session's id is known right after a long first turn --
    // omp writes the file only when the first reply lands and drops its handle when it rewrites
    // the file (the title), so the open-file poll alone may never learn it; the gateway asks live
    // control instead.
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "FIRST-LONG-MARK take your time",
        Vec::new(),
        api::ComposeMode::Send,
    );
    w.working(&mut a, "the long first turn");
    w.until(&mut a, "the long first turn's end", TURN, |s| {
        s.activity == Activity::Idle
    });
    let ended = Instant::now();
    let s = w.until(
        &mut a,
        "the session id after the first turn",
        ID_AFTER_TURN,
        |s| s.session.is_some(),
    );
    eprintln!(
        "(g) session {:?} known {:.1} s after the long first turn",
        s.session,
        ended.elapsed().as_secs_f32()
    );
    w.idle(&mut a);

    // -- (c) compose with an image attachment --
    let png = gradient_png(PNG_SIDE);
    let png_b64 = STANDARD.encode(&png);
    let upload = match a.call(
        &mut [],
        None,
        Op::UploadBegin {
            name: "dot.png".into(),
            mime: "image/png".into(),
            size: png.len() as u64,
        },
    ) {
        ReplyResult::Ok(Resp::Upload { upload }) => upload,
        other => panic!("upload_begin: {other:?}"),
    };
    let r = a.call(
        &mut [],
        None,
        Op::UploadChunk {
            upload: upload.clone(),
            offset: 0,
            data: png_b64.clone(),
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let before = model.count();
    let d = compose(
        &mut a,
        &mut w,
        &mut seq,
        "IMG-MARK what is in this picture",
        vec![upload],
        api::ComposeMode::Send,
    );
    eprintln!("(c) compose with attachment: {d:?}");
    let req = model_request(&mut a, &mut w, &model, before, "with the image", |r| {
        fm::has_tools(r) && fm::last_user_text(r).contains("IMG-MARK")
    });
    let user = fm::last_user(&req).expect("user message");
    let images = fm::images(user);
    eprintln!(
        "(c) model got text {:?} and {} image part(s): {:?}",
        fm::text(user),
        images.len(),
        images
            .iter()
            .map(|u| u.chars().take(48).collect::<String>())
            .collect::<Vec<_>>()
    );
    assert_eq!(images.len(), 1, "one image part: {user:#}");
    let (head, data) = images[0].split_once(',').expect("a data URL");
    assert!(
        head.starts_with("data:image/") && head.ends_with(";base64"),
        "{head}"
    );
    let sent = STANDARD.decode(data).expect("image base64");
    eprintln!(
        "(c) the model's image is {} the uploaded bytes ({} bytes, {head})",
        if sent == png { "exactly" } else { "NOT" },
        sent.len()
    );
    assert!(sent == png, "omp passed the uploaded image on unchanged");
    let fm_text = fm::text(user);
    assert!(
        fm_text.contains("IMG-MARK what is in this picture"),
        "{fm_text}"
    );
    w.idle(&mut a);
    let session = w
        .until(&mut a, "omp's session id", TURN, |s| s.session.is_some())
        .session
        .expect("session id");

    // -- (d) the transcript's image and its blob, byte for byte --
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Transcript {
                session: session.clone(),
                agent: None,
                tail: 50,
            },
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let image = a.take(
        &mut [],
        "the user item with its image",
        TURN,
        |r| match event(r)? {
            Event::Feed {
                value: FeedValue::Transcript { items, .. },
                ..
            } => items.into_iter().find_map(|i| match i {
                Item::User { text, images, .. }
                    if text.contains("IMG-MARK") && !images.is_empty() =>
                {
                    Some(images[0].clone())
                }
                _ => None,
            }),
            _ => None,
        },
    );
    eprintln!("(d) transcript image: {image:?}");
    let (download, size, mime) = match a.call(
        &mut [],
        None,
        Op::Blob {
            session: session.clone(),
            agent: None,
            blob: image.blob.clone(),
        },
    ) {
        ReplyResult::Ok(Resp::Download {
            download,
            size,
            mime,
            ..
        }) => (download, size, mime),
        other => panic!("blob: {other:?}"),
    };
    let bytes = download_all(&mut a, &download);
    eprintln!("(d) blob {mime}, {size} bytes downloaded");
    assert_eq!(bytes.len() as u64, size);
    assert_eq!(bytes, png, "the blob is exactly the uploaded image");
    let r = a.call(
        &mut [],
        None,
        Op::Unsubscribe {
            feed: Feed::Transcript {
                session: session.clone(),
                agent: None,
                tail: 50,
            },
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    w.absorb(&mut a);

    // -- (a) an `ask` answered from the device --
    let before = model.count();
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "ASK-MARK choose one",
        Vec::new(),
        api::ComposeMode::Send,
    );
    let ask = w.pending(&mut a, UiKind::Ask);
    eprintln!("(a) pending ask: {ask:?}");
    assert_eq!(ask.questions.len(), 1);
    let q = &ask.questions[0];
    assert_eq!(q.id, "q1");
    assert_eq!(q.question, "Pick a color?");
    let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["red", "blue"]);
    assert!(answer_ui(
        &mut a,
        &mut w,
        &mut seq,
        &ask.request,
        UiAnswer::Ask {
            answers: vec![UiAskAnswer {
                id: "q1".into(),
                selected: vec![1],
                custom: None,
            }],
        },
    ));
    let req = model_request(&mut a, &mut w, &model, before, "with the ask result", |r| {
        fm::last_user_text(r).contains("ASK-MARK") && !tool_texts(r).is_empty()
    });
    let result = tool_texts(&req).join("\n");
    eprintln!("(a) the model got the ask result {result:?}");
    assert!(result.contains("blue"), "{result}");
    assert!(!result.contains("red"), "{result}");
    w.idle(&mut a);

    // -- an approval denied from the device --
    let before = model.count();
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "DENY-MARK run it",
        Vec::new(),
        api::ComposeMode::Send,
    );
    let approve = w.pending(&mut a, UiKind::Approve);
    eprintln!("(a) pending approval: {approve:?}");
    let view = approve.approve.as_ref().expect("approve view");
    assert!(view.full.contains("echo deny-run"), "{view:?}");
    assert!(answer_ui(
        &mut a,
        &mut w,
        &mut seq,
        &approve.request,
        UiAnswer::Approve {
            decision: ApproveDecision::Deny {},
        },
    ));
    let req = model_request(&mut a, &mut w, &model, before, "with the denial", |r| {
        fm::last_user_text(r).contains("DENY-MARK") && !tool_texts(r).is_empty()
    });
    let result = tool_texts(&req).join("\n");
    eprintln!("(a) the model got the denied result {result:?}");
    assert!(
        result.to_ascii_lowercase().contains("denied"),
        "a denial: {result}"
    );
    assert!(
        !result.lines().any(|l| l.trim() == "deny-run"),
        "the command did not run: {result}"
    );
    w.idle(&mut a);

    // -- the terminal answers first; the device's later answer is not applied --
    let before = model.count();
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "RACE-MARK run it",
        Vec::new(),
        api::ComposeMode::Send,
    );
    let approve = w.pending(&mut a, UiKind::Approve);
    let view = match a.call(
        &mut [],
        None,
        Op::Attach {
            live: live.clone(),
            mode: ViewMode::Input,
        },
    ) {
        ReplyResult::Ok(Resp::Attached { view, .. }) => view,
        other => panic!("attach: {other:?}"),
    };
    a.take(&mut [], "the view's snapshot", STEP, |r| match r {
        Record::Snapshot {
            view: v,
            last: true,
            ..
        } if *v == view => Some(()),
        _ => None,
    });
    // omp ignores Enter for its first 500 ms (`APPROVE_ARM_MS`, so a key meant for the composer
    // never answers a prompt as it appears); a person reads the prompt first.
    w.pump(&mut a, Duration::from_secs(1));
    let request = approve.request.clone();
    let refused = type_keys(&mut a, &mut w, view, b"\r", |s| {
        s.pending_ui.iter().all(|u| u.request != request)
    });
    eprintln!(
        "(a) Enter typed in the device's terminal view answered the approval \
         ({refused} input_held refusal(s) retyped)"
    );
    let req = model_request(
        &mut a,
        &mut w,
        &model,
        before,
        "with the approved result",
        |r| fm::last_user_text(r).contains("RACE-MARK") && !tool_texts(r).is_empty(),
    );
    let result = tool_texts(&req).join("\n");
    eprintln!("(a) terminal Enter approved; the model got {result:?}");
    assert!(result.contains("race-run"), "{result}");
    w.idle(&mut a);
    let applied = answer_ui(
        &mut a,
        &mut w,
        &mut seq,
        &approve.request,
        UiAnswer::Approve {
            decision: ApproveDecision::Deny {},
        },
    );
    eprintln!("(a) the device's late answer: applied={applied}");
    assert!(!applied, "the terminal won");
    let r = a.call(&mut [], None, Op::Detach { view });
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    a.records.retain(|r| {
        !matches!(r, Record::Snapshot { view: v, .. } | Record::Output { view: v, .. } if *v == view)
    });

    // -- (b) steer and follow-up while busy --
    let before = model.count();
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "STEER-MARK read the notes",
        Vec::new(),
        api::ComposeMode::Send,
    );
    model_request(&mut a, &mut w, &model, before, "the steered turn", |r| {
        fm::last_user_text(r).contains("STEER-MARK")
    });
    w.working(&mut a, "the steered turn working");
    let steer = compose(
        &mut a,
        &mut w,
        &mut seq,
        "STEER-TEXT change course",
        Vec::new(),
        api::ComposeMode::Steer,
    );
    let follow = compose(
        &mut a,
        &mut w,
        &mut seq,
        "FOLLOW-TEXT afterwards",
        Vec::new(),
        api::ComposeMode::FollowUp,
    );
    eprintln!("(b) steer {steer:?}; follow-up {follow:?}");
    let steered = model_request(&mut a, &mut w, &model, before, "with the steer", |r| {
        fm::last_user_text(r).contains("STEER-TEXT")
    });
    let msgs = fm::messages(&steered);
    let steer_at = msgs
        .iter()
        .rposition(|m| fm::role(m) == "user" && fm::text(m).contains("STEER-TEXT"))
        .expect("steer message");
    let mark_at = msgs
        .iter()
        .rposition(|m| fm::role(m) == "user" && fm::text(m).contains("STEER-MARK"))
        .expect("prompt");
    let between: Vec<&str> = msgs[mark_at + 1..steer_at].iter().map(fm::role).collect();
    eprintln!("(b) the steer arrived as a user message after {between:?} of the same turn");
    assert!(
        between.contains(&"tool"),
        "the steer follows the running tool call: {between:?}"
    );
    assert!(
        !msgs.iter().any(|m| fm::text(m).contains("FOLLOW-TEXT")),
        "the follow-up waits for the turn's end"
    );
    let followed = model_request(&mut a, &mut w, &model, before, "with the follow-up", |r| {
        fm::last_user_text(r).contains("FOLLOW-TEXT")
    });
    let msgs = fm::messages(&followed);
    let follow_at = msgs
        .iter()
        .rposition(|m| fm::role(m) == "user" && fm::text(m).contains("FOLLOW-TEXT"))
        .expect("follow-up message");
    let steer_at = msgs
        .iter()
        .rposition(|m| fm::role(m) == "user" && fm::text(m).contains("STEER-TEXT"))
        .expect("steer message");
    let between: Vec<&str> = msgs[steer_at + 1..follow_at].iter().map(fm::role).collect();
    eprintln!("(b) the follow-up arrived after the steered reply: {between:?}");
    assert!(
        between.contains(&"assistant"),
        "the follow-up comes after the agent finished: {between:?}"
    );
    w.idle(&mut a);

    // -- (e) push for a finished turn; none while visible --
    let ua = Ua::new();
    let r = a.call(
        &mut [],
        None,
        Op::PushSubscribe {
            endpoint: pushes.endpoint(),
            p256dh: ua.p256dh(),
            auth: URL_SAFE_NO_PAD.encode(ua.auth),
        },
    );
    eprintln!("(e) push_subscribe: {r:?}");
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let r = a.call(&mut [], None, Op::Presence { visible: false });
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let before = model.count();
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "PUSH-MARK take your time",
        Vec::new(),
        api::ComposeMode::Send,
    );
    model_request(&mut a, &mut w, &model, before, "the slow turn", |r| {
        fm::last_user_text(r).contains("PUSH-MARK")
    });
    let (path, body) = {
        let end = Instant::now() + TURN;
        loop {
            if let Ok(p) = pushes.got.try_recv() {
                break p;
            }
            assert!(Instant::now() < end, "no push for the finished turn");
            w.pump(&mut a, SLICE);
        }
    };
    let payload: Value =
        serde_json::from_slice(&ua.decrypt(body)).expect("the push payload is JSON");
    eprintln!("(e) push to {path}: {payload}");
    assert_eq!(path, "/push/phone");
    assert_eq!(payload["web_push"], 8030);
    let n = &payload["notification"];
    assert_eq!(n["title"], "OmpDeck");
    assert_eq!(n["body"], "A session finished");
    let navigate = n["navigate"].as_str().expect("navigate");
    assert!(
        navigate.ends_with(&format!("/#s={session}")),
        "{navigate} names {session}"
    );
    w.idle(&mut a);
    let quiet_until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < quiet_until {
        if let Ok((p, _)) = pushes.got.try_recv() {
            panic!("a second push for one finished turn: {p}");
        }
        w.pump(&mut a, SLICE);
    }

    let r = a.call(&mut [], None, Op::Presence { visible: true });
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let before = model.count();
    compose(
        &mut a,
        &mut w,
        &mut seq,
        "QUIET-MARK take your time",
        Vec::new(),
        api::ComposeMode::Send,
    );
    model_request(
        &mut a,
        &mut w,
        &model,
        before,
        "the visible slow turn",
        |r| fm::last_user_text(r).contains("QUIET-MARK"),
    );
    w.working(&mut a, "the visible turn working");
    w.idle(&mut a);
    w.pump(&mut a, Duration::from_secs(3));
    if let Ok((p, _)) = pushes.got.try_recv() {
        panic!("a push while the device is visible: {p}");
    }
    eprintln!("(e) no push while visible");
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
