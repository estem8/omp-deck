//! Headless PTY tests: real `sh -c` children through `Terminal` (and `TerminalView` driven by a
//! hidden egui::Context). Every wait is bounded.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{contains, poll_until, sh_quote, spawn_sh, wait_file};
use deck_term::TerminalView;
use deck_term::input::{self, Key, KeyInput, KeyRules, Mods};
use egui::{Event, ImeEvent, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2, vec2};

#[test]
fn osc_title_reported() {
    let mut t = spawn_sh(r"printf '\033]0;hi\007'; sleep 5", false);
    let title = poll_until(&mut t, "title", |ev, _| ev.title_changed.clone());
    assert_eq!(title, "hi");
    assert_eq!(t.title(), Some("hi"));
}

#[test]
fn exit_code_reported_once() {
    let mut t = spawn_sh("exit 3", false);
    let code = poll_until(&mut t, "exit", |ev, _| ev.exited);
    assert_eq!(code, 3);
    assert_eq!(t.exit_code(), Some(3));
    assert!(t.is_exited());
    for _ in 0..30 {
        assert_eq!(t.poll().exited, None, "exit reported twice");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn child_exit_clears_held_run_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let release = dir.path().join("exit");
    let script = format!(
        r"printf '\033]7501;state=working\007'; while test ! -e {}; do sleep 0.01; done; exit 0",
        sh_quote(&release.display().to_string())
    );
    let mut t = spawn_sh(&script, false);
    poll_until(&mut t, "working report", |_, t| t.run_state());
    std::fs::write(&release, []).expect("release child");
    poll_until(&mut t, "child exit", |ev, _| ev.exited);
    assert_eq!(t.run_state(), None);
}

#[test]
fn osc11_da1_dsr_replies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("replies");
    // DA1 (5 bytes) + DSR 6 at 1;1 (6) + OSC 11 reply (24) = 35 bytes.
    let script = format!(
        r"stty raw -echo; printf '\033]11;?\007\033[c\033[6n'; dd bs=1 count=35 of={} 2>/dev/null; sleep 5",
        sh_quote(&out.display().to_string())
    );
    let mut t = spawn_sh(&script, false);
    let data = wait_file(&mut t, &out, "replies", |d| d.len() >= 35);
    assert!(
        contains(&data, b"\x1b]11;rgb:2323/2626/2727\x07"),
        "{:?}",
        String::from_utf8_lossy(&data)
    );
    assert!(
        contains(&data, b"\x1b[?6c"),
        "{:?}",
        String::from_utf8_lossy(&data)
    );
    assert!(
        contains(&data, b"\x1b[1;1R"),
        "{:?}",
        String::from_utf8_lossy(&data)
    );
}

fn raw_input(size: egui::Vec2, events: Vec<Event>) -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
        events,
        ..RawInput::default()
    }
}

fn frame(ctx: &egui::Context, t: &mut deck_term::Terminal, size: egui::Vec2, events: Vec<Event>) {
    let output = ctx.run_ui(raw_input(size, events), |ui| {
        TerminalView::new(t).show(ui);
    });
    output.drop_without_applying_deltas();
}

fn click(at: Pos2) -> Vec<Event> {
    vec![
        Event::PointerMoved(at),
        Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        },
        Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        },
    ]
}

fn key(key: egui::Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: Some(key),
        pressed: true,
        repeat: false,
        modifiers,
    }
}

#[test]
fn resize_delivers_sigwinch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let size_file = dir.path().join("size");
    let ready = dir.path().join("ready");
    let script = format!(
        "trap 'stty size > {s}' WINCH; : > {r}; while :; do sleep 0.05; done",
        s = sh_quote(&size_file.display().to_string()),
        r = sh_quote(&ready.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "trap installed", |_| ready.exists());

    let ctx = egui::Context::default();
    frame(&ctx, &mut t, vec2(800.0, 600.0), Vec::new());
    let first = wait_file(&mut t, &size_file, "first SIGWINCH", |d| {
        !d.is_empty() && d.ends_with(b"\n")
    });
    let first = String::from_utf8(first).expect("utf8");
    let (rows, cols) = parse_size(&first);
    assert!(
        rows > 5 && cols > 20 && (rows, cols) != (24, 80),
        "size after resize: {first:?}"
    );

    frame(&ctx, &mut t, vec2(400.0, 300.0), Vec::new());
    let second = wait_file(&mut t, &size_file, "second SIGWINCH", |d| {
        let s = String::from_utf8_lossy(d);
        s.ends_with('\n') && s.trim() != first.trim()
    });
    let (rows2, cols2) = parse_size(&String::from_utf8(second).expect("utf8"));
    assert!(
        rows2 < rows && cols2 < cols,
        "{rows2}x{cols2} vs {rows}x{cols}"
    );
}

