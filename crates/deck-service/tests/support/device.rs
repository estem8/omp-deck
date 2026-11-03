//! The device-protocol harness of the live gateway tests: a private HOME/XDG sandbox with its
//! own `omp-deck-test-*` namespace, an in-process relay, pairing through the whole R3 flow and a
//! device speaking `deck_remote::api` over Noise.
//!
//! Each test binary uses part of it.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_remote::api::{
    self, Event, FeedValue, LiveSession, Op, OutcomeView, Record, ReplyResult, Resp,
};
use deck_remote::noise::{DeviceHello, Initiator, Keypair, Mode, PairingInvite, Session};
use deck_remote::relay::{self, DEVICE_PATH, HELLO_OK, Hello, RoomKey, close};
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions};
use deck_service::gateway::protocol::{
    Done, MobileView, Mutation, Outcome, PairingState, Permissions, Role,
};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const GATEWAY: &str = env!("CARGO_BIN_EXE_omp-deck-gateway");
pub const STEP: Duration = Duration::from_secs(20);
/// A device that holds messages unacknowledged this long is closed (`OMP_DECK_DEVICE_STALL_MS`).
pub const STALL: Duration = Duration::from_secs(4);
/// How long one device read waits before the other devices are serviced.
pub const SLICE: Duration = Duration::from_millis(40);

// ---- sandbox ------------------------------------------------------------------------------------

/// omp's `config.yml` without a first-run wizard or collab: omp starts straight into its editor.
pub const OMP_CONFIG: &str = "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\n";

pub struct Sandbox {
    pub root: PathBuf,
    pub ns: String,
    pub profile: Profile,
}

impl Sandbox {
    /// `tag` names the namespace (`omp-deck-test-<tag>-<pid>`); `omp_config` is omp's
    /// `config.yml` (see [`OMP_CONFIG`]).
    pub fn new(tag: &str, omp_config: &str) -> Sandbox {
        // Short: the ctl socket path (`$XDG_RUNTIME_DIR/omp-deck/ctl/<ns>/<leaf>/ctl.sock`, both
        // under this root) must stay well inside `sun_path`, or omp runs without live control.
        let ns = format!("omp-deck-test-{tag}-{}", std::process::id());
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
        // SAFETY: the only test of its binary; set before any thread exists.
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
        std::fs::create_dir_all(&agent).expect("agent dir");
        std::fs::write(agent.join("config.yml"), omp_config).expect("omp config");
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

    /// omp's agent directory (`PI_CODING_AGENT_DIR`).
    pub fn agent(&self) -> PathBuf {
        self.root.join("home/.omp/agent")
    }

    pub fn proj(&self) -> PathBuf {
        // Under HOME: device views show it `~`-abbreviated.
        self.root.join("home/proj")
    }

    /// A session of the sandbox project in the store, so the project has an id.
    pub fn seed_session(&self, id: &str) {
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

    pub fn gateway<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Output {
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

    pub fn owner_pid(&self) -> Option<u32> {
        let out = self.gateway(&["status", "--json"]);
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v["status"]["pid"].as_u64().map(|p| p as u32)
    }

    /// Any failure prints the owner's log below the panic message (readable from a CI log).
    pub fn report_failures(&self) {
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

    pub fn client(&self) -> GatewayClient {
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

pub fn kill_process(pid: u32) {
    let _ = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
    } else {
        Command::new("kill").args(["-9", &pid.to_string()]).output()
    };
}

pub fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ---- relay --------------------------------------------------------------------------------------

pub struct Relay {
    pub port: u16,
    pub key: RoomKey,
    pub stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub thread: Option<std::thread::JoinHandle<()>>,
    config: deck_relay::Config,
}

impl Relay {
    pub fn start(dir: PathBuf) -> Relay {
        Relay::start_with(deck_relay::Config::new(dir), 0)
    }

    /// The relay also serving the phone app from `web_dir` (`--web-dir`), on `port` (0: any).
    pub fn start_serving(dir: PathBuf, web_dir: PathBuf, port: u16) -> Relay {
        let mut config = deck_relay::Config::new(dir);
        config.web_dir = Some(web_dir);
        Relay::start_with(config, port)
    }

    fn start_with(config: deck_relay::Config, port: u16) -> Relay {
        let (key, _) = deck_relay::rooms::enroll(&config.state_dir, "test").expect("enroll a room");
        let mut relay = Relay {
            port,
            key,
            stop: None,
            thread: None,
            config,
        };
        relay.serve();
        relay
    }

    /// Serves on `self.port` (0: any, then remembered); the enrolled room stays in the state dir.
    fn serve(&mut self) {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", self.port)).expect("bind the relay");
        self.port = listener.local_addr().expect("relay address").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let config = self.config.clone();
        self.thread = Some(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("relay runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                deck_relay::serve(config, listener, async move {
                    let _ = stopped.await;
                })
                .await
                .expect("relay serve");
            });
        }));
        self.stop = Some(stop);
    }

