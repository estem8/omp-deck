//! Runs the real `omp` binary, always against a throwaway agent dir (`PI_CODING_AGENT_DIR` is set
//! per command through `Omp::with_env`; the test process env is never touched).

use std::fs;
use std::path::{Path, PathBuf};

use deck_core::omp::{MoveKind, Omp, SettingKind};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};

/// omp's live-writer check is global by session id (across agent dirs), so tests that
/// pack/unpack the shared fixture session must not run concurrently.
static SESSION_LOCK: Mutex<()> = Mutex::new(());

fn session_lock() -> MutexGuard<'static, ()> {
    SESSION_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn sandboxed(agent: &Path) -> Option<Omp> {
    match Omp::detect() {
        Some(o) => Some(o.with_env("PI_CODING_AGENT_DIR", agent)),
        None => {
            eprintln!("omp not found; skipping real-omp integration test");
            None
        }
    }
}
/// An rpc omp that closes stdin and stdout but keeps running must not hang the caller: the
/// shutdown is bounded, the process killed, and its stderr reported.
#[cfg(unix)]
#[test]
fn an_rpc_omp_that_closes_its_pipes_but_stays_alive_is_killed_after_a_bounded_wait() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("omp");
    install_exec(
        &fake,
        "#!/bin/sh\necho boom >&2\nexec 0<&- 1>&-\nexec sleep 120\n",
    );
    let started = std::time::Instant::now();
    let err = Omp::at(&fake)
        .copy_session(&tmp.path().join("src.jsonl"), tmp.path())
        .unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

fn setting<'a>(cat: &'a deck_core::omp::SettingsCatalog, key: &str) -> &'a deck_core::omp::Setting {
    cat.tabs
        .iter()
        .flat_map(|t| &t.settings)
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("missing {key}"))
}

#[test]
fn set_and_reset_round_trip_in_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(omp) = sandboxed(&tmp.path().join("agent")) else {
        return;
    };
    let cat = omp.load_settings().unwrap();
    let cases = [
        ("autocompleteMaxVisible", json!(-5)),
        ("theme.dark", json!("  spaced  ")),
        ("symbolPreset", json!("nerd")),
        ("compaction.enabled", json!(false)),
        ("enabledModels", json!(["a/b", "c"])),
        ("modelRoles", json!({"default": "x/y"})),
    ];
    for (key, value) in &cases {
        let s = setting(&cat, key);
        assert_eq!(&omp.set_setting(s, value).unwrap(), value, "{key}");
    }
    let after = omp.load_settings().unwrap();
    for (key, value) in &cases {
        let s = setting(&after, key);
        assert_eq!(s.value.as_ref(), Some(value), "{key}");
        assert!(s.is_modified(), "{key}");
    }
    for (key, _) in &cases {
        omp.reset_setting(key).unwrap();
    }
    let reset = omp.load_settings().unwrap();
    for (key, _) in &cases {
        assert!(!setting(&reset, key).is_modified(), "{key}");
    }
}

#[test]
fn secret_set_returns_null_and_is_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(omp) = sandboxed(&tmp.path().join("agent")) else {
        return;
    };
    let cat = omp.load_settings().unwrap();
    let tok = setting(&cat, "searxng.token");
    assert!(tok.secret && !tok.is_modified());
    assert_eq!(
        omp.set_setting(tok, &json!("sandbox-only-token")).unwrap(),
        json!(null)
    );
    let after = omp.load_settings().unwrap();
    let tok = setting(&after, "searxng.token");
    assert!(tok.secret && tok.is_modified());
    assert!(!format!("{tok:?}").contains("sandbox-only-token"));
}

#[test]
fn invalid_value_is_rejected_with_omp_message() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(omp) = sandboxed(&tmp.path().join("agent")) else {
        return;
    };
    let cat = omp.load_settings().unwrap();
    let s = setting(&cat, "autocompleteMaxVisible");
    assert_eq!(s.kind, SettingKind::Number);
    // Bypass the pre-spawn validation to see omp's own error text through `Failed`.
    let mut forged = s.clone();
    forged.kind = SettingKind::String;
    let err = omp.set_setting(&forged, &json!("abc")).unwrap_err();
    assert_eq!(err.to_string(), "Invalid number: abc");
}

