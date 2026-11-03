//! Mobile access end to end, no GUI and no phone: a real gateway owner (`omp-deck-gateway`)
//! links to an in-test relay (`deck_relay::serve` on loopback) and a scripted device speaks the
//! relay's device protocol and the Noise handshakes over a real WebSocket.
//!
//! Covers pairing (approval fingerprint, welcome token), the IK reconnect, replay refusal, an
//! owner restart keeping devices and `last_ts`, deny, cancel, expiry, revocation of a live
//! device, a wrong PSK never reaching approval, a second claimant on one invite, and that no
//! secret reaches the owner log or the `Mobile` topic.
//!
//! Everything runs in a private HOME/XDG tree under a unique `omp-deck-test-*` namespace; the
//! real omp store and the real `omp-deck` tmux server are never touched.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use deck_core::config::GatewayMode;
use deck_remote::noise::{
    DeviceHello, Initiator, Keypair, Mode, PairingInvite, Welcome, fingerprint,
};
use deck_remote::relay::{self, DEVICE_PATH, HELLO_OK, Hello, RoomKey, close};
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Done, MobileView, Mutation, Outcome, PairingState, Permissions, RelayLink, Role, SecretText,
};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const GATEWAY: &str = env!("CARGO_BIN_EXE_omp-deck-gateway");
/// The pairing window of this test's owner (`OMP_DECK_PAIRING_TTL_MS`).
const TTL: Duration = Duration::from_secs(6);
const STEP: Duration = Duration::from_secs(20);

// ---- failure diagnostics ------------------------------------------------------------------------

/// The owner's state dir (its `gateway.log`), for the failure report.
static OWNER_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
/// The in-test relay's log lines (`deck_relay` targets, debug and up), newest last.
static RELAY_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static LOG_START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
const RELAY_LOG_LINES: usize = 2_000;

struct RelayLog;

impl log::Log for RelayLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("deck_relay") && m.level() <= log::Level::Debug
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let at = LOG_START.elapsed();
        let mut lines = RELAY_LOG.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= RELAY_LOG_LINES {
            lines.remove(0);
        }
        lines.push(format!("{at:>10.3?} {:<5} {}", r.level(), r.args()));
    }

    fn flush(&self) {}
}

/// Any failure (an admission refused, a wait timed out …) prints the owner log and the relay's
/// log below the panic message, so a CI-only failure can be read from the job log.
fn report_failures(owner_dir: PathBuf) {
    *OWNER_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(owner_dir);
    std::sync::LazyLock::force(&LOG_START);
    if log::set_logger(&RelayLog).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let owner = OWNER_DIR
            .lock()
            .ok()
            .and_then(|d| d.clone())
            .map(|dir| logs_in(&dir))
            .unwrap_or_default();
        let relay = RELAY_LOG.lock().map(|l| l.join("\n")).unwrap_or_default();
        eprintln!(
            "==== owner gateway.log ====\n{owner}\n==== relay log ====\n{relay}\n==== end ===="
        );
    }));
}

/// Every `gateway.log` under the owner's state dir.
fn logs_in(dir: &std::path::Path) -> String {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("gateway.log")).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- sandbox ------------------------------------------------------------------------------------

struct Sandbox {
    root: PathBuf,
    ns: String,
    profile: Profile,
}

impl Sandbox {
    fn new() -> Sandbox {
        let ns = format!("omp-deck-test-remote-{}", std::process::id());
        let root = std::env::temp_dir().join(&ns);
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            "home",
            "home/AppData/Roaming",
            "home/AppData/Local",
            "config",
            "data",
            "state",
            "run",
            "relay",
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
            // The autostarted owner inherits a short pairing window.
            std::env::set_var("OMP_DECK_PAIRING_TTL_MS", TTL.as_millis().to_string());
            // Debug logs: the secrets check covers the chattiest level.
            std::env::set_var("OMP_DECK_GATEWAY_LOG", "trace");
        }
        std::fs::create_dir_all(&agent).expect("agent dir");
        let profile = Profile {
            host_namespace: ns.clone(),
            settings: root.join("config/omp-deck/state.json"),
            sessions_root: agent.join("sessions"),
        };
        Sandbox { root, ns, profile }
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

    fn owner_dir(&self) -> PathBuf {
        self.profile.settings.with_file_name("gateway")
    }

    fn owner_log(&self) -> String {
        logs_in(&self.owner_dir())
    }

