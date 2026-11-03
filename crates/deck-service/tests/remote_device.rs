//! The device application protocol end to end, no GUI and no phone: a real gateway owner in
//! Always-on mode (its GUI client dropped) links to an in-test relay (`deck_relay::serve` on
//! loopback); scripted Rust devices pair, reconnect (Noise IK) and speak `deck_remote::api`
//! records against a real omp session.
//!
//! Covers `new_session` in a project, an observe view (snapshot then output; its input refused
//! `not_allowed`), a runtime command (`action rename`) delivered and visible in the `live` feed,
//! a Manage op refused without the permission, a `new_session` whose socket dropped before
//! anything was read recovered by `outcome` after an IK reconnect and launched exactly once
//! (also after its seq was re-sent and answered `duplicate`), an unknown seq below `next_seq`
//! answered `expired`, a device that stops acknowledging closed `STALLED` while another one
//! keeps working, a local client refused the outcome of a device's request key, and an attach
//! refused `maintenance` under the maintenance barrier.
//!
//! Everything runs in a private HOME/XDG tree under a unique `omp-deck-test-*` namespace; the
//! real omp store and the real `omp-deck` tmux server are never touched. Skipped (with a
//! message) when omp is not installed.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::config::GatewayMode;
use deck_remote::api::{
    self, ApiError, DoneView, Event, Feed, FeedValue, LiveRef, LiveSession, Op, OutcomeView,
    Record, ReplyResult, Resp, SessionAction, ViewMode,
};
use deck_remote::noise::{DeviceHello, Initiator, Keypair, Mode, PairingInvite, Session};
use deck_remote::relay::{self, DEVICE_PATH, HELLO_OK, Hello, RoomKey, close};
use deck_service::controller::RequestKey;
use deck_service::gateway::client::{GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Done, GatewayError, MaintenanceOp, MobileView, Mutation, Outcome, PairingState, Permissions,
    Refusal, RelayLink, Role, SecretText,
};
use deck_service::gateway::{MaintenancePurpose, Profile};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const GATEWAY: &str = env!("CARGO_BIN_EXE_omp-deck-gateway");
const STEP: Duration = Duration::from_secs(20);
/// A device that holds messages unacknowledged this long is closed (`OMP_DECK_DEVICE_STALL_MS`).
const STALL: Duration = Duration::from_secs(4);
/// How long one device read waits before the other devices are serviced.
const SLICE: Duration = Duration::from_millis(40);

// ---- sandbox ------------------------------------------------------------------------------------

struct Sandbox {
    root: PathBuf,
    ns: String,
    profile: Profile,
}