#[test]
fn move_session_relocates_and_rewrites_header_cwd() {
    let _guard = session_lock();
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/omp/session/2026-01-01T10-00-00-000Z_0198aa00bb11.jsonl");
    let src_dir = agent.join("sessions/-src");
    fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join(fixture.file_name().unwrap());
    fs::copy(&fixture, &src).unwrap();
    let dest = tmp.path().join("dest-project");
    fs::create_dir(&dest).unwrap();

    let out = omp.move_session(&src, &dest).unwrap();
    assert_eq!(out.kind, MoveKind::Replaced);
    assert!(!src.exists(), "original must be gone");
    assert!(out.new_path.starts_with(agent.join("sessions")));
    let text = fs::read_to_string(&out.new_path).unwrap();
    // omp may put a title record first; the header is the `session` record.
    let header = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "session")
        .unwrap();
    let cwd = header["cwd"].as_str().unwrap();
    assert_eq!(
        fs::canonicalize(cwd).unwrap(),
        fs::canonicalize(&dest).unwrap()
    );
    assert_eq!(header["id"], "0198aa00bb11");
}

#[test]
fn move_session_to_missing_dir_fails_and_keeps_original() {
    let _guard = session_lock();
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/omp/session/2026-01-01T10-00-00-000Z_0198aa00bb11.jsonl");
    let src_dir = agent.join("sessions/-src");
    fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join(fixture.file_name().unwrap());
    fs::copy(&fixture, &src).unwrap();
    let err = omp
        .move_session(&src, &tmp.path().join("does-not-exist"))
        .unwrap_err();
    assert!(err.to_string().contains("Directory not found"), "{err}");
    assert!(src.exists());
}

// ---- fork_at -----------------------------------------------------------------------------------

fn entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> String {
    let parent = parent.map_or("null".to_owned(), |p| format!("\"{p}\""));
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-03-01T10:00:0{}.000Z","message":{{"role":"{role}","content":[{{"type":"text","text":"{text}"}}],"timestamp":1}}}}"#,
        id.len() % 9
    )
}

/// A session under `<agent>/sessions/<dir>/` with header cwd `cwd`: model_change m0 → user u1 → assistant a1 →
/// user u2 → assistant a2.
fn write_session(agent: &Path, cwd: &Path) -> PathBuf {
    let dir = agent.join("sessions/-fork-proj");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("2026-03-01T10-00-00-000Z_01a00000-0000-7000-8000-000000000001.jsonl");
    let mut s = String::new();
    s.push_str(r#"{"type":"title","v":1,"title":"fork src","source":"user","updatedAt":"2026-03-01T10:00:00.000Z","pad":""}"#);
    s.push('\n');
    s.push_str(&format!(
        r#"{{"type":"session","version":3,"id":"01a00000-0000-7000-8000-000000000001","timestamp":"2026-03-01T10:00:00.000Z","cwd":{}}}"#,
        serde_json::to_string(&cwd.to_string_lossy()).unwrap()
    ));
    s.push('\n');
    s.push_str(
        r#"{"type":"model_change","id":"m0","parentId":null,"timestamp":"2026-03-01T10:00:00.000Z","model":"fake/fake-1","role":"default"}"#,
    );
    s.push('\n');
    for l in [
        entry("u1", Some("m0"), "user", "first"),
        entry("a1", Some("u1"), "assistant", "one"),
        entry("u2", Some("a1"), "user", "second"),
        entry("a2", Some("u2"), "assistant", "two"),
    ] {
        s.push_str(&l);
        s.push('\n');
    }
    fs::write(&file, s).unwrap();
    // Resuming restores the session's model: give the sandbox a (never contacted) one.
    fs::write(
        agent.join("models.yml"),
        "providers:\n  fake:\n    baseUrl: http://127.0.0.1:9/v1\n    apiKey: sk-fake\n    api: openai-completions\n    models:\n      - id: fake-1\n        name: Fake One\n        contextWindow: 100000\n        maxTokens: 4096\n",
    )
    .unwrap();
    fs::write(
        agent.join("config.yml"),
        "startup:\n  setupWizard: false\nmodelRoles:\n  default: fake/fake-1\n",
    )
    .unwrap();
    file
}

fn entry_ids(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "message")
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .collect()
}