    /// Stops the relay (every connection closes) and serves again on the same port and room,
    /// like a relay restart or upgrade.
    pub fn restart(&mut self) {
        self.shut();
        self.serve();
    }

    fn shut(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.shut();
    }
}

// ---- websocket ----------------------------------------------------------------------------------

pub type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

pub fn set_timeout(ws: &Ws, t: Duration) {
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(t)).expect("read timeout");
    }
}

/// One binary message, the close code, or `None` when nothing came within the read timeout.
pub fn read_once(ws: &mut Ws) -> Option<Result<Vec<u8>, u16>> {
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

pub fn read_blocking(ws: &mut Ws) -> Result<Vec<u8>, u16> {
    set_timeout(ws, STEP);
    read_once(ws).expect("an answer in time")
}

/// A device connection admitted with `token` (retried while the token is on its way).
pub fn admit(relay: &Relay, token: &relay::Token) -> Ws {
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

/// `s4`: the device announces slice 4 ([`api::FEATURE_S4`]).
pub fn hello(name: &str, s4: bool) -> DeviceHello {
    DeviceHello {
        name: name.into(),
        platform: "test".into(),
        ts: now_ms(),
        api: 1,
        features: if s4 {
            vec![api::FEATURE_S4.to_owned()]
        } else {
            Vec::new()
        },
    }
}

// ---- gateway client -----------------------------------------------------------------------------

pub fn run(client: &GatewayClient, op: Mutation) -> Result<Done, String> {
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

pub fn wait_mobile(
    client: &GatewayClient,
    what: &str,
    f: impl Fn(&MobileView) -> bool,
) -> Arc<MobileView> {
    wait_for(what, STEP, || client.state().mobile.get().filter(|m| f(m)))
}

/// A paired device: its key, registry id and relay token.
pub struct Paired {
    pub key: Keypair,
    pub gateway: deck_remote::noise::PublicKey,
    pub id: String,
    pub token: relay::Token,
}

/// Pairs a device named `name` with `permissions` through the whole R3 flow.
pub fn pair(client: &GatewayClient, relay: &Relay, name: &str, permissions: Permissions) -> Paired {
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
        &hello(name, true),
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
pub struct Device {
    pub ws: Ws,
    pub noise: Session,
    /// Gateway messages processed (the `ACK` count).
    pub received: u64,
    /// Acknowledge every message (a stalling device does not).
    pub ack: bool,
    pub records: VecDeque<Record>,
    pub closed: Option<u16>,
    pub next_id: u64,
    pub welcome: api::Welcome,
}

impl Device {
    /// An IK reconnect of `p`; its first record must be `WELCOME`.
    pub fn connect(relay: &Relay, p: &Paired, name: &str, s4: bool) -> Device {
        let mut ws = admit(relay, &p.token);
        let (init, msg1) = Initiator::start(Mode::Reconnect, &p.key, &p.gateway, &hello(name, s4))
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

    pub fn send(&mut self, records: &[Record]) {
        let msg = api::encode(records).expect("encode records");
        for frame in self.noise.seal(&msg).expect("seal") {
            self.ws.send(Message::binary(frame)).expect("send frame");
        }
    }

    /// Reads for up to `wait`: one message's records are queued (and acknowledged).
    pub fn poll(&mut self, wait: Duration) {
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
    pub fn request(&mut self, seq: Option<u64>, op: Op) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::to_vec(&api::Request { id, seq, op }).expect("request json");
        self.send(&[Record::Request(body)]);
        id
    }

    /// The first queued record `f` maps to `Some` (removed), reading while needed; `others`
    /// keep being serviced meanwhile.
    pub fn take<T>(
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

    pub fn reply(&mut self, others: &mut [&mut Device], id: u64) -> ReplyResult {
        self.take(others, &format!("reply {id}"), STEP, |r| match r {
            Record::Reply(b) => {
                let reply: api::Reply = serde_json::from_slice(b).expect("reply json");
                (reply.id == id).then_some(reply.result)
            }
            _ => None,
        })
    }

    pub fn call(&mut self, others: &mut [&mut Device], seq: Option<u64>, op: Op) -> ReplyResult {
        let id = self.request(seq, op);
        self.reply(others, id)
    }

    /// The final outcome event of `seq`.
    pub fn outcome(
        &mut self,
        others: &mut [&mut Device],
        seq: u64,
        timeout: Duration,
    ) -> OutcomeView {
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
    pub fn live(
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

pub fn event(r: &Record) -> Option<Event> {
    match r {
        Record::Event(b) => Some(serde_json::from_slice(b).expect("event json")),
        _ => None,
    }
}

pub fn accepted(r: ReplyResult) -> bool {
    match r {
        ReplyResult::Ok(Resp::Accepted { duplicate, .. }) => duplicate,
        other => panic!("not accepted: {other:?}"),
    }
}
