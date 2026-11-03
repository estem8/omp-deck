//! A real omp in a sandbox, driven like OmpDeck's chat view drives it: launched through the
//! session host as a TSP terminal (`tsp: true`) with the control extension loaded, a scripted
//! model ([`fake_model`]) behind it, prompts written as TSP `send` events, the document read
//! with `tsp_peek`. HOME, `PI_*` and `XDG_*` of the omp process point into a temp dir; nothing
//! under the real `~/.omp` is touched. Used by `tests/ctl_tree.rs`.
#![allow(dead_code)]

#[path = "../../../deck-service/tests/support/fake_model.rs"]
pub mod fake_model;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::ctl::{
    CtlClient, CtlCommand, CtlReply, ctl_dir, ctl_env_with_key, install_extension, prepare_ctl_dir,
    prune_ctl_dirs, with_ctl,
};
use deck_core::omp::Omp;
use deck_host::{AttachParams, HostEvent, Namespace, Palette, SessionSpec};
use deck_tsp::terminal::{InputEncoding, encode_event};
use deck_tsp::wire::Event;
use serde_json::Value;

pub use fake_model::{FakeModel, Reply};

/// omp's `config.yml` without a first-run wizard or collab, default model = the fake.
const OMP_CONFIG: &str = "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\n\
                          modelRoles:\n  default: fake/fake-1\n";
/// How long omp may take to show a surface or finish a turn.
pub const TURN: Duration = Duration::from_secs(60);
pub const SESSION: &str = "live1";

/// Polls `f` until it returns `Some`.
pub fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `omp-deck-host` next to the test binaries, built when `cargo test` did not.
fn ensure_host_exe() {
    let exe = format!("omp-deck-host{}", std::env::consts::EXE_SUFFIX);
    let profile_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent().map(Path::to_path_buf))
        .expect("test executable dir");
    let beside = profile_dir.join(&exe);
    if !beside.is_file() {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "deck-host", "--bin", "omp-deck-host"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p deck-host");
    }
    assert!(beside.is_file(), "{} exists", beside.display());
    // SAFETY: set once, before this process starts any thread that reads the environment.
    unsafe { std::env::set_var(deck_host::HOST_EXE_ENV, &beside) };
}