/// omp joins `PI_CONFIG_DIR` onto the home directory: the sandbox is the home and `cfg` its
/// config directory (an absolute value is an invalid path on Windows).
fn isolate_home(omp: Omp, home: &Path) -> Omp {
    omp.with_env("PI_CONFIG_DIR", "cfg")
        .with_env("HOME", home)
        .with_env("USERPROFILE", home)
}

#[test]
fn fork_at_creates_a_new_file_ending_at_the_entry_and_keeps_the_source() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let cwd = tmp.path().join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let omp = isolate_home(omp, tmp.path());
    let src = write_session(&agent, &cwd);
    let before = fs::read(&src).unwrap();

    let new = omp.fork_at(&src, "a1").unwrap();
    assert_ne!(new, src);
    assert!(new.is_file(), "{}", new.display());
    assert_eq!(entry_ids(&new), ["u1", "a1"]);
    assert_eq!(fs::read(&src).unwrap(), before, "source untouched");

    let err = omp.fork_at(&src, "no-such-entry").unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn copy_session_copies_the_whole_session_with_artifacts_into_the_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let cwd = tmp.path().join("proj");
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&dest).unwrap();
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let omp = isolate_home(omp, tmp.path());
    let src = write_session(&agent, &cwd);
    let artifacts = src.with_extension("");
    fs::create_dir_all(artifacts.join("artifacts")).unwrap();
    fs::write(artifacts.join("artifacts/a.txt"), "hello").unwrap();
    let before = fs::read(&src).unwrap();

    let new = omp.copy_session(&src, &dest).unwrap();
    assert_ne!(new, src);
    assert!(new.is_file(), "{}", new.display());
    assert_eq!(
        entry_ids(&new),
        ["u1", "a1", "u2", "a2"],
        "the whole session"
    );
    assert_eq!(fs::read(&src).unwrap(), before, "source untouched");
    assert!(src.is_file() && artifacts.join("artifacts/a.txt").is_file());
    assert_eq!(
        fs::read_to_string(new.with_extension("").join("artifacts/a.txt")).unwrap(),
        "hello",
        "artifacts are copied"
    );

    let header: serde_json::Value = fs::read_to_string(&new)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "session")
        .unwrap();
    assert_eq!(header["cwd"].as_str(), dest.to_str());
    assert_eq!(
        header["parentSession"].as_str(),
        Some("01a00000-0000-7000-8000-000000000001"),
        "omp records the parent's session id"
    );
    assert_ne!(
        header["id"].as_str(),
        Some("01a00000-0000-7000-8000-000000000001")
    );

    // Copying again makes another fresh copy.
    let again = omp.copy_session(&src, &dest).unwrap();
    assert_ne!(again, new);
}

/// Without credentials for the session's model omp aborts start-up *after* `--fork` wrote the
/// copy: the copy must be reported (not "failed"), and a retry must not be needed.
#[test]
fn copy_session_finds_the_fork_omp_wrote_before_failing_to_restore_the_model() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let cwd = tmp.path().join("proj");
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&dest).unwrap();
    let Some(omp) = sandboxed(&agent) else {
        return;
    };
    let omp = isolate_home(omp, tmp.path());
    let src = write_session(&agent, &cwd);
    // No provider for `fake/fake-1`: nothing to restore the model with.
    fs::remove_file(agent.join("models.yml")).unwrap();
    let artifacts = src.with_extension("");
    fs::create_dir_all(artifacts.join("artifacts")).unwrap();
    fs::write(artifacts.join("artifacts/a.txt"), "hello").unwrap();

    let new = omp.copy_session(&src, &dest).unwrap();
    assert_ne!(new, src);
    assert_eq!(entry_ids(&new), ["u1", "a1", "u2", "a2"]);
    assert_eq!(
        fs::read_to_string(new.with_extension("").join("artifacts/a.txt")).unwrap(),
        "hello",
        "artifacts are copied"
    );
    let top_level: Vec<_> = fs::read_dir(new.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .collect();
    assert_eq!(top_level.len(), 1, "exactly one new session file");
}