impl Sandbox {
    fn new() -> Sandbox {
        // Short: the ctl socket path (`$XDG_RUNTIME_DIR/omp-deck/ctl/<ns>/<leaf>/ctl.sock`, both
        // under this root) must stay well inside `sun_path`, or omp runs without live control.
        let ns = format!("omp-deck-test-rd-{}", std::process::id());
        let root = std::env::temp_dir().join(&ns);
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            "home",
            "home/AppData/Roaming",
            "home/AppData/Local",
            "config/omp-deck",
            "data",
            "state",
            "run",
            "relay",
            "home/proj",
        ] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dir");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700))
                .expect("private runtime dir");
        }
        let agent = root.join("home/.omp/agent");
        let envs: [(&str, PathBuf); 7] = [
            ("HOME", root.join("home")),
            ("USERPROFILE", root.join("home")),
            ("XDG_CONFIG_HOME", root.join("config")),
            ("XDG_DATA_HOME", root.join("data")),
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_RUNTIME_DIR", root.join("run")),
            // omp's own store: never the real one, on any platform.
            ("PI_CODING_AGENT_DIR", agent.clone()),
        ];
        // SAFETY: the only test of this binary; set before any thread exists.
        unsafe {
            for (k, v) in envs {
                std::env::set_var(k, v);
            }
            std::env::set_var("OMP_DECK_HOST_NS", &ns);
            std::env::set_var("OMP_DECK_TMUX_SOCKET", &ns);
            std::env::set_var("PI_NOTIFICATIONS", "off");
            // Never the desktop's session bus: the owner's notifier must not pop up for test sessions.
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", root.join("run/no-bus").display()),
            );
            std::env::set_var("OMP_DECK_GATEWAY_LOG", "debug");
            // The autostarted owner closes a silent device after `STALL`.
            std::env::set_var("OMP_DECK_DEVICE_STALL_MS", STALL.as_millis().to_string());
            let host = PathBuf::from(GATEWAY).with_file_name(if cfg!(windows) {
                "omp-deck-host.exe"
            } else {
                "omp-deck-host"
            });
            if host.is_file() {
                std::env::set_var("OMP_DECK_HOST_EXE", host);
            }
        }
        // No first-run wizard, no collab: omp starts straight into its editor.
        std::fs::create_dir_all(&agent).expect("agent dir");
        std::fs::write(
            agent.join("config.yml"),
            "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\n",
        )
        .expect("omp config");
        let profile = Profile {
            host_namespace: ns.clone(),
            settings: root.join("config/omp-deck/state.json"),
            sessions_root: agent.join("sessions"),
        };
        // Always on, written before the owner starts: the mode's lifetime rule (the owner
        // outlives its GUI) without an OS autostart registration in a test.
        std::fs::write(&profile.settings, br#"{"gateway_mode":"Background"}"#).expect("settings");
        Sandbox { root, ns, profile }
    }

    fn proj(&self) -> PathBuf {
        // Under HOME: device views show it `~`-abbreviated.
        self.root.join("home/proj")
    }

    /// A session of the sandbox project in the store, so the project has an id.
    fn seed_session(&self, id: &str) {
        let dir = self.profile.sessions_root.join("--proj--");
        std::fs::create_dir_all(&dir).expect("store dir");
        let header = serde_json::json!({
            "type": "session",
            "id": id,
            "cwd": self.proj().to_string_lossy(),
            "timestamp": "2026-10-29T00:00:00Z",
        });
        let title = serde_json::json!({ "type": "title", "title": "Seeded" });
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{header}\n{title}\n"),
        )
        .expect("seed session");
    }

    fn gateway<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Output {
        Command::new(GATEWAY)
            .args(args)
            .args([
                std::ffi::OsString::from("--ns"),
                self.ns.clone().into(),
                "--settings".into(),
                self.profile.settings.clone().into(),
                "--sessions-root".into(),
                self.profile.sessions_root.clone().into(),
            ])
            .output()
            .expect("run omp-deck-gateway")
    }

    fn owner_pid(&self) -> Option<u32> {
        let out = self.gateway(&["status", "--json"]);
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v["status"]["pid"].as_u64().map(|p| p as u32)
    }

    /// Any failure prints the owner's log below the panic message (readable from a CI log).
    fn report_failures(&self) {
        let dir = self.profile.settings.with_file_name("gateway");
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            let log = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path().join("gateway.log")).ok())
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!("==== owner gateway.log ====\n{log}\n==== end ====");
        }));
    }

    fn client(&self) -> GatewayClient {
        let mut opts = GatewayOptions::new(self.profile.clone(), Role::Gui, Arc::new(|| {}));
        opts.gateway_exe = Some(PathBuf::from(GATEWAY));
        GatewayClient::start(opts)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Our own test backend only (never the default tmux server): the owner first, then every
        // session it left behind.
        if let Some(pid) = self.owner_pid() {
            kill_process(pid);
        }
        // Host-backend sessions outlive the owner (and their sockets go with the root): end every
        // session of this test's namespace.
        if let Ok(ns) = deck_host::Namespace::new(&self.ns) {
            for name in ns.list() {
                let _ = ns.kill(&name);
            }
        }
        if !cfg!(windows) {
            let _ = Command::new("tmux")
                .args(["-L", &self.ns, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn kill_process(pid: u32) {
    let _ = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
    } else {
        Command::new("kill").args(["-9", &pid.to_string()]).output()
    };
}

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ---- relay --------------------------------------------------------------------------------------

struct Relay {
    port: u16,
    key: RoomKey,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    fn start(dir: PathBuf) -> Relay {
        let (key, _) = deck_relay::rooms::enroll(&dir, "test").expect("enroll a room");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the relay");
        let port = listener.local_addr().expect("relay address").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("relay runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                deck_relay::serve(deck_relay::Config::new(dir), listener, async move {
                    let _ = stopped.await;
                })
                .await
                .expect("relay serve");
            });
        });
        Relay {
            port,
            key,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- websocket ----------------------------------------------------------------------------------

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn set_timeout(ws: &Ws, t: Duration) {
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(t)).expect("read timeout");
    }
}

