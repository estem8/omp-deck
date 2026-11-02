//! End-to-end tests against real `omp-deck-host` daemons (Linux). Every test uses its own
//! namespace `omp-deck-test-<pid>-<n>` under a temporary `XDG_RUNTIME_DIR`; daemons are
//! killed afterwards.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use common::{Client, Observer, TestNs, env, params, scratch, wait_until};
use deck_host::protocol::{self as wire, Kind};
use deck_host::{Error, Grid, HostEvent, ObserverEvent, Palette, SessionSpec};

fn spec(name: &str, program: &str, args: &[&str]) -> SessionSpec {
    SessionSpec {
        name: name.into(),
        program: program.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        cwd: env().root.clone(),
        env_set: vec![
            ("HOME".into(), env().root.join("home").display().to_string()),
            ("PS1".into(), "$ ".into()),
            ("TERM".into(), "xterm-256color".into()),
        ],
        env_remove: vec!["ENV".into(), "BASH_ENV".into()],
        tsp: false,
    }
}

fn shell(name: &str) -> SessionSpec {
    spec(name, "/bin/sh", &[])
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z "))
}

#[test]
fn info_reports_the_osc_title_set_by_the_session() {
    let ns = TestNs::new();
    let c = Client::create(&ns, &shell("ttl"), 80, 24);
    assert_eq!(ns.0.info("ttl").unwrap().title, None);
    c.type_line("printf '\\033]2;my title\\twith é\\007'");
    // Poll the daemon: waiting for output text would also match the shell's echo of the typed
    // line, before printf has run.
    let mut title = None;
    wait_until("the OSC title", || {
        title = ns.0.info("ttl").unwrap().title;
        title.is_some()
    });
    // The parser drops the control character.
    assert_eq!(title.as_deref(), Some("my titlewith é"));
}

#[test]
fn input_round_trip_detach_and_reattach_with_same_screen_and_cursor() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("rt"), 80, 24);
    c.type_line("printf 'hello-%s\\n' world");
    c.wait_output("hello-world");
    c.wait_output("$ ");
    c.settle(Duration::from_millis(300));
    let cursor = c.cursor();
    let screen = c.screen();
    assert!(screen.contains("hello-world"), "{screen}");

    c.tx.detach().unwrap();
    assert!(matches!(c.next(), HostEvent::Closed));
    assert_eq!(ns.0.list(), ["rt"]);
    wait_until("detached state", || !ns.0.info("rt").unwrap().attached);

    let again = Client::attach(&ns, "rt", 80, 24);
    assert_eq!(again.screen(), screen);
    assert_eq!(again.cursor(), cursor);
    let info = ns.0.info("rt").unwrap();
    assert!(info.attached);
    assert_eq!((info.cols, info.rows), (80, 24));
    assert!(info.tty.as_deref().is_some_and(|t| t.starts_with("/dev/")));
    assert_eq!(info.argv, ["/bin/sh"]);
}

#[test]
fn alternate_screen_state_survives_reattach() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("alt"), 60, 20);
    c.type_line("echo primary-marker");
    c.wait_output("primary-marker\r\n");
    c.type_line("printf '\\033[?1049h\\033[H\\033[?2004halt-marker'; read x");
    c.wait_output("alt-marker");
    c.settle(Duration::from_millis(300));
    assert!(c.term.mode().contains(TermMode::ALT_SCREEN));
    let cursor = c.cursor();
    drop(c);

    let mut again = Client::attach(&ns, "alt", 60, 20);
    assert!(
        again
            .term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::BRACKETED_PASTE)
    );
    assert!(
        again.screen().starts_with("alt-marker"),
        "{}",
        again.screen()
    );
    assert_eq!(again.cursor(), cursor);
    // The primary screen comes back with its content when the program leaves.
    again.tx.input(b"\r").unwrap();
    again.type_line("printf '\\033[?1049l'; echo left-alt");
    again.wait_output("left-alt\r\n");
    again.settle(Duration::from_millis(200));
    assert!(!again.term.mode().contains(TermMode::ALT_SCREEN));
    assert!(
        again.screen().contains("primary-marker"),
        "{}",
        again.screen()
    );
}