#[cfg(unix)]
#[test]
fn copy_session_failure_without_a_fork_reports_only_the_error_line() {
    let tmp = tempfile::tempdir().unwrap();
    let dies = tmp.path().join("dies.sh");
    install_exec(
        &dies,
        "#!/bin/sh\nprintf '27217 |   throw new Error(`x`)\\n              ^\\nerror: Could not restore model a/b\\n      at mY (/$bunfs/root/x.js:1:2)\\n' >&2\nexit 1\n",
    );
    let e = Omp::at(dies)
        .with_env("PI_CODING_AGENT_DIR", tmp.path().join("agent"))
        .copy_session(Path::new("/a.jsonl"), Path::new("/b"))
        .unwrap_err()
        .to_string();
    assert_eq!(e, "Could not restore model a/b");
}

#[cfg(unix)]
#[test]
fn copy_session_runs_omp_fork_with_absolute_paths_and_reads_the_new_file_from_get_state() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("omp.sh");
    install_exec(
        &script,
        r#"#!/bin/sh
echo "$@" > "$(dirname "$0")/argv"
read a
echo "$a" > "$(dirname "$0")/request"
echo '{"type":"ready"}'
echo '{"id":"2","type":"response","command":"get_state","success":true,"data":{"sessionFile":"/new/copy.jsonl"}}'
cat >/dev/null
"#,
    );
    let omp = Omp::at(script);
    let got = omp
        .copy_session(Path::new("rel/src.jsonl"), Path::new("rel/dest"))
        .unwrap();
    assert_eq!(got, Path::new("/new/copy.jsonl"));
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        fs::read_to_string(tmp.path().join("argv")).unwrap().trim(),
        format!(
            "--mode rpc --fork {} --cwd {}",
            cwd.join("rel/src.jsonl").display(),
            cwd.join("rel/dest").display()
        )
    );
    let req: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("request")).unwrap()).unwrap();
    assert_eq!(
        req["type"], "get_state",
        "no fork request: --fork already copied"
    );

    let dies = tmp.path().join("dies.sh");
    install_exec(
        &dies,
        "#!/bin/sh\necho 'Error: destination missing' >&2\nexit 1\n",
    );
    let e = Omp::at(dies)
        .copy_session(Path::new("/a.jsonl"), Path::new("/b"))
        .unwrap_err()
        .to_string();
    assert_eq!(e, "destination missing");
}