/// One binary message, the close code, or `None` when nothing came within the read timeout.
fn read_once(ws: &mut Ws) -> Option<Result<Vec<u8>, u16>> {
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => return Some(Ok(b.to_vec())),
            Ok(Message::Close(frame)) => {
                return Some(Err(frame.map_or(1005, |f| u16::from(f.code))));
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Text(t)) => panic!("text from the relay: {t}"),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return None;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Some(Err(1006));
            }
            Err(e) => panic!("device read: {e}"),
        }
    }
}

fn read_blocking(ws: &mut Ws) -> Result<Vec<u8>, u16> {
    set_timeout(ws, STEP);
    read_once(ws).expect("an answer in time")
}

/// A device connection admitted with `token` (retried while the token is on its way).
fn admit(relay: &Relay, token: &relay::Token) -> Ws {
    let end = Instant::now() + STEP;
    loop {
        let (mut ws, _) = tungstenite::connect(format!("{}{DEVICE_PATH}", relay.url()))
            .expect("device websocket");
        ws.send(Message::binary(
            Hello::Device {
                room: relay.key.room,
                token: *token,
            }
            .encode(),
        ))
        .expect("device hello");
        match read_blocking(&mut ws) {
            Ok(b) if b[..] == HELLO_OK[..] => return ws,
            Ok(b) => panic!("unexpected hello answer {b:?}"),
            Err(code)
                if (code == close::UNAUTHORIZED || code == close::GATEWAY_OFFLINE)
                    && Instant::now() < end =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(code) => panic!("device refused by the relay: {code}"),
        }
    }
}

fn hello(name: &str) -> DeviceHello {
    DeviceHello {
        name: name.into(),
        platform: "test".into(),
        ts: now_ms(),
        api: 1,
        features: Vec::new(),
    }
}

// ---- gateway client -----------------------------------------------------------------------------

fn run(client: &GatewayClient, op: Mutation) -> Result<Done, String> {
    let m = client.mutate(op);
    match m.accepted.wait(STEP) {
        Some(Ok(_)) => {}
        Some(Err(e)) => return Err(e.to_string()),
        None => return Err("not accepted in time".into()),
    }
    match m.outcome.wait(STEP) {
        Some(Ok(Outcome::Done(d))) => Ok(*d),
        other => Err(format!("{other:?}")),
    }
}

fn wait_mobile(
    client: &GatewayClient,
    what: &str,
    f: impl Fn(&MobileView) -> bool,
) -> Arc<MobileView> {
    wait_for(what, STEP, || client.state().mobile.get().filter(|m| f(m)))
}

/// A paired device: its key, registry id and relay token.
struct Paired {
    key: Keypair,
    gateway: deck_remote::noise::PublicKey,
    id: String,
    token: relay::Token,
}

/// Pairs a device named `name` with `permissions` through the whole R3 flow.
fn pair(client: &GatewayClient, relay: &Relay, name: &str, permissions: Permissions) -> Paired {
    // The previous window ends once its connection is gone (shortly after it closed).
    let started = wait_for("a pairing window", STEP, || {
        match run(client, Mutation::PairingBegin) {
            Err(e) if e.contains("already open") => None,
            other => Some(other.expect("pairing begins")),
        }
    });
    let (pairing, invite) = match started {
        Done::PairingStarted {
            pairing, invite, ..
        } => (
            pairing,
            PairingInvite::parse(invite.expose()).expect("invite parses"),
        ),
        other => panic!("pairing-begin result: {other:?}"),
    };
    let key = Keypair::generate().expect("device key");
    let mut ws = admit(relay, &invite.token);
    let (init, msg1) = Initiator::start(
        Mode::Pairing { psk: &invite.psk },
        &key,
        &invite.gateway_public,
        &hello(name),
    )
    .expect("pairing msg1");
    ws.send(Message::binary(msg1)).expect("send msg1");
    wait_mobile(client, "the approval request", |m| {
        matches!(&m.pairing, Some(p) if p.pairing == pairing
            && matches!(&p.state, PairingState::Approval { .. }))
    });
    run(
        client,
        Mutation::PairingApprove {
            pairing,
            permissions,
        },
    )
    .expect("approved");
    let msg2 = read_blocking(&mut ws).expect("pairing message 2");
    let (_, welcome) = init.finish(&msg2).expect("pairing msg2");
    let token = welcome.token.expect("pairing welcome carries the token");
    let _ = ws.close(None);
    drop(ws);
    wait_mobile(client, "the window to close", |m| m.pairing.is_none());
    Paired {
        key,
        gateway: invite.gateway_public,
        id: welcome.device,
        token,
    }
}