#[test]
fn resize_reaches_the_child() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("rs"), 80, 24);
    c.tx.resize(101, 33, 9, 18).unwrap();
    c.type_line("stty size");
    c.wait_output("33 101");
    assert_eq!(
        (ns.0.info("rs").unwrap().cols, ns.0.info("rs").unwrap().rows),
        (101, 33)
    );
}

#[test]
fn queries_are_answered_while_detached() {
    let ns = TestNs::new();
    let out = scratch(&format!("cpr-{}", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let script = format!(
        "stty raw -echo; printf 'abc\\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); \
         printf '\\033[c'; d=$(dd bs=1 count=5 2>/dev/null); \
         printf '%s|%s' \"$r\" \"$d\" | od -An -c | tr -s ' ' > {0}.tmp; mv {0}.tmp {0}; sleep 100",
        out.display()
    );
    deck_host::start(&ns.0, &spec("q", "/bin/sh", &["-c", &script]), 80, 24).unwrap();
    wait_until("query replies", || out.exists());
    let got = std::fs::read_to_string(&out).unwrap();
    assert_eq!(got.trim(), r"033 [ 1 ; 4 R | 033 [ ? 6 c");
}

#[test]
fn second_attach_displaces_the_first() {
    let ns = TestNs::new();
    let mut first = Client::create(&ns, &shell("dp"), 80, 24);
    let mut second = Client::attach(&ns, "dp", 80, 24);
    assert_eq!(first.next(), HostEvent::Displaced);
    assert_eq!(first.next(), HostEvent::Closed);
    second.type_line("echo from-second");
    second.wait_output("from-second\r\n");
    assert!(ns.0.info("dp").unwrap().attached);
}

#[test]
fn kill_removes_session_and_process_tree() {
    let ns = TestNs::new();
    let pid_file = scratch(&format!("gpid-{}", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let script = format!(
        "sleep 1000 & echo $! > {0}.tmp; mv {0}.tmp {0}; wait",
        pid_file.display()
    );
    deck_host::start(&ns.0, &spec("k", "/bin/sh", &["-c", &script]), 80, 24).unwrap();
    wait_until("grandchild pid", || pid_file.exists());
    let grandchild: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let info = ns.0.info("k").unwrap();
    assert!(pid_alive(grandchild) && pid_alive(info.child_pid) && pid_alive(info.daemon_pid));

    ns.0.kill("k").unwrap();
    assert!(!ns.0.has("k"));
    assert!(ns.0.list().is_empty());
    assert!(!ns.dir().join("k.sock").exists());
    wait_until("process tree gone", || {
        !pid_alive(grandchild) && !pid_alive(info.child_pid) && !pid_alive(info.daemon_pid)
    });
    // Killing a session that is gone is not an error.
    ns.0.kill("k").unwrap();
}

#[test]
fn attached_kill_reports_exit() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("ak"), 80, 24);
    c.tx.kill().unwrap();
    loop {
        match c.next() {
            HostEvent::Exited(_) => break,
            HostEvent::Output(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    wait_until("endpoint gone", || !ns.0.has("ak"));
}

#[test]
fn child_exit_reports_code_and_removes_endpoint() {
    let ns = TestNs::new();
    let mut c = Client::create(
        &ns,
        &spec("ex", "/bin/sh", &["-c", "echo bye; sleep 0.3; exit 7"]),
        80,
        24,
    );
    loop {
        match c.next() {
            HostEvent::Exited(code) => {
                assert_eq!(code, Some(7));
                break;
            }
            HostEvent::Output(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(c.screen().contains("bye"), "{}", c.screen());
    assert_eq!(c.next(), HostEvent::Closed);
    assert!(!ns.0.has("ex"));
    assert!(!ns.dir().join("ex.sock").exists());
}

#[test]
fn startup_failure_is_reported_from_the_log() {
    let ns = TestNs::new();
    let err =
        deck_host::start(&ns.0, &spec("bad", "/nonexistent/program", &[]), 80, 24).unwrap_err();
    match err {
        Error::DaemonStartup(msg) => assert!(msg.contains("/nonexistent/program"), "{msg}"),
        other => panic!("expected DaemonStartup, got {other:?}"),
    }
    assert!(!ns.0.has("bad"));
}

#[test]
fn second_daemon_for_the_same_name_is_refused() {
    let ns = TestNs::new();
    deck_host::start(&ns.0, &shell("one"), 80, 24).unwrap();
    assert!(matches!(
        deck_host::start(&ns.0, &shell("one"), 80, 24),
        Err(Error::AlreadyRunning(_))
    ));
}

#[test]
fn protocol_mismatch_is_a_typed_error() {
    let ns = TestNs::new();
    let dir = ns.dir();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(dir.join("old.sock")).unwrap();
    let server = std::thread::spawn(move || {
        let mut kinds = Vec::new();
        for _ in 0..3 {
            let (mut s, _) = listener.accept().unwrap();
            let mut payload = Vec::new();
            let hello = deck_host::protocol::read_frame(&mut s, &mut payload).unwrap();
            assert_eq!(hello, Some(deck_host::protocol::Kind::Hello));
            let reply = deck_host::protocol::Hello {
                protocol: 99,
                version: "99.0.0".into(),
            };
            deck_host::protocol::write_json(
                &mut s,
                &mut Vec::new(),
                deck_host::protocol::Kind::Hello,
                &reply,
            )
            .unwrap();
            if let Ok(Some(kind)) = deck_host::protocol::read_frame(&mut s, &mut payload) {
                kinds.push(kind);
            }
        }
        kinds
    });
    match ns.0.info("old") {
        Err(Error::ProtocolMismatch {
            ours,
            theirs,
            host_version,
        }) => {
            assert_eq!(
                (ours, theirs, host_version.as_str()),
                (deck_host::PROTOCOL, 99, "99.0.0")
            );
        }
        other => panic!("expected ProtocolMismatch, got {other:?}"),
    }
    assert!(matches!(
        deck_host::attach(&ns.0, "old", params(80, 24)),
        Err(Error::ProtocolMismatch { .. })
    ));
    // Kill speaks to any protocol version.
    let ns_kill = ns.0.clone();
    let killer = std::thread::spawn(move || ns_kill.kill("old"));
    assert_eq!(server.join().unwrap(), [deck_host::protocol::Kind::Kill]);
    killer.join().unwrap().unwrap();
}

#[test]
fn group_accessible_endpoint_dir_is_refused() {
    let ns = TestNs::new();
    let dir = ns.dir();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = deck_host::create_or_attach(&ns.0, &shell("p"), params(80, 24)).err();
    assert!(
        matches!(&err, Some(Error::InsecureDir { path, .. }) if *path == dir),
        "{err:?}"
    );
    assert!(matches!(ns.0.info("p"), Err(Error::InsecureDir { .. })));
    assert!(!ns.0.has("p"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn cli_new_ls_dump_kill() {
    let ns = TestNs::new();
    let run = common::run_cli;
    let nsa = ns.0.as_str();
    let (ok, stdout, stderr) = run(&[
        "new",
        "--ns",
        nsa,
        "cli",
        "--size",
        "40x10",
        "--",
        "/bin/sh",
        "-c",
        "echo cli-marker; sleep 100",
    ]);
    assert!(ok && stdout.is_empty(), "{stderr}");
    assert_eq!(run(&["ls", "--ns", nsa]).1, "cli\n");
    wait_until("marker on screen", || {
        run(&["dump", "--ns", nsa, "cli"]).1.contains("cli-marker")
    });
    let (ok, _, stderr) = run(&["new", "--ns", nsa, "cli", "--", "/bin/sh"]);
    assert!(!ok && stderr.contains("already running"), "{stderr}");
    let info: serde_json::Value =
        serde_json::from_str(&run(&["info", "--ns", nsa, "cli"]).1).unwrap();
    assert_eq!(info["cols"], 40);
    assert!(run(&["kill", "--ns", nsa, "cli"]).0);
    assert_eq!(run(&["ls", "--ns", nsa]).1, "");
}

fn zombie_children() -> Vec<String> {
    let me = std::process::id().to_string();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter(|stat| {
            // `pid (comm) state ppid …`; comm may contain spaces.
            let rest = stat.rsplit_once(") ").map_or("", |(_, r)| r);
            let mut fields = rest.split(' ');
            fields.next() == Some("Z") && fields.next() == Some(me.as_str())
        })
        .collect()
}

#[test]
fn launcher_is_reaped() {
    let ns = TestNs::new();
    deck_host::start(&ns.0, &shell("z"), 80, 24).unwrap();
    let _c = Client::create(&ns, &shell("z2"), 80, 24);
    // Other tests' launchers may be zombies for a moment, but none may stay.
    wait_until("no zombie children", || zombie_children().is_empty());
}

#[test]
fn child_exiting_at_launch_is_still_served_with_final_screen_and_code() {
    let ns = TestNs::new();
    let nsa = ns.0.as_str();
    let (ok, _, stderr) = common::run_cli(&[
        "new",
        "--ns",
        nsa,
        "quick",
        "--",
        "/bin/sh",
        "-c",
        "echo launch-error; exit 5",
    ]);
    assert!(ok, "{stderr}");
    let (_, dump, _) = common::run_cli(&["dump", "--ns", nsa, "quick"]);
    assert!(dump.contains("launch-error"), "{dump}");

    // The first attach (here via create_or_attach) sees the final screen, then the code.
    let attached = deck_host::create_or_attach(&ns.0, &shell("quick"), params(80, 24)).unwrap();
    let mut c = Client::new(attached, 80, 24);
    c.expect_snapshot();
    assert!(c.screen().contains("launch-error"), "{}", c.screen());
    assert_eq!(c.next(), HostEvent::Exited(Some(5)));
    assert_eq!(c.next(), HostEvent::Closed);
    wait_until("endpoint gone", || !ns.0.has("quick"));

    // Logs are private.
    let log = ns.0.log_path("quick").unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&log), 0o600);
    assert_eq!(mode(log.parent().unwrap()), 0o700);
    assert_eq!(mode(log.parent().unwrap().parent().unwrap()), 0o700);
}

#[test]
fn unseen_exit_lingers_until_killed() {
    let ns = TestNs::new();
    deck_host::start(&ns.0, &spec("gone", "/bin/sh", &["-c", "exit 2"]), 80, 24).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(ns.0.has("gone"));
    ns.0.kill("gone").unwrap();
    assert!(!ns.0.has("gone"));
}

fn rss_kib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[test]
fn input_to_a_child_that_never_reads_is_bounded() {
    let ns = TestNs::new();
    let c = Client::create(
        &ns,
        &spec(
            "full",
            "/bin/sh",
            &["-c", "stty raw -echo; exec sleep 1000"],
        ),
        80,
        24,
    );
    let daemon = ns.0.info("full").unwrap().daemon_pid;
    let tx = c.tx.clone();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sent2 = std::sync::Arc::clone(&sent);
    std::thread::spawn(move || {
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..64 {
            if tx.input(&chunk).is_err() {
                break;
            }
            sent2.fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed);
        }
    });
    // The sender stalls once the daemon stops reading it (budget plus socket buffers).
    let mut last = 0;
    wait_until("sender stalled", || {
        std::thread::sleep(Duration::from_millis(300));
        let now = sent.load(std::sync::atomic::Ordering::Relaxed);
        let stalled = now == last && now > 0;
        last = now;
        stalled
    });
    assert!(last < 32 << 20, "sent {last} bytes without backpressure");
    assert!(
        rss_kib(daemon) < 48 * 1024,
        "daemon RSS {} KiB",
        rss_kib(daemon)
    );
    // Other connections are still served.
    assert!(ns.0.info("full").unwrap().attached);
    ns.0.kill("full").unwrap();
    assert!(!ns.0.has("full"));
}

#[test]
fn clients_that_drop_immediately_do_not_break_the_endpoint() {
    use std::io::Write;
    let ns = TestNs::new();
    deck_host::start(&ns.0, &shell("drop"), 80, 24).unwrap();
    let socket = ns.dir().join("drop.sock");
    for i in 0..30 {
        let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        if i % 2 == 1 {
            // A partial frame, then gone before reading the Hello reply.
            let _ = s.write_all(&[5, 0]);
        }
        drop(s);
    }
    assert_eq!(ns.0.list(), ["drop"]);
    assert!(ns.0.info("drop").is_ok());
    let mut c = Client::attach(&ns, "drop", 80, 24);
    c.type_line("echo still-served");
    c.wait_output("still-served\r\n");
}

// --- Read-only observers -------------------------------------------------------------------

/// A hand-driven observer connection (Hello + Observe), read through its snapshot.
fn raw_observer(ns: &TestNs, name: &str) -> UnixStream {
    let mut s = UnixStream::connect(ns.dir().join(format!("{name}.sock"))).unwrap();
    s.set_read_timeout(Some(common::WAIT)).unwrap();
    let (mut buf, mut payload) = (Vec::new(), Vec::new());
    wire::write_json(&mut s, &mut buf, Kind::Hello, &wire::Hello::ours()).unwrap();
    assert_eq!(
        wire::read_frame(&mut s, &mut payload).unwrap(),
        Some(Kind::Hello)
    );
    let observe = wire::ObserveParams { history_lines: 0 };
    wire::write_json(&mut s, &mut buf, Kind::Observe, &observe).unwrap();
    let mut kinds = Vec::new();
    loop {
        let kind = wire::read_frame(&mut s, &mut payload)
            .unwrap()
            .expect("frame");
        kinds.push(kind);
        if kind == Kind::SnapshotEnd {
            break;
        }
    }
    assert_eq!(kinds[..3], [Kind::Info, Kind::Resized, Kind::Snapshot]);
    s
}

/// Expects the daemon's read-only refusal: the connection ends (frames already in flight may
/// precede the end; a frame cut by the abort counts as the end too).
fn expect_refused(s: &mut UnixStream, sent: Kind) {
    let mut payload = Vec::new();
    loop {
        match wire::read_frame(s, &mut payload) {
            Ok(Some(
                Kind::Info | Kind::Resized | Kind::Snapshot | Kind::SnapshotEnd | Kind::Output,
            )) => {}
            Ok(None) => return,
            Err(Error::Protocol(msg)) if msg.starts_with("truncated") => return,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionReset => return,
            other => panic!("{sent:?}: expected the connection to close, got {other:?}"),
        }
    }
}

/// Threads of process `pid`.
fn thread_count(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .count()
}

/// Lines of the form `n<number>`, in order (screen text or raw `\r\n` output).
fn numbered_lines(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix('n')?.parse().ok())
        .collect()
}

fn visible(screen: &str, rows: usize) -> Vec<&str> {
    let lines: Vec<&str> = screen.lines().collect();
    lines[lines.len().saturating_sub(rows)..].to_vec()
}

#[test]
fn observers_join_gap_free_without_displacing_or_resizing_the_controller() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("gap"), 80, 24);
    // Paced (a short sleep every 100 lines, ~1.5 s in all) so the joins below land while
    // output is still flowing rather than after the loop finished.
    c.type_line(
        "i=0; while [ $i -lt 3000 ]; do echo n$i; i=$((i+1)); \
         [ $((i % 100)) -eq 0 ] && sleep 0.05; done; echo loop-''done",
    );
    // Joins at different points of the stream, each with enough history for all of it.
    let mut observers = Vec::new();
    for mark in ["n10\r\n", "n1000\r\n", "n2000\r\n"] {
        c.wait_output(mark);
        observers.push(Observer::new(&ns, "gap", 10_000));
    }
    c.wait_output("loop-done\r\n");
    c.settle(Duration::from_millis(300));
    let expected: Vec<u32> = (0..3000).collect();
    for o in &mut observers {
        // The end may already be in the snapshot (late join), so wait on the screen.
        o.wait_screen("loop-done");
        o.settle(Duration::from_millis(200));
        // Snapshot plus live output: every line exactly once, in order.
        assert_eq!(numbered_lines(&o.screen()), expected);
        assert_eq!(visible(&o.screen(), 24), visible(&c.screen(), 24));
        assert_eq!(o.grid.cols, 80);
        assert!(o.resizes.is_empty());
    }
    // The first join happened mid-loop: part of the sequence arrived live after the snapshot.
    assert!(
        !numbered_lines(&String::from_utf8_lossy(&observers[0].output)).is_empty(),
        "first observer joined after the loop; no live join was exercised"
    );
    // The controller is still attached, unresized, and keeps input.
    let info = ns.0.info("gap").unwrap();
    assert!(info.attached);
    assert_eq!((info.cols, info.rows), (80, 24));
    c.type_line("echo ctl-''still");
    c.wait_output("ctl-still\r\n");
    for o in &mut observers {
        o.wait_output("ctl-still\r\n");
    }
}

#[test]
fn controller_resize_reaches_observers_in_stream_order() {
    let ns = TestNs::new();
    let c = Client::create(&ns, &shell("ors"), 80, 24);
    let mut o = Observer::new(&ns, "ors", 100);
    assert_eq!(
        o.grid,
        Grid {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16
        }
    );
    c.tx.resize(101, 33, 9, 18).unwrap();
    c.type_line("stty size; echo size-''done");
    let mut resized = false;
    loop {
        match o.next() {
            ObserverEvent::Resized(grid) => {
                assert_eq!(
                    grid,
                    Grid {
                        cols: 101,
                        rows: 33,
                        cell_width: 9,
                        cell_height: 18
                    }
                );
                resized = true;
            }
            ObserverEvent::Output(_) => {
                let out = String::from_utf8_lossy(&o.output).into_owned();
                if out.contains("33 101") {
                    assert!(resized, "output of the new size arrived before Resized");
                }
                if out.contains("size-done") {
                    break;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(o.resizes.len(), 1);
    let info = ns.0.info("ors").unwrap();
    assert_eq!((info.cols, info.rows), (101, 33));
}

#[test]
fn observer_frames_cannot_affect_the_session() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("ro"), 80, 24);
    c.type_line("echo ready-''ro");
    c.wait_output("ready-ro\r\n");
    let child = ns.0.info("ro").unwrap().child_pid;
    fn json(value: &impl serde::Serialize) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }
    let mut palette = Palette::default();
    palette.0[Palette::BACKGROUND] = [1, 2, 3];
    let frames: Vec<(Kind, Vec<u8>)> = vec![
        (Kind::Input, b"echo pwned-''input\r".to_vec()),
        (
            Kind::Resize,
            json(&wire::ResizeParams {
                cols: 33,
                rows: 11,
                cell_width: 1,
                cell_height: 1,
            }),
        ),
        (Kind::Palette, json(&palette)),
        (Kind::Attach, json(&params(33, 11))),
        (Kind::Kill, Vec::new()),
        (
            Kind::Observe,
            json(&wire::ObserveParams { history_lines: 0 }),
        ),
        (Kind::Peek, json(&wire::PeekParams { history_lines: 0 })),
        (Kind::Info, Vec::new()),
    ];
    for (kind, payload) in frames {
        let mut s = raw_observer(&ns, "ro");
        wire::write_frame(&mut s, &mut Vec::new(), kind, &payload).unwrap();
        expect_refused(&mut s, kind);
    }
    // Nothing reached the session: same controller, size, child; no input executed.
    c.settle(Duration::from_millis(300));
    assert!(!String::from_utf8_lossy(&c.output).contains("pwned-input"));
    let info = ns.0.info("ro").unwrap();
    assert!(info.attached);
    assert_eq!((info.cols, info.rows, info.child_pid), (80, 24, child));
    assert!(pid_alive(child));
    c.type_line("echo still-''mine");
    c.wait_output("still-mine\r\n");
}

#[test]
fn violating_observers_that_never_read_are_cut_off_without_leaking_threads() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("leak"), 80, 24);
    // Megabytes of history: a full snapshot cannot fit into the socket buffers.
    c.type_line(
        "seq -f 'history-%06.0f-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' 1 60000; \
         echo hist-''done",
    );
    drain_client_until(&mut c, "hist-done\r\n");
    c.settle(Duration::from_millis(300));
    let daemon = ns.0.info("leak").unwrap().daemon_pid;
    std::thread::sleep(Duration::from_millis(300));
    let baseline = thread_count(daemon);

    // Observe with the whole history, violate at once, and never read anything.
    let mut held = Vec::new();
    for _ in 0..8 {
        let mut s = UnixStream::connect(ns.dir().join("leak.sock")).unwrap();
        s.set_read_timeout(Some(common::WAIT)).unwrap();
        let (mut buf, mut payload) = (Vec::new(), Vec::new());
        wire::write_json(&mut s, &mut buf, Kind::Hello, &wire::Hello::ours()).unwrap();
        assert_eq!(
            wire::read_frame(&mut s, &mut payload).unwrap(),
            Some(Kind::Hello)
        );
        let observe = wire::ObserveParams {
            history_lines: 50_000,
        };
        wire::write_json(&mut s, &mut buf, Kind::Observe, &observe).unwrap();
        wire::write_frame(&mut s, &mut buf, Kind::Input, b"echo pwned-''unread\r").unwrap();
        held.push(s);
    }
    // Their writers and connection threads end although the peers still hold the sockets.
    wait_until("violating observers' threads gone", || {
        thread_count(daemon) <= baseline
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(thread_count(daemon) <= baseline, "daemon threads grew");
    for s in &mut held {
        expect_refused(s, Kind::Input);
    }

    c.output.clear();
    c.type_line("echo ctl-''alive");
    c.wait_output("ctl-alive\r\n");
    assert!(!String::from_utf8_lossy(&c.output).contains("pwned-unread"));
    let info = ns.0.info("leak").unwrap();
    assert!(info.attached);
    assert_eq!((info.cols, info.rows), (80, 24));
}

/// Consumes events until `needle` was output, keeping only a short tail (floods).
fn drain_client_until(c: &mut Client, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !String::from_utf8_lossy(&c.output).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?}"
        );
        let keep_from = c.output.len().saturating_sub(64);
        c.output.drain(..keep_from);
        match c.next() {
            HostEvent::Output(_) => {}
            other => panic!("unexpected {other:?} while waiting for {needle:?}"),
        }
    }
}

fn drain_observer_until(o: &mut Observer, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !String::from_utf8_lossy(&o.output).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?}"
        );
        let keep_from = o.output.len().saturating_sub(64);
        o.output.drain(..keep_from);
        match o.next() {
            ObserverEvent::Output(_) => {}
            other => panic!("unexpected {other:?} while waiting for {needle:?}"),
        }
    }
}