#[cfg(unix)]
#[test]
fn fork_at_speaks_the_rpc_protocol_and_surfaces_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let src = write_session(&tmp.path().join("agent"), &cwd);
    let script = |name: &str, body: &str| {
        let p = tmp.path().join(name);
        install_exec(&p, &format!("#!/bin/sh\n{body}\n"));
        Omp::at(p)
    };

    // success: answers fork then get_state; the new file is reported relative to the process cwd
    let ok = script(
        "ok.sh",
        r#"echo '{"type":"ready"}'
read a; read b
echo "$a" > "$(pwd)/request1.json"
echo '{"id":"1","type":"response","command":"fork","success":true,"data":{"cancelled":false}}'
echo "{\"id\":\"2\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,\"data\":{\"sessionFile\":\"$(pwd)/forked.jsonl\"}}"
cat >/dev/null"#,
    );
    let got = ok.fork_at(&src, "a1").unwrap();
    assert_eq!(got, cwd.join("forked.jsonl"), "runs in the header cwd");
    let req: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cwd.join("request1.json")).unwrap()).unwrap();
    assert_eq!(
        (req["type"].as_str(), req["entryId"].as_str()),
        (Some("fork"), Some("a1"))
    );

    // rpc-level failure keeps omp's text
    let locked = script(
        "locked.sh",
        r#"read a
echo '{"id":"1","type":"response","command":"fork","success":false,"error":"Error: session is held by pid 4242"}'
cat >/dev/null"#,
    );
    let e = locked.fork_at(&src, "a1").unwrap_err().to_string();
    assert_eq!(e, "session is held by pid 4242");

    // cancelled fork
    let cancelled = script(
        "cancelled.sh",
        r#"read a
echo '{"id":"1","type":"response","command":"fork","success":true,"data":{"cancelled":true}}'
cat >/dev/null"#,
    );
    assert!(
        cancelled
            .fork_at(&src, "a1")
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );

    // the process dies at startup (e.g. lease held): stderr is the message
    let dies = script(
        "dies.sh",
        "echo 'Error: session locked by a live omp' >&2\nexit 1",
    );
    let e = dies.fork_at(&src, "a1").unwrap_err().to_string();
    assert_eq!(e, "session locked by a live omp");
}

#[cfg(unix)]
#[test]
fn relocate_session_sends_move_from_dest_and_returns_the_new_file() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&wt).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let src = write_session(&tmp.path().join("agent-old"), &wt);
    // What omp leaves behind after `/move`: the same file name in the repo's bucket.
    let moved = write_session(&tmp.path().join("agent"), &repo);
    let script = |name: &str, body: &str| {
        let p = tmp.path().join(name);
        install_exec(&p, &format!("#!/bin/sh\n{body}\n"));
        Omp::at(p).with_env("PI_CODING_AGENT_DIR", tmp.path().join("agent"))
    };

    let ok = script(
        "ok.sh",
        &format!(
            r#"echo "$@" > "{0}/argv"
pwd > "{0}/pwd"
read a; read b
echo "$a" > "{0}/request1.json"
echo "$b" > "{0}/request2.json"
echo '{{"type":"ready"}}'
echo '{{"id":"1","type":"response","command":"prompt","success":true,"data":{{"agentInvoked":false}}}}'
echo '{{"id":"2","type":"response","command":"get_state","success":true,"data":{{"sessionFile":"{1}"}}}}'
cat >/dev/null"#,
            tmp.path().display(),
            moved.display()
        ),
    );
    assert_eq!(ok.relocate_session(&src, &repo).unwrap(), moved);
    assert_eq!(
        fs::read_to_string(tmp.path().join("argv")).unwrap().trim(),
        format!("--mode rpc --resume {} --no-extensions", src.display())
    );
    assert_eq!(
        Path::new(fs::read_to_string(tmp.path().join("pwd")).unwrap().trim()),
        fs::canonicalize(&repo).unwrap()
    );
    let req: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("request1.json")).unwrap())
            .unwrap();
    assert_eq!(
        (req["type"].as_str(), req["message"].as_str()),
        (
            Some("prompt"),
            Some(format!("/move {}", repo.display()).as_str())
        )
    );

    // get_state names a stale (missing) file: the file is found in the destination's bucket.
    let stale = script(
        "stale.sh",
        r#"read a; read b
echo '{"id":"1","type":"response","command":"prompt","success":true,"data":{}}'
echo '{"id":"2","type":"response","command":"get_state","success":true,"data":{"sessionFile":"/gone/old.jsonl"}}'
cat >/dev/null"#,
    );
    assert_eq!(stale.relocate_session(&src, &repo).unwrap(), moved);

    // omp's refusal text is the error.
    let refused = script(
        "refused.sh",
        r#"read a
echo '{"id":"1","type":"response","command":"prompt","success":false,"error":"Error: Cannot move while streaming"}'
cat >/dev/null"#,
    );
    assert_eq!(
        refused
            .relocate_session(&src, &repo)
            .unwrap_err()
            .to_string(),
        "Cannot move while streaming"
    );
}