fn parse_size(s: &str) -> (usize, usize) {
    let mut it = s
        .split_whitespace()
        .map(|n| n.parse::<usize>().expect("number"));
    (it.next().expect("rows"), it.next().expect("cols"))
}

/// Keys through the real widget path (focus by click, egui Key events) into `cat -v`.
fn widget_keys_into_cat(prefix: &str, keys: Vec<Event>, expect: &[u8]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        "stty raw -echo; printf '{prefix}'; : > {r}; cat -v > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    // Let the mode-setting output reach the Term.
    std::thread::sleep(Duration::from_millis(200));

    let ctx = egui::Context::default();
    let size = vec2(640.0, 400.0);
    frame(&ctx, &mut t, size, Vec::new());
    frame(&ctx, &mut t, size, click(pos2(50.0, 50.0)));
    frame(&ctx, &mut t, size, Vec::new());
    frame(&ctx, &mut t, size, keys);
    let data = wait_file(&mut t, &out, "cat -v output", |d| d.len() >= expect.len());
    assert_eq!(
        String::from_utf8_lossy(&data),
        String::from_utf8_lossy(expect)
    );
}

#[test]
fn widget_shift_enter_ctrl_v_legacy() {
    widget_keys_into_cat(
        "",
        vec![
            key(egui::Key::Enter, Modifiers::SHIFT),
            key(egui::Key::V, Modifiers::CTRL),
            key(egui::Key::C, Modifiers::CTRL),
            key(egui::Key::A, Modifiers::NONE),
            Event::Text("a".into()),
            key(egui::Key::A, Modifiers::ALT),
            Event::Text("a".into()),
        ],
        // raw mode: Ctrl+C is data, not SIGINT.
        b"^M^V^Ca^[a",
    );
}

#[test]
fn widget_shift_enter_kitty() {
    // The child requests kitty disambiguate (CSI > 1 u): the Term mode drives the encoder.
    widget_keys_into_cat(
        r"\033[>1u",
        vec![
            key(egui::Key::Enter, Modifiers::SHIFT),
            key(egui::Key::V, Modifiers::CTRL),
            key(egui::Key::Escape, Modifiers::NONE),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
        b"^[[13;2u^[[118;5u^[[27u^M",
    );
}

fn release(key: egui::Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: Some(key),
        pressed: false,
        repeat: false,
        modifiers,
    }
}