/// Every node of the open inline surface, depth first, as `(id, kind, props)`.
pub fn nodes(doc: &Value) -> Vec<(String, String, Value)> {
    fn walk(n: &Value, out: &mut Vec<(String, String, Value)>) {
        out.push((
            n["id"].as_str().unwrap_or_default().to_owned(),
            n["k"].as_str().unwrap_or_default().to_owned(),
            n["p"].clone(),
        ));
        for c in n["c"].as_array().into_iter().flatten() {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for s in doc["surfaces"].as_array().into_iter().flatten() {
        if s["open"] == true {
            walk(&s["root"], &mut out);
        }
    }
    out
}

/// Text of the first node whose text starts with `head`.
pub fn text_starting(doc: &Value, head: &str) -> Option<String> {
    nodes(doc)
        .into_iter()
        .filter_map(|(_, _, p)| p["text"].as_str().map(str::to_owned))
        .find(|t| t.starts_with(head))
}

pub struct Live {
    pub root: PathBuf,
    pub home: PathBuf,
    pub agent: PathBuf,
    pub proj: PathBuf,
    pub ns: String,
    pub omp: Omp,
    pub model: FakeModel,
    client: Option<CtlClient>,
    namespace: Namespace,
    ctl: PathBuf,
}

impl Live {
    /// Starts omp (`resume`: `--resume <file>`) and waits until the control client is connected
    /// with a snapshot. `None` when omp is not installed (the test is skipped).
    pub fn start(
        tag: &str,
        script: impl FnMut(&Value) -> Reply + Send + 'static,
        seed: impl FnOnce(&Live),
        resume: impl FnOnce(&Live) -> Option<PathBuf>,
    ) -> Option<Live> {
        let omp_exe = std::env::var_os("OMPDECK_TEST_OMP").map(PathBuf::from)?;
        ensure_host_exe();
        let ns = format!("omp-deck-test-{tag}-{}", std::process::id());
        let root = std::env::temp_dir().join(&ns);
        let _ = std::fs::remove_dir_all(&root);
        let (home, proj) = (root.join("home"), root.join("home/proj"));
        let agent = home.join(".omp/agent");
        for d in [&agent, &proj, &root.join("data"), &home.join(".config")] {
            std::fs::create_dir_all(d).expect("sandbox dir");
        }
        std::fs::write(agent.join("config.yml"), OMP_CONFIG).expect("omp config");
        let model = FakeModel::start(script);
        std::fs::write(agent.join("models.yml"), model.models_yml()).expect("models.yml");

        let mut env: Vec<(String, String)> = vec![
            ("HOME".into(), home.display().to_string()),
            ("USERPROFILE".into(), home.display().to_string()),
            ("PI_CODING_AGENT_DIR".into(), agent.display().to_string()),
            ("PI_NOTIFICATIONS".into(), "off".into()),
        ];
        if cfg!(unix) {
            env.push((
                "XDG_CONFIG_HOME".into(),
                home.join(".config").display().to_string(),
            ));
            env.push((
                "XDG_DATA_HOME".into(),
                home.join(".local/share").display().to_string(),
            ));
            env.push((
                "XDG_CACHE_HOME".into(),
                home.join(".cache").display().to_string(),
            ));
            env.push((
                "XDG_STATE_HOME".into(),
                home.join(".local/state").display().to_string(),
            ));
        }
        let mut omp = Omp::at(&omp_exe);
        for (k, v) in &env {
            omp = omp.with_env(k, v);
        }
        let namespace = Namespace::new(&ns).expect("namespace");
        let ctl = ctl_dir(&ns, SESSION);
        let mut live = Live {
            root,
            home,
            agent,
            proj,
            ns,
            omp,
            model,
            client: None,
            namespace,
            ctl: ctl.clone(),
        };
        seed(&live);
        let resume = resume(&live);

        let data = live.root.join("data");
        let key_file = data.join("ctl.key");
        let ext = install_extension(&data).expect("install ctl extension");
        prepare_ctl_dir(&ctl).expect("ctl dir");
        let mut launch_env = env;
        launch_env.extend(
            ctl_env_with_key(&ctl, &key_file)
                .expect("ctl env")
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_string_lossy().into_owned())),
        );
        let mut argv = vec![omp_exe.display().to_string()];
        if let Some(file) = &resume {
            argv.extend(["--resume".to_owned(), file.display().to_string()]);
        }
        let argv = with_ctl(&argv, &ext);
        let (mut env_set, env_remove) = deck_core::mux::host_env(SESSION, &launch_env, false);
        env_set.retain(|(k, _)| k != "PI_NOTIFICATIONS");
        env_set.push(("PI_NOTIFICATIONS".into(), "off".into()));
        let (program, args) = argv.split_first().expect("argv");
        let spec = SessionSpec {
            name: SESSION.to_owned(),
            program: PathBuf::from(program),
            args: args.to_vec(),
            cwd: live.proj.clone(),
            env_set,
            env_remove,
            tsp: true,
        };
        deck_host::start(&live.namespace, &spec, 100, 30).expect("start omp");
        live.client = Some(CtlClient::spawn_with_key(ctl, key_file, || {}));
        let client = live.client();
        wait_for("the ctl snapshot", TURN, || {
            client.latest().filter(|_| client.connected())
        });
        wait_for("a sendable composer", TURN, || live.composer());
        Some(live)
    }

    pub fn client(&self) -> &CtlClient {
        self.client.as_ref().expect("client started")
    }

    pub fn peek(&self) -> Value {
        deck_host::tsp_peek(&self.namespace, SESSION).expect("tsp_peek")
    }

    /// `(surface id, editor node id)` once omp shows a sendable composer.
    pub fn composer(&self) -> Option<(String, String)> {
        let doc = self.peek();
        let sf = doc["surfaces"]
            .as_array()?
            .iter()
            .find(|s| s["mode"] == "inline" && s["open"] == true)?["id"]
            .as_str()?
            .to_owned();
        nodes(&doc)
            .into_iter()
            .find(|(_, k, p)| k == "editor" && p["sendable"] == true)
            .map(|(id, _, _)| (sf, id))
    }

    /// Writes TSP events through a short attach (what the chat view does).
    pub fn events(&self, events: &[Event]) {
        let params = AttachParams {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 16,
            history_lines: 0,
            palette: Palette::default(),
        };
        let attached = deck_host::attach(&self.namespace, SESSION, params).expect("attach");
        let (mut rx, tx) = attached.split();
        let reader = std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                if matches!(event, HostEvent::Closed | HostEvent::Exited(_)) {
                    break;
                }
            }
        });
        for e in events {
            tx.input(&encode_event(e, InputEncoding::Apc))
                .expect("write event");
            std::thread::sleep(Duration::from_millis(50));
        }
        tx.detach().expect("detach");
        reader.join().expect("reader thread");
    }

    /// Submits `text` through the composer and waits until omp is idle again.
    pub fn send(&self, text: &str) {
        let (sf, id) = wait_for("the composer", TURN, || self.composer());
        self.events(&[Event::Send {
            sf,
            id,
            text: text.to_owned(),
        }]);
    }

    /// Sends `cmd` over the control link and waits for its reply.
    pub fn ctl(&self, cmd: CtlCommand, timeout: Duration) -> CtlReply {
        let rid = self.client().send(cmd);
        wait_for("a ctl reply", timeout, || {
            self.client()
                .take_replies()
                .into_iter()
                .find(|(r, _)| *r == rid)
                .map(|(_, reply)| reply)
        })
    }

    /// The session file omp reports.
    pub fn session_file(&self) -> PathBuf {
        self.client()
            .latest()
            .and_then(|s| s.session_file.clone())
            .expect("a session file")
    }

    pub fn main_idle(&self) -> bool {
        self.client()
            .latest()
            .is_some_and(|s| s.main_idle == Some(true))
    }

    /// Entries of the live session file, parsed.
    pub fn entries(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.session_file()).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        for name in self.namespace.list() {
            let _ = self.namespace.kill(&name);
        }
        prune_ctl_dirs(&self.ns, &std::collections::HashSet::new());
        let _ = std::fs::remove_dir_all(&self.ctl);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