    /// The owner's last relay hello epoch (`relay-epoch` in its state dir).
    fn relay_epoch(&self) -> Option<u64> {
        let dir = self.profile.settings.with_file_name("gateway");
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("relay-epoch")).ok())
            .find_map(|t| t.trim().parse().ok())
    }

    fn client(&self) -> GatewayClient {
        let mut opts = GatewayOptions::new(self.profile.clone(), Role::Gui, Arc::new(|| {}));
        opts.gateway_exe = Some(PathBuf::from(GATEWAY));
        GatewayClient::start(opts)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(pid) = self.owner_pid() {
            kill_process(pid, cfg!(windows));
        }
        // Host-backend sessions outlive the owner (and their sockets go with the root): end every
        // session of this test's namespace.
        if let Ok(ns) = deck_host::Namespace::new(&self.ns) {
            for name in ns.list() {
                let _ = ns.kill(&name);
            }
        }
        // Our own test namespace only (never the default tmux server).
        if !cfg!(windows) {
            let _ = Command::new("tmux")
                .args(["-L", &self.ns, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn kill_process(pid: u32, force: bool) {
    let _ = if cfg!(windows) {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string()]);
        if force {
            c.arg("/F");
        }
        c.output()
    } else {
        let mut c = Command::new("kill");
        if force {
            c.arg("-9");
        }
        c.arg(pid.to_string()).output()
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

// ---- device -------------------------------------------------------------------------------------

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// What the relay or the gateway answered.
#[derive(Debug)]
enum Reply {
    Data(Vec<u8>),
    Closed(u16),
}

fn read(ws: &mut Ws) -> Reply {
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => return Reply::Data(b.to_vec()),
            Ok(Message::Close(frame)) => {
                return Reply::Closed(frame.map_or(1005, |f| u16::from(f.code)));
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Text(t)) => panic!("text from the relay: {t}"),
            Err(e) => panic!("device read: {e}"),
        }
    }
}

/// A device connection admitted with `token`, or the code it was refused with.
fn admit(relay: &Relay, token: &relay::Token) -> Result<Ws, u16> {
    let (mut ws, _) =
        tungstenite::connect(format!("{}{DEVICE_PATH}", relay.url())).expect("device websocket");
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(STEP)).expect("read timeout");
    }
    ws.send(Message::binary(
        Hello::Device {
            room: relay.key.room,
            token: *token,
        }
        .encode(),
    ))
    .expect("device hello");
    match read(&mut ws) {
        Reply::Data(b) if b[..] == HELLO_OK[..] => Ok(ws),
        Reply::Data(b) => panic!("unexpected hello answer {b:?}"),
        Reply::Closed(code) => Err(code),
    }
}

/// [`admit`], retried while the relay has no gateway (an owner restarting).
fn admit_when_online(relay: &Relay, token: &relay::Token) -> Result<Ws, u16> {
    admit_retrying(relay, token, close::GATEWAY_OFFLINE)
}

/// [`admit`] of a just-issued token: `Done` can overtake the token set on its way to the relay
/// (the relay does not acknowledge it), so a refusal is retried briefly.
fn admit_fresh(relay: &Relay, token: &relay::Token) -> Ws {
    admit_retrying(relay, token, close::UNAUTHORIZED).expect("a fresh token is admitted")
}