#[test]
fn widget_direct_modify_other_keys() {
    // alacritty ignores `CSI > 4 ; 2 m`; the PTY wrapper tracks it, so modified keys use
    // modifyOtherKeys (lone Escape stays a plain ESC outside tmux).
    widget_keys_into_cat(
        r"\033[>4;2m",
        vec![
            key(egui::Key::Enter, Modifiers::SHIFT),
            key(egui::Key::Escape, Modifiers::NONE),
            key(egui::Key::A, Modifiers::CTRL | Modifiers::SHIFT),
            key(egui::Key::A, Modifiers::CTRL),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
        b"^[[27;2;13~^[^[[27;6;97~^A^M",
    );
}

#[test]
fn widget_modify_other_keys_reset() {
    widget_keys_into_cat(
        r"\033[>4;2m\033[>4m",
        vec![
            key(egui::Key::Enter, Modifiers::SHIFT),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
        b"^M^M",
    );
}

#[test]
fn widget_clipboard_shortcut_releases_swallowed_kitty() {
    // Flags 3 (disambiguate + event types): releases are reported, except for the local
    // clipboard shortcuts — also when Shift was let go before the letter.
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
    widget_keys_into_cat(
        r"\033[>3u",
        vec![
            key(egui::Key::A, Modifiers::NONE),
            Event::Text("a".into()),
            release(egui::Key::A, Modifiers::NONE),
            key(egui::Key::C, ctrl_shift),
            release(egui::Key::C, Modifiers::CTRL),
            release(egui::Key::Insert, Modifiers::SHIFT),
            release(egui::Key::V, ctrl_shift),
            key(egui::Key::Enter, Modifiers::NONE),
            release(egui::Key::Enter, Modifiers::NONE),
            key(egui::Key::B, Modifiers::CTRL),
        ],
        b"a^[[97;1:3u^M^[[98;5u",
    );
}

#[test]
fn focus_reports_pair_across_set_focused_and_widget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        r"stty raw -echo; printf '\033[?1004h'; : > {r}; cat -v > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    std::thread::sleep(Duration::from_millis(200));

    let ctx = egui::Context::default();
    let size = vec2(640.0, 400.0);
    frame(&ctx, &mut t, size, Vec::new());
    frame(&ctx, &mut t, size, click(pos2(50.0, 50.0)));
    frame(&ctx, &mut t, size, Vec::new());
    // Tab switch away (app-driven), twice: one out report.
    t.set_focused(false);
    t.set_focused(false);
    // Tab shown again: the widget still holds egui focus → one in report.
    frame(&ctx, &mut t, size, Vec::new());
    frame(&ctx, &mut t, size, Vec::new());
    t.set_focused(false);
    t.write(b"Z");
    let expect = b"^[[I^[[O^[[I^[[OZ";
    let data = wait_file(&mut t, &out, "focus reports", |d| d.ends_with(b"Z"));
    assert_eq!(
        String::from_utf8_lossy(&data),
        String::from_utf8_lossy(expect)
    );
}

#[test]
fn bracketed_paste_and_ctrl_v_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        r"stty raw -echo; printf '\033[?2004h'; : > {r}; cat -v > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    std::thread::sleep(Duration::from_millis(200));

    // The mode as the widget would see it: ?2004h was processed by the Term.
    let ctx = egui::Context::default();
    frame(&ctx, &mut t, vec2(640.0, 400.0), Vec::new());
    let mode = alacritty_terminal::term::TermMode::BRACKETED_PASTE;
    t.write(&input::encode_paste("hi\nthere\x1b[201~", mode));
    t.write(&input::encode_key(
        &KeyInput::press(Key::Char('v'), Mods::CTRL),
        Default::default(),
        KeyRules::Legacy,
    ));
    let expect = b"^[[200~hi\nthere^[[201~^V";
    let data = wait_file(&mut t, &out, "paste", |d| d.len() >= expect.len());
    assert_eq!(
        String::from_utf8_lossy(&data),
        String::from_utf8_lossy(expect)
    );
}

/// `cat -v` child in raw mode after `prefix`, focused through the widget by a click.
fn focused_cat(prefix: &str) -> (tempfile::TempDir, deck_term::Terminal, egui::Context) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        "stty raw -echo; printf '{prefix}'; : > {r}; cat -v > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    std::thread::sleep(Duration::from_millis(200));
    let ctx = egui::Context::default();
    let size = vec2(640.0, 400.0);
    frame(&ctx, &mut t, size, Vec::new());
    frame(&ctx, &mut t, size, click(pos2(50.0, 50.0)));
    frame(&ctx, &mut t, size, Vec::new());
    (dir, t, ctx)
}

