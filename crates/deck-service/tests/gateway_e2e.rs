//! The gateway as a real process with real omp, no GUI: concurrent startup gives one owner, a
//! session is launched and controlled through the private IPC alone, a lost owner is replaced and
//! re-adopts the live backend session, and a stopped session is only that session.
//!
//! Everything runs in a private HOME/XDG tree under a unique `omp-deck-test-*` namespace (backend
//! sockets included); the real omp store and the real `omp-deck` tmux server are never touched.
//! Skipped (with a message) when omp is not installed.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_service::controller::{Command as ControlCommand, Delivery, Runtime};
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Answer, ControllerInfo, Done, GatewayError, LaunchSpec, Mutation, Outcome, Query, Role,
    SessionRole, Topic, TopicValue,
};
use deck_service::session_actions::Action;

const GATEWAY: &str = env!("CARGO_BIN_EXE_omp-deck-gateway");

struct Sandbox {
    root: PathBuf,
    ns: String,
    profile: Profile,
}

impl Sandbox {
    fn new() -> Sandbox {
        let ns = format!("omp-deck-test-gwe2e-{}", std::process::id());
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
            "proj",
        ] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dir");
        }
        // A real XDG_RUNTIME_DIR is 0700 whatever the umask; under a group-writable umask (0002)
        // `create_dir_all` yields 0775, which ctl rejects (fallback socket path exceeds sun_path).
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
            // The session host binary sits beside the gateway in the target directory.
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
        Sandbox { root, ns, profile }
    }

    /// Runs the gateway CLI against this sandbox's profile (explicit on every platform: the
    /// default settings path does not follow `XDG_CONFIG_HOME` on Windows).
    fn gateway<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Output {
        Command::new(GATEWAY)
            .args(args)
            .args(self.profile_args())
            .output()
            .expect("run omp-deck-gateway")
    }

    fn profile_args(&self) -> Vec<std::ffi::OsString> {
        vec![
            "--ns".into(),
            self.ns.clone().into(),
            "--settings".into(),
            self.profile.settings.clone().into(),
            "--sessions-root".into(),
            self.profile.sessions_root.clone().into(),
        ]
    }

    fn owner_pid(&self) -> Option<u32> {
        let out = self.gateway(&["status", "--json"]);
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v["status"]["pid"].as_u64().map(|p| p as u32)
    }

    /// The owner's `gateway.log` (for failure messages).
    fn owner_log(&self) -> String {
        let dir = self.profile.settings.with_file_name("gateway");
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("gateway.log")).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn client(&self) -> GatewayClient {
        let mut opts = GatewayOptions::new(self.profile.clone(), Role::Gui, Arc::new(|| {}));
        opts.gateway_exe = Some(PathBuf::from(GATEWAY));
        GatewayClient::start(opts)
    }

    fn proj(&self) -> PathBuf {
        self.root.join("proj")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Our own test backend only (never the default tmux server, never the real `omp-deck`
        // namespace): the owner first, then every session it left behind.
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
        if !cfg!(windows) {
            let _ = Command::new("tmux")
                .args(["-L", &self.ns, "kill-server"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Ends a process: `force` is SIGKILL / `taskkill /F` (no chance to clean up).
fn kill_process(pid: u32, force: bool) {
    let out = if cfg!(windows) {
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
    assert!(
        out.is_ok_and(|o| o.status.success()) || !force,
        "killing {pid}"
    );
}

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn up(client: &GatewayClient) {
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
}

fn controllers(client: &GatewayClient) -> Vec<ControllerInfo> {
    match client
        .query(Query::Topic(Topic::Controllers))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Controllers(c) => c,
            other => panic!("controllers value: {other:?}"),
        },
        other => panic!("controllers query: {other:?}"),
    }
}

fn screen_text(client: &GatewayClient, info: &ControllerInfo) -> String {
    match client
        .query(Query::Screen(info.target.clone()))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Screen(s))) => s.lines.join("\n"),
        other => panic!("screen query: {other:?}"),
    }
}

#[test]
fn headless_gateway_controls_real_omp_without_a_gui() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let sb = Sandbox::new();

    // Not running yet.
    assert_eq!(sb.gateway(&["status"]).status.code(), Some(3));

    // Concurrent starts: every one succeeds, exactly one owner serves. A GUI-role client is the
    // product's own path: no GUI process involved, only the IPC.
    let (client, others) = if cfg!(windows) {
        // `serve` runs in the calling console on Windows (the launchers detach it), so the
        // concurrent starters are clients, each launching the owner detached when none answers.
        let mut all: Vec<_> = (0..6).map(|_| sb.client()).collect();
        for c in &all {
            up(c);
        }
        let epoch = all[0].epoch().expect("epoch");
        assert!(
            all.iter().all(|c| c.epoch() == Some(epoch)),
            "one owner serves every starter"
        );
        let first = all.pop().expect("a client");
        (first, all)
    } else {
        let starts: Vec<_> = (0..6)
            .map(|_| {
                let args = [
                    vec!["serve".into()],
                    sb.profile_args()
                        .into_iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect::<Vec<String>>(),
                ]
                .concat();
                std::thread::spawn(move || {
                    Command::new(GATEWAY).args(args).output().expect("serve")
                })
            })
            .collect();
        for s in starts {
            let out = s.join().expect("serve thread");
            assert!(out.status.success(), "serve: {out:?}");
        }
        let owner = wait_for("the owner", Duration::from_secs(15), || sb.owner_pid());
        let already = sb.gateway(&["serve"]);
        assert!(already.status.success());
        assert!(
            String::from_utf8_lossy(&already.stdout).contains(&format!("pid={owner}")),
            "a later start must report the one owner: {already:?}"
        );
        (sb.client(), Vec::new())
    };
    up(&client);
    let first_owner = wait_for("the owner", Duration::from_secs(15), || sb.owner_pid());
    let epoch1 = client.epoch().expect("epoch");

    // Launch a real omp session in a sandbox project.
    let launch = client.mutate(Mutation::Launch(LaunchSpec {
        mux_name: Some("omp-new-gwe2e".into()),
        cwd: sb.proj(),
        session_path: None,
        label: "e2e".into(),
        argv: vec!["omp".into()],
        role: SessionRole::Session,
        copy_of: None,
        view: Default::default(),
    }));
    launch
        .accepted
        .wait(Duration::from_secs(20))
        .expect("accepted in time")
        .expect("launch accepted");
    let target = match launch.outcome.wait(Duration::from_secs(60)) {
        Some(Ok(Outcome::Done(d))) => match *d {
            Done::Session(t, view) => {
                assert_eq!(view, deck_core::config::SessionView::Terminal);
                t
            }
            other => panic!("launch result: {other:?}"),
        },
        other => panic!("launch outcome: {other:?}"),
    };
    assert_eq!(target.epoch, epoch1);

    // omp comes up idle with live control.
    let idle = |c: &ControllerInfo| {
        c.target == target && c.live_control && !c.busy && c.exit_code.is_none()
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    let info = loop {
        let all = controllers(&client);
        if let Some(c) = all.iter().find(|c| idle(c)) {
            break c.clone();
        }
        if Instant::now() > deadline {
            let screens: Vec<String> = all.iter().map(|c| screen_text(&client, c)).collect();
            panic!(
                "no idle live-control session: {all:#?}\nscreens: {screens:#?}\nowner log:\n{}",
                sb.owner_log()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    wait_for("omp's composer on screen", Duration::from_secs(60), || {
        let text = screen_text(&client, &info);
        (!text.trim().is_empty()).then_some(())
    });

    // Real command: /rename, through the controller's injection (delivery and runtime verdicts).
    let title = "gw-e2e-title";
    let rename = client.submit(
        target.clone(),
        ControlCommand::Action(Action::Rename(title.into())),
    );
    rename
        .accepted
        .wait(Duration::from_secs(10))
        .expect("accepted")
        .expect("rename accepted");
    match rename.outcome.wait(Duration::from_secs(60)) {
        Some(Ok(Outcome::Done(d))) => match *d {
            Done::Submitted(rec) => {
                assert!(
                    rec.delivery == Delivery::Delivered,
                    "delivery: {:?}",
                    rec.delivery
                );
                assert!(
                    rec.runtime == Runtime::Untracked,
                    "runtime: {:?}",
                    rec.runtime
                );
            }
            other => panic!("rename result: {other:?}"),
        },
        other => panic!("rename outcome: {other:?}"),
    }
    // omp applied it: the terminal title the gateway observed carries the new name.
    wait_for("the renamed title", Duration::from_secs(30), || {
        controllers(&client)
            .into_iter()
            .find(|c| c.target == target && c.title.as_deref().is_some_and(|t| t.contains(title)))
            .map(|_| ())
    });

    // Journal lookups and job cancels are limited to the client's own request keys.
    let other = sb.client();
    up(&other);
    assert_ne!(other.client_id(), client.client_id());
    let refused = |r: Option<Result<_, GatewayError>>, what: &str| match r {
        Some(Err(GatewayError::Refused(_))) => {}
        other => panic!("{what} of a foreign key: {other:?}"),
    };
    refused(
        other
            .outcome(rename.key.clone())
            .wait(Duration::from_secs(10))
            .map(|r| r.map(|_| ())),
        "outcome",
    );
    refused(
        other
            .mutate(Mutation::CancelJob(rename.key.clone()))
            .accepted
            .wait(Duration::from_secs(10))
            .map(|r| r.map(|_| ())),
        "cancel",
    );
    match client
        .outcome(rename.key.clone())
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Outcome::Done(_))) => {}
        r => panic!("own outcome lookup: {r:?}"),
    }
    drop(other);

    // The owner is lost (SIGKILL): the backend session survives and a new owner adopts it.
    drop(client);
    drop(others);
    kill_process(first_owner, true);
    wait_for("the old owner to be gone", Duration::from_secs(10), || {
        (sb.owner_pid() != Some(first_owner)).then_some(())
    });

    let client = sb.client();
    up(&client);
    let epoch2 = client.epoch().expect("epoch");
    assert_ne!(epoch1, epoch2, "a new owner is a new epoch");
    let adopted = wait_for(
        "the live session re-adopted",
        Duration::from_secs(60),
        || {
            controllers(&client)
                .into_iter()
                .find(|c| c.label == "e2e" && c.exit_code.is_none())
        },
    );
    assert_eq!(adopted.target.epoch, epoch2);
    assert!(
        screen_text(&client, &adopted).contains("omp")
            || !screen_text(&client, &adopted).trim().is_empty(),
        "the re-adopted session shows its screen"
    );

    // Stop/kill are this session only.
    let kill = client.mutate(Mutation::Kill(adopted.target.clone()));
    kill.accepted
        .wait(Duration::from_secs(10))
        .expect("accepted")
        .expect("kill accepted");
    match kill.outcome.wait(Duration::from_secs(30)) {
        Some(Ok(Outcome::Done(_))) => {}
        other => panic!("kill outcome: {other:?}"),
    }
    wait_for("the process to end", Duration::from_secs(30), || {
        let list = controllers(&client);
        (!list
            .iter()
            .any(|c| c.label == "e2e" && c.exit_code.is_none()))
        .then_some(())
    });
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
        .or_else(|| {
            // omp installs to ~/.local/bin, which a sandboxed HOME hides.
            let real = std::env::var_os("REAL_HOME")
                .map(PathBuf::from)
                .or_else(|| dirs_home().map(|h| h.join(".local/bin")))?;
            let p = real.join(program);
            p.is_file().then_some(p)
        })
}

fn dirs_home() -> Option<PathBuf> {
    // The sandbox replaced HOME; the passwd entry is not needed because PATH normally has omp.
    None
}