fn admit_retrying(relay: &Relay, token: &relay::Token, transient: u16) -> Result<Ws, u16> {
    let end = Instant::now() + STEP;
    loop {
        match admit(relay, token) {
            Err(code) if code == transient && Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// Sends message 1 and returns the answer.
fn handshake(ws: &mut Ws, msg1: &[u8]) -> Reply {
    ws.send(Message::binary(msg1.to_vec())).expect("send msg1");
    read(ws)
}

fn hello(name: &str, ts: u64) -> DeviceHello {
    DeviceHello {
        name: name.into(),
        platform: "test".into(),
        ts,
        api: 1,
        features: Vec::new(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn assert_closed(r: Result<Ws, u16>, code: u16, what: &str) {
    match r {
        Err(c) => assert_eq!(c, code, "{what}"),
        Ok(mut ws) => match read(&mut ws) {
            Reply::Closed(c) => assert_eq!(c, code, "{what}"),
            other => panic!("{what}: {other:?}"),
        },
    }
}

// ---- gateway ------------------------------------------------------------------------------------

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

fn mobile(client: &GatewayClient) -> Option<Arc<MobileView>> {
    client.state().mobile.get()
}

fn wait_mobile(
    client: &GatewayClient,
    what: &str,
    f: impl Fn(&MobileView) -> bool,
) -> Arc<MobileView> {
    wait_for(what, STEP, || mobile(client).filter(|m| f(m)))
}

struct Started {
    pairing: String,
    invite: PairingInvite,
    secrets: Vec<String>,
}

fn begin(client: &GatewayClient) -> Started {
    match run(client, Mutation::PairingBegin).expect("pairing begins") {
        Done::PairingStarted {
            pairing,
            invite,
            link,
            expires_unix_ms,
        } => {
            assert!(expires_unix_ms > now_ms(), "expires in the future");
            assert!(
                link.expose()
                    .ends_with(&format!("#pair={}", invite.expose()))
            );
            let parsed = PairingInvite::parse(invite.expose()).expect("invite parses");
            Started {
                pairing,
                secrets: vec![
                    invite.expose().to_owned(),
                    URL_SAFE_NO_PAD.encode(parsed.token),
                    URL_SAFE_NO_PAD.encode(parsed.psk.as_ref()),
                    hex(&parsed.token),
                    hex(parsed.psk.as_ref()),
                ],
                invite: parsed,
            }
        }
        other => panic!("pairing-begin result: {other:?}"),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A device that scanned `s`: admitted and its pairing message 1 answered with `Approval`.
fn claim(
    client: &GatewayClient,
    relay: &Relay,
    s: &Started,
    kp: &Keypair,
    name: &str,
) -> (Ws, Initiator) {
    let mut ws = admit_fresh(relay, &s.invite.token);
    let (init, msg1) = Initiator::start(
        Mode::Pairing { psk: &s.invite.psk },
        kp,
        &s.invite.gateway_public,
        &hello(name, now_ms()),
    )
    .expect("pairing msg1");
    ws.send(Message::binary(msg1)).expect("send msg1");
    let want = fingerprint(kp.public());
    wait_mobile(client, "the approval request", |m| {
        matches!(&m.pairing, Some(p) if p.pairing == s.pairing
            && matches!(&p.state, PairingState::Approval { fingerprint, name: n, .. }
                if *fingerprint == want && n == name))
    });
    (ws, init)
}

// ---- the test -----------------------------------------------------------------------------------

#[test]
fn pairing_reconnect_and_revocation_over_a_real_relay() {
    let sb = Sandbox::new();
    report_failures(sb.owner_dir());
    let relay = Relay::start(sb.root.join("relay"));
    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    let mut secrets = vec![
        relay.key.encode(),
        URL_SAFE_NO_PAD.encode(relay.key.secret),
        hex(&relay.key.secret),
    ];

    // Mobile access needs the gateway on; pairing needs the relay connected.
    let refused = run(&client, Mutation::PairingBegin).expect_err("mobile access is off");
    assert!(refused.contains("Mobile access is off"), "{refused}");
    client
        .settings()
        .set_gateway_mode(GatewayMode::WhileOpen)
        .wait(STEP)
        .expect("mode answered")
        .expect("mode set");
    let bad = run(
        &client,
        Mutation::RelaySet {
            url: format!("ws://relay.example:{}", relay.port),
            room_key: SecretText::new(relay.key.encode()),
        },
    )
    .expect_err("plain ws is loopback-only");
    assert!(bad.contains("wss"), "{bad}");
    run(
        &client,
        Mutation::RelaySet {
            url: relay.url(),
            room_key: SecretText::new(relay.key.encode()),
        },
    )
    .expect("relay set");
    let view = wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
    let relay_view = view.relay.clone().expect("relay shown");
    assert_eq!(relay_view.url, relay.url());
    assert_eq!(relay_view.room_id, relay::room_id(&relay.key.room));
    let gateway_fp = view.fingerprint.clone();
    assert!(!gateway_fp.is_empty());

    // -- a wrong PSK never reaches approval and does not use the window up --
    let s = begin(&client);
    secrets.extend(s.secrets.iter().cloned());
    assert_eq!(s.invite.url, relay.url());
    assert_eq!(fingerprint(&s.invite.gateway_public), gateway_fp);
    {
        let mut ws = admit_fresh(&relay, &s.invite.token);
        let wrong = zeroize::Zeroizing::new([7u8; 32]);
        let (_, msg1) = Initiator::start(
            Mode::Pairing { psk: &wrong },
            &Keypair::generate().expect("key"),
            &s.invite.gateway_public,
            &hello("Intruder", now_ms()),
        )
        .expect("msg1");
        match handshake(&mut ws, &msg1) {
            Reply::Closed(c) => assert_eq!(c, close::UNAUTHORIZED, "wrong psk"),
            other => panic!("wrong psk answered: {other:?}"),
        }
        let m = mobile(&client).expect("view");
        let p = m.pairing.as_ref().expect("window still open");
        assert_eq!(p.state, PairingState::Waiting);
    }

    // -- pairing: approval shows the device fingerprint; message 2 carries a token --
    let phone = Keypair::generate().expect("device key");
    let (mut ws, init) = claim(&client, &relay, &s, &phone, "Test phone");
    // A second scanner of the same invite is turned away while the first is held.
    wait_for("a second claimant refused", STEP, || {
        match admit(&relay, &s.invite.token) {
            Err(c) => Some(c),
            Ok(mut other) => match read(&mut other) {
                Reply::Closed(c) => Some(c),
                Reply::Data(d) => panic!("second claimant got data {d:?}"),
            },
        }
    });
    let refused = run(
        &client,
        Mutation::PairingApprove {
            pairing: "nope".into(),
            permissions: Permissions::default(),
        },
    )
    .expect_err("wrong window id");
    assert!(refused.contains("ended"), "{refused}");
    run(
        &client,
        Mutation::PairingApprove {
            pairing: s.pairing.clone(),
            permissions: Permissions::default(),
        },
    )
    .expect("approved");
    let (_, welcome): (_, Welcome) = match read(&mut ws) {
        Reply::Data(msg2) => init.finish(&msg2).expect("pairing msg2"),
        other => panic!("no message 2: {other:?}"),
    };
    let token = welcome.token.expect("pairing welcome carries the token");
    secrets.push(URL_SAFE_NO_PAD.encode(token));
    secrets.push(hex(&token));
    assert!(
        welcome.gateway.starts_with("OmpDeck"),
        "{}",
        welcome.gateway
    );
    let device_id = welcome.device.clone();
    let view = wait_mobile(&client, "the paired device", |m| m.devices.len() == 1);
    let d = &view.devices[0];
    assert_eq!(d.id, device_id);
    assert_eq!(d.name, "Test phone");
    assert_eq!(d.platform, "test");
    assert_eq!(d.fingerprint, fingerprint(phone.public()));
    assert_eq!(d.permissions, Permissions::default());
    // The pairing connection ends: the window closes and its token is withdrawn.
    let _ = ws.close(None);
    drop(ws);
    wait_mobile(&client, "the window to close", |m| m.pairing.is_none());
    assert_closed(
        admit(&relay, &s.invite.token),
        close::UNAUTHORIZED,
        "used invite",
    );

    // -- IK reconnect with the device token; a replayed message 1 is refused --
    let mut ts = now_ms() + 1;
    let (init, msg1) = Initiator::start(
        Mode::Reconnect,
        &phone,
        &s.invite.gateway_public,
        &hello("Test phone", ts),
    )
    .expect("reconnect msg1");
    let mut ws = admit(&relay, &token).expect("device token admitted");
    match handshake(&mut ws, &msg1) {
        Reply::Data(msg2) => {
            let (_, w) = init.finish(&msg2).expect("reconnect msg2");
            assert_eq!(w.device, device_id);
            assert!(w.token.is_none(), "a reconnect carries no token");
        }
        other => panic!("reconnect: {other:?}"),
    }
    wait_mobile(&client, "the device online", |m| {
        m.devices.first().is_some_and(|d| d.online)
    });
    let _ = ws.close(None);
    drop(ws);
    wait_mobile(&client, "the device offline", |m| {
        m.devices.first().is_some_and(|d| !d.online)
    });
    let mut ws = admit(&relay, &token).expect("device token admitted");
    assert!(
        matches!(
            handshake(&mut ws, &msg1),
            Reply::Closed(close::UNAUTHORIZED)
        ),
        "replayed msg1"
    );

    // Rename and permissions are stored.
    run(
        &client,
        Mutation::DeviceRename {
            id: device_id.clone(),
            name: "  Pocket  ".into(),
        },
    )
    .expect("renamed");
    let perms = Permissions {
        control: false,
        manage: true,
    };
    run(
        &client,
        Mutation::DeviceSetPermissions {
            id: device_id.clone(),
            permissions: perms,
        },
    )
    .expect("permissions set");
    wait_mobile(&client, "rename and permissions", |m| {
        m.devices
            .first()
            .is_some_and(|d| d.name == "Pocket" && d.permissions == perms)
    });

    // -- an owner restart keeps the device and its last timestamp --
    let pid = sb.owner_pid().expect("owner pid");
    let epoch_before = sb.relay_epoch().expect("the hello epoch is persisted");
    // Graceful on Unix (SIGTERM); forced on Windows, where `taskkill` without /F only posts
    // WM_CLOSE and the windowless gateway never receives it.
    kill_process(pid, cfg!(windows));
    wait_for("a new owner", Duration::from_secs(60), || {
        sb.owner_pid().filter(|p| *p != pid)
    });
    let mut ws = admit_when_online(&relay, &token).expect("device token admitted after restart");
    assert!(
        matches!(
            handshake(&mut ws, &msg1),
            Reply::Closed(close::UNAUTHORIZED)
        ),
        "replayed msg1 after a restart"
    );
    ts += 1;
    let (init, msg1) = Initiator::start(
        Mode::Reconnect,
        &phone,
        &s.invite.gateway_public,
        &hello("Test phone", ts),
    )
    .expect("reconnect msg1");
    let mut live = admit_when_online(&relay, &token).expect("device token admitted");
    match handshake(&mut live, &msg1) {
        Reply::Data(msg2) => {
            init.finish(&msg2).expect("reconnect after restart");
        }
        other => panic!("reconnect after restart: {other:?}"),
    }
    let view = wait_mobile(&client, "the device online after restart", |m| {
        m.link == RelayLink::Up && m.devices.first().is_some_and(|d| d.online)
    });
    assert_eq!(view.devices.len(), 1);
    assert_eq!(view.devices[0].name, "Pocket");
    let epoch_after = sb.relay_epoch().expect("the hello epoch is persisted");
    assert!(
        epoch_after > epoch_before,
        "epochs rise across restarts: {epoch_before} → {epoch_after}"
    );

    // -- revocation closes the live device; its token is refused afterwards --
    run(
        &client,
        Mutation::DeviceRevoke {
            id: device_id.clone(),
        },
    )
    .expect("revoked");
    // A reconnect session carries application records (its `WELCOME` first) before the close.
    loop {
        match read(&mut live) {
            Reply::Data(_) => {}
            Reply::Closed(c) => {
                assert_eq!(c, close::UNAUTHORIZED, "revoked live device");
                break;
            }
        }
    }
    wait_mobile(&client, "no devices", |m| m.devices.is_empty());
    assert_closed(admit(&relay, &token), close::UNAUTHORIZED, "revoked token");

    // -- deny --
    let s = begin(&client);
    secrets.extend(s.secrets.iter().cloned());
    let other = Keypair::generate().expect("key");
    let (mut ws, _) = claim(&client, &relay, &s, &other, "Denied phone");
    run(
        &client,
        Mutation::PairingDeny {
            pairing: s.pairing.clone(),
        },
    )
    .expect("denied");
    match read(&mut ws) {
        Reply::Closed(c) => assert_eq!(c, close::UNAUTHORIZED, "denied device"),
        other => panic!("denied device got {other:?}"),
    }
    let view = wait_mobile(&client, "the denied window closed", |m| m.pairing.is_none());
    assert!(view.devices.is_empty());
    assert_closed(
        admit(&relay, &s.invite.token),
        close::UNAUTHORIZED,
        "denied invite",
    );

    // -- a verified candidate that leaves ends the window: the approval reaches nobody --
    let s = begin(&client);
    secrets.extend(s.secrets.iter().cloned());
    let leaver = Keypair::generate().expect("key");
    let (ws, _) = claim(&client, &relay, &s, &leaver, "Leaving phone");
    drop(ws);
    wait_mobile(&client, "the abandoned window closed", |m| {
        m.pairing.is_none()
    });
    let ended = run(
        &client,
        Mutation::PairingApprove {
            pairing: s.pairing.clone(),
            permissions: Permissions::default(),
        },
    )
    .expect_err("approval after the candidate left");
    assert!(ended.contains("ended"), "{ended}");
    let ended = run(
        &client,
        Mutation::PairingDeny {
            pairing: s.pairing.clone(),
        },
    )
    .expect_err("deny after the candidate left");
    assert!(ended.contains("ended"), "{ended}");
    assert_closed(
        admit(&relay, &s.invite.token),
        close::UNAUTHORIZED,
        "abandoned invite",
    );
    assert!(mobile(&client).expect("view").devices.is_empty());

    // -- revoking right after approval closes the still-open pairing connection --
    let s = begin(&client);
    secrets.extend(s.secrets.iter().cloned());
    let quick = Keypair::generate().expect("key");
    let (mut ws, init) = claim(&client, &relay, &s, &quick, "Quick phone");
    run(
        &client,
        Mutation::PairingApprove {
            pairing: s.pairing.clone(),
            permissions: Permissions::default(),
        },
    )
    .expect("approved");
    let welcome = match read(&mut ws) {
        Reply::Data(msg2) => init.finish(&msg2).expect("pairing msg2").1,
        other => panic!("no message 2: {other:?}"),
    };
    if let Some(t) = welcome.token {
        secrets.push(URL_SAFE_NO_PAD.encode(t));
        secrets.push(hex(&t));
    }
    // The pairing connection is the device's session: it shows online.
    wait_mobile(&client, "the new device online", |m| {
        m.devices.iter().any(|d| d.id == welcome.device && d.online)
    });
    run(
        &client,
        Mutation::DeviceRevoke {
            id: welcome.device.clone(),
        },
    )
    .expect("revoked");
    match read(&mut ws) {
        Reply::Closed(c) => assert_eq!(c, close::UNAUTHORIZED, "revoked pairing connection"),
        other => panic!("revoked pairing connection got {other:?}"),
    }
    let view = wait_mobile(&client, "the revoked window gone", |m| {
        m.pairing.is_none() && m.devices.is_empty()
    });
    assert!(view.devices.is_empty());
    assert_closed(
        admit(&relay, &s.invite.token),
        close::UNAUTHORIZED,
        "invite of a revoked device",
    );

    // -- cancel --
    let s = begin(&client);
    secrets.extend(s.secrets.iter().cloned());
    run(&client, Mutation::PairingCancel).expect("cancelled");
    wait_mobile(&client, "the cancelled window closed", |m| {
        m.pairing.is_none()
    });
    assert_closed(
        admit(&relay, &s.invite.token),
        close::UNAUTHORIZED,
        "cancelled invite",
    );

    // -- expiry --
    let s = begin(&client);
    secrets.extend(s.secrets.iter().cloned());
    std::thread::sleep(TTL);
    wait_mobile(&client, "the window to expire", |m| m.pairing.is_none());
    assert_closed(
        admit(&relay, &s.invite.token),
        close::UNAUTHORIZED,
        "expired invite",
    );

    // -- no secret in the topic or the owner log --
    let topic = serde_json::to_string(&*mobile(&client).expect("view")).expect("topic json");
    let log = sb.owner_log();
    assert!(
        log.contains("relay link up"),
        "the owner log is the one checked:\n{log}"
    );
    for secret in &secrets {
        assert!(
            !topic.contains(secret.as_str()),
            "a secret in the topic: {topic}"
        );
        assert!(!log.contains(secret.as_str()), "a secret in the owner log");
    }

    // -- an explicit reconnect restarts the link (a new hello, so a newer epoch) --
    let before = sb.relay_epoch().expect("epoch");
    run(&client, Mutation::RelayReconnect).expect("reconnect");
    wait_for("a new hello", STEP, || {
        sb.relay_epoch().filter(|e| *e > before)
    });
    wait_mobile(&client, "the link up after reconnecting", |m| {
        m.link == RelayLink::Up
    });
    // Pairing works on the new connection (the engine itself requires the link up).
    wait_for("pairing on the new link", STEP, || {
        run(&client, Mutation::PairingBegin).ok()
    });
    run(&client, Mutation::PairingCancel).expect("cancelled");

    run(&client, Mutation::RelayClear).expect("relay cleared");
    let view = wait_mobile(&client, "the relay cleared", |m| m.relay.is_none());
    assert_eq!(view.link, RelayLink::Off);
    let refused = run(&client, Mutation::RelayReconnect).expect_err("no relay to reconnect to");
    assert!(refused.contains("No relay"), "{refused}");
}
