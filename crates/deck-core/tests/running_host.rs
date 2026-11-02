//! `Mux::running` on the host backend: one live session reports the child's pid, cwd, start and
//! the memory of the daemon's tree (daemon + child). Needs the `omp-deck-host` binary in the test
//! profile dir (`cargo build -p deck-host`); otherwise it returns early with a note.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use deck_core::mux::{HostMux, Mux, MuxKind};

struct Cleanup(HostMux, &'static str);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.namespace().kill(self.1);
    }
}

fn host_exe() -> Option<PathBuf> {
    let profile_dir = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    let exe = profile_dir.join(format!("omp-deck-host{}", std::env::consts::EXE_SUFFIX));
    exe.is_file().then_some(exe)
}

#[test]
fn host_running_reports_child_and_daemon_tree_until_killed() {
    let Some(exe) = host_exe() else {
        eprintln!("skipping: omp-deck-host not built next to the test profile");
        return;
    };
    // SAFETY: this file holds a single test, so nothing else reads or writes the environment.
    unsafe { std::env::set_var("OMP_DECK_HOST_EXE", &exe) };
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    let host = HostMux::new(&format!("omp-deck-test-run-{}", std::process::id())).unwrap();
    let mux = Mux::Host(host.clone());
    let name = "omp-hostrun";
    let _cleanup = Cleanup(host.clone(), name);
    let spec = deck_host::SessionSpec {
        name: name.to_owned(),
        program: PathBuf::from("sleep"),
        args: vec!["30".to_owned()],
        cwd: work.clone(),
        env_set: Vec::new(),
        env_remove: Vec::new(),
        tsp: false,
    };
    let before = SystemTime::now() - Duration::from_secs(2);
    deck_host::start(host.namespace(), &spec, 80, 24).unwrap();

    let running = mux.running(dir.path());
    assert_eq!(running.len(), 1, "{running:?}");
    let r = &running[0];
    assert_eq!((r.name.as_str(), r.backend), (name, MuxKind::Host));
    assert_eq!(r.cwd.as_deref(), Some(work.as_path()));
    assert!(
        r.started
            .is_some_and(|s| s >= before && s <= SystemTime::now())
    );
    let pid = r.pid.expect("child pid");
    assert!(
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| c.starts_with("sleep"))
    );
    let mem = r.memory.expect("daemon tree memory");
    assert!(
        mem.processes >= 2 && mem.bytes > 0,
        "daemon + child: {mem:?}"
    );
    assert_eq!(mux.server_memory(), None);

    mux.kill(name).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !mux.running(dir.path()).is_empty() {
        assert!(Instant::now() < deadline, "session still listed");
        std::thread::sleep(Duration::from_millis(50));
    }
}