#[cfg(unix)]
#[test]
fn relocate_session_retries_a_live_writer_and_skips_omp_when_already_there() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&wt).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let src = write_session(&tmp.path().join("agent-old"), &wt);
    let moved = write_session(&tmp.path().join("agent"), &repo);
    let runs = tmp.path().join("runs");
    let flaky = tmp.path().join("flaky.sh");
    install_exec(
        &flaky,
        &format!(
            r#"#!/bin/sh
echo x >> "{0}"
if [ "$(wc -l < "{0}")" -lt 3 ]; then
  echo 'Error: session is open in another live omp process (pid 1)' >&2
  exit 1
fi
read a; read b
echo '{{"id":"1","type":"response","command":"prompt","success":true,"data":{{}}}}'
echo '{{"id":"2","type":"response","command":"get_state","success":true,"data":{{"sessionFile":"{1}"}}}}'
cat >/dev/null
"#,
            runs.display(),
            moved.display()
        ),
    );
    let omp = Omp::at(flaky);
    assert_eq!(omp.relocate_session(&src, &repo).unwrap(), moved);
    assert_eq!(fs::read_to_string(&runs).unwrap().lines().count(), 3);

    // A held lease that never frees up surfaces omp's text after ~3 s.
    let stuck = tmp.path().join("stuck.sh");
    install_exec(
        &stuck,
        "#!/bin/sh\necho 'Error: session is open in another live omp process' >&2\nexit 1\n",
    );
    let e = Omp::at(stuck).relocate_session(&src, &repo).unwrap_err();
    assert!(e.to_string().contains("another live omp process"), "{e}");

    // Header cwd already the destination: omp is not run at all.
    let never = tmp.path().join("never.sh");
    install_exec(
        &never,
        &format!("#!/bin/sh\ntouch \"{}/ran\"\n", tmp.path().display()),
    );
    assert_eq!(
        Omp::at(never).relocate_session(&moved, &repo).unwrap(),
        moved
    );
    assert!(!tmp.path().join("ran").exists());
}

#[test]
fn relocate_session_rejects_bad_destinations_and_files_without_running_omp() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    fs::create_dir_all(&wt).unwrap();
    let src = write_session(&tmp.path().join("agent"), &wt);
    let omp = Omp::at(tmp.path().join("no-such-omp"));
    let err = |file: &Path, dest: &Path| omp.relocate_session(file, dest).unwrap_err().to_string();
    assert!(err(&src, Path::new("rel/dir")).contains("absolute"));
    assert!(err(&src, &tmp.path().join("missing")).contains("not a directory"));
    assert!(err(&src, &PathBuf::from(format!("{}\nx", tmp.path().display()))).contains("control"));
    assert!(err(&wt.join("x.txt"), tmp.path()).contains("Not a session file"));
    assert!(err(&wt, tmp.path()).contains("Not a session file"));
}