#[test]
fn hold_input_drops_user_input_but_not_write() {
    // Kitty flags 3: Ctrl+C has no 0x03 byte and releases are reported.
    let (dir, mut t, ctx) = focused_cat(r"\033[>3u");
    let size = vec2(640.0, 400.0);
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
    t.hold_input_until(Instant::now() + Duration::from_secs(30));
    frame(
        &ctx,
        &mut t,
        size,
        vec![
            key(egui::Key::A, Modifiers::NONE),
            Event::Text("a".into()),
            key(egui::Key::C, Modifiers::CTRL),
            key(egui::Key::Enter, Modifiers::NONE),
            Event::Text("t".into()),
            Event::Copy,
            Event::Cut,
            Event::Paste("p".into()),
            Event::Ime(ImeEvent::Commit("i".into())),
            key(egui::Key::V, ctrl_shift),
            key(egui::Key::Insert, Modifiers::SHIFT),
        ],
    );
    // Injected bytes are never held.
    t.write(b"W");
    assert_eq!(t.last_ctrl_c(), None, "held kitty Ctrl+C must not count");
    t.hold_input_until(Instant::now());
    frame(
        &ctx,
        &mut t,
        size,
        vec![
            // Releases of presses dropped while held are swallowed.
            release(egui::Key::A, Modifiers::NONE),
            release(egui::Key::C, Modifiers::CTRL),
            key(egui::Key::B, Modifiers::NONE),
            Event::Text("b".into()),
            release(egui::Key::B, Modifiers::NONE),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
    );
    let out = dir.path().join("cat");
    let expect = b"Wb^[[98;1:3u^M";
    let data = wait_file(&mut t, &out, "input after hold", |d| d.ends_with(b"^M"));
    assert_eq!(
        String::from_utf8_lossy(&data),
        String::from_utf8_lossy(expect)
    );
}

#[test]
fn release_within_hold_ends_held_key_bookkeeping() {
    // Kitty flags 3: releases are reported.
    let (dir, mut t, ctx) = focused_cat(r"\033[>3u");
    let size = vec2(640.0, 400.0);
    t.hold_input_until(Instant::now() + Duration::from_secs(30));
    // Press and release both dropped while held.
    frame(
        &ctx,
        &mut t,
        size,
        vec![
            key(egui::Key::A, Modifiers::NONE),
            Event::Text("a".into()),
            release(egui::Key::A, Modifiers::NONE),
        ],
    );
    t.hold_input_until(Instant::now());
    // A later legit pair is delivered whole.
    frame(
        &ctx,
        &mut t,
        size,
        vec![
            key(egui::Key::A, Modifiers::NONE),
            Event::Text("a".into()),
            release(egui::Key::A, Modifiers::NONE),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
    );
    let out = dir.path().join("cat");
    let data = wait_file(&mut t, &out, "input after hold", |d| d.ends_with(b"^M"));
    assert_eq!(String::from_utf8_lossy(&data), "a^[[97;1:3u^M");
}

#[test]
fn repeat_within_hold_keeps_release_of_earlier_press() {
    // Kitty flags 3: releases are reported.
    let (dir, mut t, ctx) = focused_cat(r"\033[>3u");
    let size = vec2(640.0, 400.0);
    // Pressed before the hold: delivered.
    frame(
        &ctx,
        &mut t,
        size,
        vec![key(egui::Key::A, Modifiers::NONE), Event::Text("a".into())],
    );
    t.hold_input_until(Instant::now() + Duration::from_secs(30));
    // Its auto-repeat while held is dropped and leaves no bookkeeping behind.
    frame(
        &ctx,
        &mut t,
        size,
        vec![
            Event::Key {
                key: egui::Key::A,
                physical_key: Some(egui::Key::A),
                pressed: true,
                repeat: true,
                modifiers: Modifiers::NONE,
            },
            Event::Text("a".into()),
        ],
    );
    t.hold_input_until(Instant::now());
    // The release after the hold matches the delivered press, so the child gets it.
    frame(
        &ctx,
        &mut t,
        size,
        vec![
            release(egui::Key::A, Modifiers::NONE),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
    );
    let out = dir.path().join("cat");
    let data = wait_file(&mut t, &out, "input after hold", |d| d.ends_with(b"^M"));
    assert_eq!(String::from_utf8_lossy(&data), "a^[[97;1:3u^M");
}

#[test]
fn focus_report_after_lone_esc_sent_after_next_write() {
    let (dir, mut t, _ctx) = focused_cat(r"\033[?1004h");
    t.write(b"\x1b");
    // Within the quiet time: deferred, then sent right after bytes that end the ESC.
    t.set_focused(false);
    t.write(b"x");
    // An out/in pair inside one quiet time cancels out.
    t.write(b"\x1b");
    t.set_focused(true);
    t.set_focused(false);
    t.write(b"Z");
    let out = dir.path().join("cat");
    let expect = b"^[[I^[x^[[O^[Z";
    let data = wait_file(&mut t, &out, "focus reports", |d| d.ends_with(b"Z"));
    assert_eq!(
        String::from_utf8_lossy(&data),
        String::from_utf8_lossy(expect)
    );
}

/// Click on app chrome (out), injected lone ESC, re-focus (in) within 1 ms: the child sees
/// ESC, then ≥ 100 ms of silence, then `ESC[I`.
#[test]
fn focus_in_deferred_after_lone_esc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut t, out) = byte_logger(dir.path(), r"printf '\033[?1004h';");
    let ctx = egui::Context::default();
    let size = vec2(640.0, 400.0);
    frame(&ctx, &mut t, size, Vec::new());
    frame(&ctx, &mut t, size, click(pos2(50.0, 50.0)));
    frame(&ctx, &mut t, size, Vec::new());
    let lines = |d: &[u8]| String::from_utf8_lossy(d).lines().count();
    wait_file(&mut t, &out, "focus-in report", |d| lines(d) >= 3);

    t.set_focused(false);
    let esc_at = wall_clock_us();
    t.write(b"\x1b");
    // The widget still holds egui focus: this frame reports focus-in again.
    frame(&ctx, &mut t, size, Vec::new());
    let deadline = Instant::now() + common::TIMEOUT;
    let data = loop {
        frame(&ctx, &mut t, size, Vec::new());
        let data = std::fs::read(&out).unwrap_or_default();
        if lines(&data) >= 10 {
            break String::from_utf8(data).expect("utf8");
        }
        assert!(Instant::now() < deadline, "bytes so far: {data:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    let rows: Vec<(u128, &str)> = data
        .lines()
        .map(|l| {
            let (ts, byte) = l.split_once(' ').expect("timestamp + byte");
            (ts.parse().expect("µs timestamp"), byte)
        })
        .collect();
    let bytes: Vec<&str> = rows.iter().map(|r| r.1).collect();
    assert_eq!(
        bytes,
        ["27", "91", "73", "27", "91", "79", "27", "27", "91", "73"],
        "{data}"
    );
    assert_quiet_time(esc_at, rows[7].0, &data);
}

/// Wall-clock µs since the Unix epoch: the clock of bash's `EPOCHREALTIME` in [`byte_logger`].
fn wall_clock_us() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_micros()
}

/// The lone-ESC quiet time (100 ms) is counted from the `write` call, so the first
/// terminal-generated byte must arrive ≥ 100 ms after `esc_at` (taken just before the
/// write). Measuring from the ESC's own arrival instead would subtract its delivery latency.
/// A missing deferral arrives within a few ms.
fn assert_quiet_time(esc_at: u128, reply_at: u128, data: &str) {
    let gap = reply_at.saturating_sub(esc_at);
    assert!(
        gap >= 100_000,
        "ESC write → first reply {gap} µs (< 100 ms)\n{data}"
    );
}

/// Spawns a fork-free bash loop logging every received byte as a `<µs timestamp> <byte value>`
/// line (reader-side timestamps track arrival closely); `before` runs after raw mode, before
/// the ready marker.
fn byte_logger(dir: &std::path::Path, before: &str) -> (deck_term::Terminal, std::path::PathBuf) {
    let out = dir.join("bytes");
    let ready = dir.join("ready");
    let script = format!(
        "LC_ALL=C; stty raw -echo; {before} : > {r}; \
         while IFS= read -r -n1 -d '' c; do \
         printf '%s %d\\n' \"${{EPOCHREALTIME/[.,]/}}\" \"'$c\" >> {o}; done",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = common::spawn(deck_term::SpawnSpec {
        program: "bash".into(),
        args: vec!["-c".into(), script],
        cwd: std::env::temp_dir(),
        env: vec![("TERM".into(), "xterm-256color".into())],
        tmux_client: false,
    });
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    std::thread::sleep(Duration::from_millis(200));
    (t, out)
}

/// Query replies (CPR, DA1 from the PTY thread; OSC 11 answered by `poll`) generated right
/// after an injected lone ESC wait out the quiet time, then arrive in query order.
#[test]
fn replies_deferred_after_lone_esc_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let go = dir.path().join("go");
    let trigger = format!(
        r"( while [ ! -e {g} ]; do sleep 0.005; done; printf '\033[6n\033[c\033]11;?\007' ) &",
        g = sh_quote(&go.display().to_string()),
    );
    let (mut t, out) = byte_logger(dir.path(), &trigger);
    let esc_at = wall_clock_us();
    t.write(b"\x1b");
    std::fs::write(&go, b"").expect("go marker");
    let replies: &[u8] = b"\x1b[1;1R\x1b[?6c\x1b]11;rgb:2323/2626/2727\x07";
    let lines = |d: &[u8]| String::from_utf8_lossy(d).lines().count();
    let data = wait_file(&mut t, &out, "replies", |d| lines(d) > replies.len());
    let data = String::from_utf8(data).expect("utf8");
    let rows: Vec<(u128, u8)> = data
        .lines()
        .map(|l| {
            let (ts, byte) = l.split_once(' ').expect("timestamp + byte");
            (
                ts.parse().expect("µs timestamp"),
                byte.parse().expect("byte"),
            )
        })
        .collect();
    let bytes: Vec<u8> = rows.iter().map(|r| r.1).collect();
    let mut expect = vec![0x1b];
    expect.extend_from_slice(replies);
    assert_eq!(bytes, expect, "{data}");
    assert_quiet_time(esc_at, rows[1].0, &data);
}

/// Bytes written in the same frame the terminal is dropped (more than the PTY buffer holds,
/// ending in `\r`) all reach the child before the hang-up.
#[test]
fn drop_right_after_write_delivers_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        "stty raw -echo; : > {r}; cat > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    std::thread::sleep(Duration::from_millis(200));
    let mut payload: Vec<u8> = b"0123456789abcdef".repeat(4096);
    payload.push(b'\r');
    t.write(&payload);
    drop(t);
    let deadline = Instant::now() + common::TIMEOUT;
    loop {
        let data = std::fs::read(&out).unwrap_or_default();
        if data.len() >= payload.len() {
            assert!(data == payload, "received {} bytes, differs", data.len());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child got {} of {} bytes",
            data.len(),
            payload.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// [`deck_term::wait_detached`] returns once the detach worker of a terminal dropped with
/// input in flight has delivered it: the child holds every byte with no further waiting.
#[test]
fn wait_detached_joins_flush_worker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        "stty raw -echo; : > {r}; cat > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    std::thread::sleep(Duration::from_millis(200));
    let payload: Vec<u8> = b"0123456789abcdef".repeat(4096);
    t.write(&payload);
    drop(t);
    let start = Instant::now();
    deck_term::wait_detached(common::TIMEOUT);
    assert!(start.elapsed() < common::TIMEOUT, "worker not joined");
    let data = std::fs::read(&out).unwrap_or_default();
    assert!(
        data == payload,
        "child got {} of {} bytes",
        data.len(),
        payload.len()
    );
}

/// With mouse reporting on (as under tmux), Ctrl+click on a plain URL opens it and a right click
/// only arms the context menu: neither reaches the child as a mouse report.
#[test]
fn ctrl_click_opens_plain_url_and_right_click_stays_local() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("cat");
    let ready = dir.path().join("ready");
    let script = format!(
        r"stty raw -echo; printf '\033[?1000hhttps://ex.io/a(b) tail'; : > {r}; cat -v > {o}",
        r = sh_quote(&ready.display().to_string()),
        o = sh_quote(&out.display().to_string()),
    );
    let mut t = spawn_sh(&script, false);
    wait_file(&mut t, &ready, "child ready", |_| ready.exists());
    poll_until(&mut t, "url on screen", |_, t| {
        t.screen_text().lines[0]
            .starts_with("https://")
            .then_some(())
    });
    let ctx = egui::Context::default();
    let size = vec2(640.0, 400.0);
    let at = pos2(20.0, 12.0);
    let button = |button, pressed, modifiers| Event::PointerButton {
        pos: at,
        button,
        pressed,
        modifiers,
    };
    let run = |t: &mut deck_term::Terminal, events| {
        let mut url = None;
        let output = ctx.run_ui(raw_input(size, events), |ui| {
            url = TerminalView::new(t).show(ui).open_url;
        });
        output.drop_without_applying_deltas();
        url
    };
    run(&mut t, Vec::new());
    let opened = run(
        &mut t,
        vec![
            Event::PointerMoved(at),
            button(PointerButton::Primary, true, Modifiers::CTRL),
            button(PointerButton::Primary, false, Modifiers::CTRL),
        ],
    );
    assert_eq!(opened.as_deref(), Some("https://ex.io/a(b)"));
    run(
        &mut t,
        vec![
            button(PointerButton::Secondary, true, Modifiers::NONE),
            button(PointerButton::Secondary, false, Modifiers::NONE),
        ],
    );
    assert_eq!(t.context_link(), Some("https://ex.io/a(b)"));
    run(
        &mut t,
        vec![
            key(egui::Key::Z, Modifiers::NONE),
            Event::Text("z".into()),
            key(egui::Key::Enter, Modifiers::NONE),
        ],
    );
    let data = wait_file(&mut t, &out, "typed input", |d| d.ends_with(b"^M"));
    assert_eq!(String::from_utf8_lossy(&data), "z^M");
}