// ---- device -------------------------------------------------------------------------------------

/// A device session speaking `deck_remote::api` over the relay.
struct Device {
    ws: Ws,
    noise: Session,
    /// Gateway messages processed (the `ACK` count).
    received: u64,
    /// Acknowledge every message (a stalling device does not).
    ack: bool,
    records: VecDeque<Record>,
    closed: Option<u16>,
    next_id: u64,
    welcome: api::Welcome,
}

impl Device {
    /// An IK reconnect of `p`; its first record must be `WELCOME`.
    fn connect(relay: &Relay, p: &Paired, name: &str) -> Device {
        let mut ws = admit(relay, &p.token);
        let (init, msg1) = Initiator::start(Mode::Reconnect, &p.key, &p.gateway, &hello(name))
            .expect("reconnect msg1");
        ws.send(Message::binary(msg1)).expect("send msg1");
        let msg2 = read_blocking(&mut ws).expect("reconnect message 2");
        let (mut noise, w) = init.finish(&msg2).expect("reconnect msg2");
        assert_eq!(w.device, p.id);
        let mut config = *noise.config();
        config.max_message = api::MAX_APP_MESSAGE;
        noise.set_config(config);
        let mut d = Device {
            ws,
            noise,
            received: 0,
            ack: true,
            records: VecDeque::new(),
            closed: None,
            next_id: 1,
            welcome: api::Welcome {
                protocol: 0,
                gateway_version: String::new(),
                epoch: String::new(),
                device: String::new(),
                name: String::new(),
                permissions: api::Permissions::default(),
                next_seq: 0,
                limits: api::Limits::default(),
                features: Vec::new(),
                push_key: String::new(),
            },
        };
        let end = Instant::now() + STEP;
        while d.records.is_empty() {
            assert!(Instant::now() < end, "no welcome");
            d.poll(SLICE);
            assert!(
                d.closed.is_none(),
                "closed before the welcome: {:?}",
                d.closed
            );
        }
        match d.records.pop_front() {
            Some(Record::Welcome(body)) => {
                d.welcome = serde_json::from_slice(&body).expect("welcome json");
            }
            other => panic!("first record is not WELCOME: {other:?}"),
        }
        assert_eq!(d.welcome.protocol, api::PROTOCOL);
        assert_eq!(d.welcome.device, p.id);
        d
    }

    fn send(&mut self, records: &[Record]) {
        let msg = api::encode(records).expect("encode records");
        for frame in self.noise.seal(&msg).expect("seal") {
            self.ws.send(Message::binary(frame)).expect("send frame");
        }
    }

    /// Reads for up to `wait`: one message's records are queued (and acknowledged).
    fn poll(&mut self, wait: Duration) {
        if self.closed.is_some() {
            std::thread::sleep(wait);
            return;
        }
        set_timeout(&self.ws, wait);
        match read_once(&mut self.ws) {
            None => {}
            Some(Err(code)) => self.closed = Some(code),
            Some(Ok(frame)) => {
                if let Some(msg) = self.noise.open(&frame).expect("open frame") {
                    self.received += 1;
                    let records = api::decode(&msg, api::Origin::Gateway).expect("gateway records");
                    self.records.extend(records);
                    if self.ack {
                        let messages = self.received;
                        self.send(&[Record::Ack { messages }]);
                    }
                }
            }
        }
    }