#[test]
fn slow_observer_is_disconnected_alone() {
    const FLOOD: usize = 16_000_000;
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("slow"), 80, 24);
    // Subscribe, then never read again: one raw connection and one through the public API.
    let mut stalled = raw_observer(&ns, "slow");
    let (_, stalled_api) = deck_host::observe(&ns.0, "slow", 0).unwrap();
    let (mut stalled_rx, _stalled_tx) = stalled_api.split();
    let mut fast = Observer::new(&ns, "slow", 0);
    c.type_line(&format!(
        "yes 0123456789abcdef | head -c {FLOOD}; echo flood-''done"
    ));
    drain_client_until(&mut c, "flood-done\r\n");
    drain_observer_until(&mut fast, "flood-done\r\n");

    // The stalled observer was cut off: buffered frames, then the end; no `Exited`.
    let mut payload = Vec::new();
    let mut received = 0;
    loop {
        match wire::read_frame(&mut stalled, &mut payload) {
            Ok(Some(Kind::Output)) => received += payload.len(),
            Ok(Some(kind)) => panic!("unexpected {kind:?} on the stalled observer"),
            Ok(None) => break,
            // Aborted mid-frame.
            Err(Error::Protocol(msg)) if msg.starts_with("truncated") => break,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("stalled observer was not disconnected: {e}"),
        }
    }
    assert!(
        received < FLOOD,
        "stalled observer received {received} bytes"
    );
    // Through the API the cut-off is a clean `Closed`, never an error or `Exited`.
    let drained = std::thread::spawn(move || {
        let mut received = 0;
        loop {
            match stalled_rx.recv().unwrap() {
                ObserverEvent::Snapshot { .. } | ObserverEvent::Resized(_) => {}
                ObserverEvent::Output(bytes) => received += bytes.len(),
                ObserverEvent::Closed => return received,
                other => panic!("unexpected {other:?} on the stalled API observer"),
            }
        }
    });
    wait_until("stalled API observer closed", || drained.is_finished());
    let received = drained.join().unwrap();
    assert!(
        received < FLOOD,
        "stalled API observer received {received} bytes"
    );

    // Controller and the other observer are unaffected.
    assert!(ns.0.info("slow").unwrap().attached);
    c.type_line("echo after-''flood");
    c.wait_output("after-flood\r\n");
    fast.wait_output("after-flood\r\n");
}