#[cfg(unix)]
#[test]
fn unpack_session_runs_detached_from_the_terminal_and_reports_the_mode() {
    use deck_core::omp::MoveKind;
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let seen = tmp.path().join("seen");
    let script = tmp.path().join("omp.sh");
    // Records argv and the terminal variables, closes nothing: stdin must already be null.
    install_exec(
        &script,
        &format!(
            "#!/bin/sh\n{{ echo \"$@\"; for v in TMUX TMUX_PANE ZELLIJ_PANE_ID CMUX_SURFACE_ID \
             KITTY_WINDOW_ID WEZTERM_PANE TERM_SESSION_ID WT_SESSION KEEP; do eval \"echo $v=\\${{$v-unset}}\"; done; \
             if [ -t 0 ]; then echo tty-stdin; fi; }} > {}\n\
             echo /agent/sessions/x/new.jsonl\n\
             echo 'Unpacked session (abc).' >&2\n",
            seen.display()
        ),
    );
    let mut omp = Omp::at(&script);
    for v in [
        "TMUX",
        "TMUX_PANE",
        "ZELLIJ_PANE_ID",
        "CMUX_SURFACE_ID",
        "KITTY_WINDOW_ID",
        "WEZTERM_PANE",
        "TERM_SESSION_ID",
        "WT_SESSION",
    ] {
        omp = omp.with_env(v, "terminal-value");
    }
    let omp = omp.with_env("KEEP", "yes");
    let tar = tmp.path().join("s.tar");
    let out = omp.unpack_session(&tar, &cwd).unwrap();
    assert_eq!(out.kind, MoveKind::Created);
    assert_eq!(
        out.new_path,
        std::path::PathBuf::from("/agent/sessions/x/new.jsonl")
    );
    let seen = fs::read_to_string(&seen).unwrap();
    let mut lines = seen.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!("session unpack {} --cwd {}", tar.display(), cwd.display())
    );
    let rest: Vec<&str> = lines.collect();
    assert_eq!(
        rest,
        [
            "TMUX=unset",
            "TMUX_PANE=unset",
            "ZELLIJ_PANE_ID=unset",
            "CMUX_SURFACE_ID=unset",
            "KITTY_WINDOW_ID=unset",
            "WEZTERM_PANE=unset",
            "TERM_SESSION_ID=unset",
            "WT_SESSION=unset",
            "KEEP=yes",
        ]
    );

    let failing = tmp.path().join("fail.sh");
    install_exec(
        &failing,
        "#!/bin/sh\necho 'error: Bad session archive' >&2\nexit 1\n",
    );
    let err = Omp::at(&failing).unpack_session(&tar, &cwd).unwrap_err();
    assert!(err.to_string().contains("Bad session archive"), "{err}");
}

#[cfg(unix)]
#[test]
fn pack_session_runs_detached_and_returns_the_archive_path() {
    let tmp = tempfile::tempdir().unwrap();
    let seen = tmp.path().join("seen");
    let script = tmp.path().join("omp.sh");
    install_exec(
        &script,
        &format!(
            "#!/bin/sh\n{{ echo \"$@\"; for v in TMUX TMUX_PANE WT_SESSION KEEP; do eval \"echo $v=\\${{$v-unset}}\"; done; \
             if [ -t 0 ]; then echo tty-stdin; fi; }} > {}\n\
             echo 'Packed session.' >&2\n",
            seen.display()
        ),
    );
    let omp = Omp::at(&script)
        .with_env("TMUX", "terminal-value")
        .with_env("WT_SESSION", "terminal-value")
        .with_env("KEEP", "yes");
    let (src, out) = (tmp.path().join("a.jsonl"), tmp.path().join("saved.tar"));
    assert_eq!(omp.pack_session(&src, &out).unwrap(), out);
    assert_eq!(
        fs::read_to_string(&seen)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            format!("session pack {} {}", src.display(), out.display()),
            "TMUX=unset".into(),
            "TMUX_PANE=unset".into(),
            "WT_SESSION=unset".into(),
            "KEEP=yes".into(),
        ]
    );

    // omp's own refusal text comes back as the error.
    let busy = tmp.path().join("busy.sh");
    install_exec(
        &busy,
        "#!/bin/sh\necho 'error: Cannot pack x: another live omp process writes this session. Exit it first.' >&2\nexit 1\n",
    );
    let err = Omp::at(&busy).pack_session(&src, &out).unwrap_err();
    assert!(
        err.to_string().contains("another live omp process"),
        "{err}"
    );
}

#[cfg(unix)]
/// Creates executable `dest` with `content` without ever holding a write fd to the executed
/// inode: a concurrent fork in a sibling test thread would otherwise leak it and exec would fail
/// with ETXTBSY. The bytes go to a staging file; `install` (external process) makes the copy.
fn install_exec(dest: &Path, content: &str) {
    let mut stage = dest.as_os_str().to_owned();
    stage.push(".src");
    let stage = std::path::PathBuf::from(stage);
    fs::write(&stage, content).unwrap();
    let status = std::process::Command::new("install")
        .args(["-m", "755"])
        .arg(&stage)
        .arg(dest)
        .status()
        .unwrap();
    assert!(status.success());
}