    /// Sends `op` (with `seq` when journaled) and returns its request id.
    fn request(&mut self, seq: Option<u64>, op: Op) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::to_vec(&api::Request { id, seq, op }).expect("request json");
        self.send(&[Record::Request(body)]);
        id
    }

    /// The first queued record `f` maps to `Some` (removed), reading while needed; `others`
    /// keep being serviced meanwhile.
    fn take<T>(
        &mut self,
        others: &mut [&mut Device],
        what: &str,
        timeout: Duration,
        mut f: impl FnMut(&Record) -> Option<T>,
    ) -> T {
        let end = Instant::now() + timeout;
        loop {
            if let Some(i) = self.records.iter().position(|r| f(r).is_some()) {
                let r = self.records.remove(i).expect("record");
                return f(&r).expect("matched");
            }
            assert!(
                self.closed.is_none(),
                "closed ({:?}) waiting for {what}",
                self.closed
            );
            if Instant::now() >= end {
                let recent: Vec<String> = self
                    .records
                    .iter()
                    .rev()
                    .take(4)
                    .map(|r| match r {
                        Record::Event(b) | Record::Reply(b) => {
                            String::from_utf8_lossy(b).into_owned()
                        }
                        other => format!("{:?}", other.kind()),
                    })
                    .collect();
                panic!("timed out waiting for {what}; newest records: {recent:#?}");
            }
            self.poll(SLICE);
            for o in others.iter_mut() {
                o.poll(Duration::from_millis(1));
            }
        }
    }

    fn reply(&mut self, others: &mut [&mut Device], id: u64) -> ReplyResult {
        self.take(others, &format!("reply {id}"), STEP, |r| match r {
            Record::Reply(b) => {
                let reply: api::Reply = serde_json::from_slice(b).expect("reply json");
                (reply.id == id).then_some(reply.result)
            }
            _ => None,
        })
    }

    fn call(&mut self, others: &mut [&mut Device], seq: Option<u64>, op: Op) -> ReplyResult {
        let id = self.request(seq, op);
        self.reply(others, id)
    }

    /// The final outcome event of `seq`.
    fn outcome(&mut self, others: &mut [&mut Device], seq: u64, timeout: Duration) -> OutcomeView {
        self.take(
            others,
            &format!("the outcome of {seq}"),
            timeout,
            |r| match event(r)? {
                Event::Outcome { seq: s, outcome } if s == seq => Some(outcome),
                _ => None,
            },
        )
    }

    /// The newest `live` feed value satisfying `f`.
    fn live(
        &mut self,
        others: &mut [&mut Device],
        what: &str,
        timeout: Duration,
        f: impl Fn(&[LiveSession]) -> bool,
    ) -> Vec<LiveSession> {
        self.take(others, what, timeout, |r| match event(r)? {
            Event::Feed {
                value: FeedValue::Live { sessions, .. },
                ..
            } if f(&sessions) => Some(sessions),
            _ => None,
        })
    }
}

fn event(r: &Record) -> Option<Event> {
    match r {
        Record::Event(b) => Some(serde_json::from_slice(b).expect("event json")),
        _ => None,
    }
}

fn accepted(r: ReplyResult) -> bool {
    match r {
        ReplyResult::Ok(Resp::Accepted { duplicate, .. }) => duplicate,
        other => panic!("not accepted: {other:?}"),
    }
}

fn rename(live: &LiveRef, name: &str) -> Op {
    Op::Action {
        live: live.clone(),
        action: SessionAction::Rename { name: name.into() },
    }
}

// ---- the test -----------------------------------------------------------------------------------