#[test]
fn observer_detach_leaves_the_session_and_exit_reaches_everyone() {
    let ns = TestNs::new();
    let mut c = Client::create(&ns, &shell("oex"), 80, 24);
    let mut gone = Observer::new(&ns, "oex", 0);
    // Detach from another thread than the one blocked in `recv`.
    gone.tx.detach().unwrap();
    loop {
        match gone.next() {
            ObserverEvent::Output(_) => {}
            ObserverEvent::Closed => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(ns.0.info("oex").unwrap().attached);

    let mut o = Observer::new(&ns, "oex", 0);
    c.type_line("echo bye; exit 3");
    loop {
        match o.next() {
            ObserverEvent::Output(_) => {}
            ObserverEvent::Exited(code) => {
                assert_eq!(code, Some(3));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(o.next(), ObserverEvent::Closed);
    loop {
        match c.next() {
            HostEvent::Output(_) => {}
            HostEvent::Exited(code) => {
                assert_eq!(code, Some(3));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(c.next(), HostEvent::Closed);
    // Observers do not keep the session alive.
    wait_until("endpoint gone", || !ns.0.has("oex"));
}

#[test]
fn observing_an_unseen_exit_shows_the_final_screen_without_ending_the_linger() {
    let ns = TestNs::new();
    let script = &["-c", "echo last-words; exit 4"];
    deck_host::start(&ns.0, &spec("lin", "/bin/sh", script), 80, 24).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut o = Observer::new(&ns, "lin", 100);
    assert!(o.screen().contains("last-words"), "{}", o.screen());
    assert_eq!(o.next(), ObserverEvent::Exited(Some(4)));
    assert_eq!(o.next(), ObserverEvent::Closed);
    drop(o);
    // Still lingering for its first real attach, which gets the final screen and code.
    assert!(ns.0.has("lin"));
    let mut c = Client::new(
        deck_host::attach(&ns.0, "lin", params(80, 24)).unwrap(),
        80,
        24,
    );
    c.expect_snapshot();
    assert!(c.screen().contains("last-words"), "{}", c.screen());
    assert_eq!(c.next(), HostEvent::Exited(Some(4)));
    wait_until("endpoint gone", || !ns.0.has("lin"));
}

#[test]
fn observing_an_older_daemon_is_unsupported_without_fallback() {
    let ns = TestNs::new();
    let dir = ns.dir();
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(dir.join("old.sock")).unwrap();
    // Same protocol number, but an `Observe` makes it drop the connection like any older
    // daemon does on an unknown frame kind. The second connection is the liveness probe.
    let server = std::thread::spawn(move || {
        let mut kinds = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut payload = Vec::new();
            let hello = wire::read_frame(&mut s, &mut payload).unwrap();
            assert_eq!(hello, Some(Kind::Hello));
            let reply = wire::Hello {
                protocol: deck_host::PROTOCOL,
                version: "0.0.1-old".into(),
            };
            wire::write_json(&mut s, &mut Vec::new(), Kind::Hello, &reply).unwrap();
            if let Ok(Some(kind)) = wire::read_frame(&mut s, &mut payload) {
                kinds.push(kind);
            }
        }
        kinds
    });
    match deck_host::observe(&ns.0, "old", 0).err() {
        Some(Error::Unsupported(name)) => assert_eq!(name, "old"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // Only the Observe request was made: no attach fallback, no retry.
    assert_eq!(server.join().unwrap(), [Kind::Observe]);
}