#[test]
fn devices_drive_a_real_session_through_the_relay() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let sb = Sandbox::new();
    sb.report_failures();
    sb.seed_session("seed-device-1");
    let relay = Relay::start(sb.root.join("relay"));

    // -- the owner's setup by a GUI client: relay, two paired devices --
    let (owner, phone, slow) = {
        let client = sb.client();
        wait_for("the gateway link", Duration::from_secs(30), || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        assert_eq!(
            client
                .state()
                .gateway
                .get()
                .map(|g| g.mode)
                .unwrap_or(GatewayMode::Background),
            GatewayMode::Background
        );
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .expect("relay set");
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        let phone = pair(
            &client,
            &relay,
            "Phone",
            Permissions {
                control: true,
                manage: false,
            },
        );
        let slow = pair(
            &client,
            &relay,
            "Slow",
            Permissions {
                control: false,
                manage: false,
            },
        );
        let owner = sb.owner_pid().expect("the owner runs");
        (owner, phone, slow)
        // The GUI client is dropped here: devices are served without it.
    };

    let project = api::project_id(sb.proj().to_string_lossy().as_bytes());
    let mut a = Device::connect(&relay, &phone, "Phone");
    assert!(a.welcome.permissions.control && !a.welcome.permissions.manage);
    let mut seq = a.welcome.next_seq;
    let first_seq = seq;

    // -- projects feed: the seeded project by id, no absolute path --
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Projects {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let projects = a.take(&mut [], "the projects feed", STEP, |r| match event(r)? {
        Event::Feed {
            value: FeedValue::Projects { projects, .. },
            ..
        } if projects.iter().any(|p| p.id == project) => Some(projects),
        _ => None,
    });
    let json = serde_json::to_string(&projects).expect("json");
    assert!(
        !json.contains(&*sb.root.to_string_lossy()),
        "absolute path in a device view: {json}"
    );

    // -- Manage is refused without the permission --
    let r = a.call(
        &mut [],
        Some(seq),
        Op::DeleteSessions {
            sessions: vec!["seed-device-1".into()],
        },
    );
    seq += 1;
    assert!(
        matches!(
            r,
            ReplyResult::Err(ApiError::NotAllowed {
                need: api::Class::Manage
            })
        ),
        "{r:?}"
    );

    // -- new_session in the project --
    let launch = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(launch),
        Op::NewSession {
            project: project.clone()
        }
    )));
    let live = match a.outcome(&mut [], launch, Duration::from_secs(60)) {
        OutcomeView::Done {
            result: DoneView::Session { live },
        } => live,
        other => panic!("new_session outcome: {other:?}"),
    };

    // omp comes up idle with live control (the `live` feed shows it without paths or keys).
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let sessions = a.live(
        &mut [],
        "an idle live-control session",
        Duration::from_secs(90),
        |s| {
            s.iter()
                .any(|s| s.live == live && s.live_control && !s.busy && s.exit_code.is_none())
        },
    );
    let json = serde_json::to_string(&sessions).expect("json");
    assert!(
        !json.contains(&*sb.root.to_string_lossy()),
        "absolute path in the live feed: {json}"
    );
    let shown = sessions.iter().find(|s| s.live == live).expect("shown");
    if shown.project.as_deref() != Some(project.as_str()) {
        // A folder spelled differently resolves on a worker: a corrected value follows.
        let (got, display) = (shown.project.clone(), shown.display.clone());
        let end = Instant::now() + STEP;
        loop {
            let fixed = a.records.iter().any(|r| {
                matches!(event(r), Some(Event::Feed {
                    value: FeedValue::Live { sessions, .. },
                    ..
                }) if sessions.iter().any(|s| s.live == live && s.project.as_deref() == Some(project.as_str())))
            });
            if fixed {
                break;
            }
            assert!(
                Instant::now() < end,
                "the live session's project {got:?} (display {display:?}) is not the \
                 project it was started in, {project:?}; the projects feed: {projects:#?}"
            );
            a.poll(SLICE);
        }
    }

    // -- an observe view: snapshot first, then output --
    let view = match a.call(
        &mut [],
        None,
        Op::Attach {
            live: live.clone(),
            mode: ViewMode::Observe,
        },
    ) {
        ReplyResult::Ok(Resp::Attached { view, mode }) => {
            assert_eq!(mode, ViewMode::Observe);
            view
        }
        other => panic!("attach: {other:?}"),
    };
    let first = a.take(&mut [], "the view's first record", STEP, |r| match r {
        Record::Snapshot { view: v, .. } | Record::Output { view: v, .. } if *v == view => {
            Some(r.clone())
        }
        _ => None,
    });
    assert!(
        matches!(first, Record::Snapshot { .. }),
        "the snapshot comes first: {first:?}"
    );
    if !matches!(first, Record::Snapshot { last: true, .. }) {
        a.take(&mut [], "the snapshot's last chunk", STEP, |r| match r {
            Record::Snapshot {
                view: v,
                last: true,
                ..
            } if *v == view => Some(()),
            _ => None,
        });
    }
    // An observe view never types: refused on the link, once per message.
    a.send(&[
        Record::Input {
            view,
            ctrl_c: false,
            bytes: b"x".to_vec(),
        },
        Record::Input {
            view,
            ctrl_c: false,
            bytes: b"y".to_vec(),
        },
    ]);
    a.take(
        &mut [],
        "the observe view's input refusal",
        STEP,
        |r| match event(r)? {
            Event::InputRefused {
                view: v,
                reason: api::InputRefusal::NotAllowed,
            } if v == view => Some(()),
            _ => None,
        },
    );

    // -- a runtime command: `action rename` delivered and visible in `live` --
    let title = "device-e2e-title";
    let first_rename = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(first_rename),
        rename(&live, title)
    )));
    match a.outcome(&mut [], first_rename, Duration::from_secs(60)) {
        OutcomeView::Done {
            result: DoneView::Submitted { delivery, .. },
        } => assert_eq!(delivery, api::DeliveryView::Delivered {}),
        other => panic!("rename outcome: {other:?}"),
    }
    a.take(
        &mut [],
        "terminal output after the snapshot",
        STEP,
        |r| match r {
            Record::Output { view: v, bytes } if *v == view && !bytes.is_empty() => Some(()),
            _ => None,
        },
    );
    a.live(&mut [], "the renamed title", Duration::from_secs(30), |s| {
        s.iter()
            .any(|s| s.live == live && s.title.as_deref().is_some_and(|t| t.contains(title)))
    });

    // -- a launch whose socket drops before anything is read --
    let dropped = seq;
    a.request(
        Some(dropped),
        Op::NewSession {
            project: project.clone(),
        },
    );
    drop(a);

    let mut a = Device::connect(&relay, &phone, "Phone");
    assert!(
        a.welcome.next_seq > dropped,
        "next_seq {} after seq {dropped}",
        a.welcome.next_seq
    );
    // Recovered by lookup: running, then final; never executed twice.
    let end = Instant::now() + Duration::from_secs(60);
    let second = loop {
        match a.call(&mut [], None, Op::Outcome { seq: dropped }) {
            ReplyResult::Ok(Resp::Outcome {
                outcome: OutcomeView::Running {},
            }) => {
                assert!(Instant::now() < end, "still running");
                std::thread::sleep(Duration::from_millis(200));
            }
            ReplyResult::Ok(Resp::Outcome {
                outcome:
                    OutcomeView::Done {
                        result: DoneView::Session { live },
                    },
            }) => break live,
            other => panic!("outcome lookup: {other:?}"),
        }
    };
    assert_ne!(second, live);
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let both =
        |s: &[LiveSession]| s.iter().any(|s| s.live == live) && s.iter().any(|s| s.live == second);
    let sessions = a.live(
        &mut [],
        "both launched sessions",
        Duration::from_secs(60),
        both,
    );
    assert_eq!(sessions.len(), 2, "one launch per request: {sessions:?}");
    let shown = sessions.iter().find(|s| s.live == live).expect("shown");
    assert!(
        shown.requests.iter().all(|r| r.seq <= dropped),
        "only this device's requests: {:?}",
        shown.requests
    );
    // The same seq with the same op is the same request: answered from the journal.
    assert!(accepted(a.call(
        &mut [],
        Some(dropped),
        Op::NewSession {
            project: project.clone(),
        },
    )));
    match a.outcome(&mut [], dropped, STEP) {
        OutcomeView::Done {
            result: DoneView::Session { live },
        } => assert_eq!(live, second),
        other => panic!("duplicate's outcome: {other:?}"),
    }
    // A seq already used for other work is refused.
    match a.call(&mut [], Some(dropped), rename(&live, "something else")) {
        ReplyResult::Err(ApiError::KeyReused {}) => {}
        other => panic!("reused seq for other work: {other:?}"),
    }
    // A seq below this session's `next_seq` that the journal does not know never runs.
    match a.call(&mut [], Some(first_seq), rename(&live, "from the past")) {
        ReplyResult::Err(ApiError::Expired {}) => {}
        other => panic!("an unknown seq below next_seq: {other:?}"),
    }
    // Still exactly two sessions: a fresh `live` value after the duplicate settled.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        a.poll(SLICE);
    }
    let r = a.call(
        &mut [],
        None,
        Op::Unsubscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    // Values from before the unsubscribe; the subscribe's own value may share its message.
    a.records
        .retain(|r| event(r).is_none_or(|e| !matches!(e, Event::Feed { .. })));
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let sessions = a.live(&mut [], "the live feed again", STEP, both);
    assert_eq!(
        sessions.len(),
        2,
        "the duplicate launched nothing: {sessions:?}"
    );

    // -- a device that stops acknowledging is closed STALLED; the other keeps working --
    let mut b = Device::connect(&relay, &slow, "Slow");
    b.ack = false;
    // Its own key space: the phone's seq is unknown to it.
    match b.call(&mut [&mut a], None, Op::Outcome { seq: dropped }) {
        ReplyResult::Ok(Resp::Outcome {
            outcome: OutcomeView::Unknown {},
        }) => {}
        other => panic!("another device's seq: {other:?}"),
    }
    // View-only: Control is refused.
    match b.call(
        &mut [&mut a],
        Some(b.welcome.next_seq),
        Op::Interrupt { live: live.clone() },
    ) {
        ReplyResult::Err(ApiError::NotAllowed {
            need: api::Class::Control,
        }) => {}
        other => panic!("control without the permission: {other:?}"),
    }
    let end = Instant::now() + STALL + STEP;
    while b.closed.is_none() {
        assert!(Instant::now() < end, "the silent device was not closed");
        b.poll(SLICE);
        a.poll(Duration::from_millis(1));
    }
    assert_eq!(b.closed, Some(close::STALLED));
    assert!(a.closed.is_none(), "the other device was closed too");
    match a.call(&mut [], None, Op::Ping {}) {
        ReplyResult::Ok(Resp::Pong {}) => {}
        other => panic!("ping: {other:?}"),
    }
    drop(a);
    drop(b);

    // The owner served all of it without a GUI.
    assert_eq!(
        sb.owner_pid(),
        Some(owner),
        "the Always-on owner kept running"
    );

    // -- a local client is refused the outcome of a device's request key --
    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    match client
        .outcome(RequestKey {
            client: phone.id.clone(),
            seq: dropped,
        })
        .wait(STEP)
    {
        Some(Err(GatewayError::Refused(_))) => {}
        other => panic!("outcome of a device's key: {other:?}"),
    }
    // -- nor may a local client claim a device's client id --
    let impostor = {
        let mut opts = GatewayOptions::new(sb.profile.clone(), Role::Gui, Arc::new(|| {}));
        opts.gateway_exe = Some(PathBuf::from(GATEWAY));
        opts.client_id = phone.id.clone();
        GatewayClient::start(opts)
    };
    wait_for(
        "the impostor's refusal",
        Duration::from_secs(30),
        || match impostor.link() {
            Link::Refused(Refusal::NotAllowed(_)) => Some(()),
            Link::Up(_) => panic!("a local client was admitted as {}", phone.id),
            _ => None,
        },
    );
    drop(impostor);

    // -- under the maintenance barrier a device gets no new terminal view --
    match client
        .maintenance(MaintenanceOp::Begin(MaintenancePurpose::Update))
        .wait(STEP)
    {
        Some(Ok(_)) => {}
        other => panic!("maintenance begin: {other:?}"),
    }
    let mut a = Device::connect(&relay, &phone, "Phone");
    match a.call(
        &mut [],
        None,
        Op::Attach {
            live: second.clone(),
            mode: ViewMode::Observe,
        },
    ) {
        ReplyResult::Err(ApiError::Maintenance {}) => {}
        other => panic!("attach under maintenance: {other:?}"),
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
