//! The gateway's worktree flows against real git repositories and the real gateway process:
//! making a worktree at its default location with its merge target recorded, the repository's
//! worktree view, retiring after a regular merge and after a squash merge alike, parking a
//! pushed, unmerged branch (removing it is refused), continuing a parked branch in the main
//! checkout, switching back and continuing it in a new worktree, the finish flow's refusal
//! without a session and its cancel while waiting on one, and a finish that ends in removing
//! the clean, merged worktree (regular and squash merge) while its session moves out and keeps
//! running (a real omp answered by a scripted model, [`support::fake_model`]).
//!
//! Everything runs in a private HOME/XDG tree under a unique `omp-deck-test-*` namespace (backend
//! sockets included); the real omp store and the real `omp-deck` tmux server are never touched.
//! Skipped (with a message) when omp or git is not installed.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::config::AppConfig;
use deck_core::worktree::Worktree;
use deck_service::controller::SessionTarget;
use deck_service::gateway::Profile;
use deck_service::gateway::client::{GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{
    Answer, ContinueIn, ControllerInfo, Done, GatewayError, LaunchSpec, Mutation, Outcome, Query,
    Retirement, Role, SessionRole, Topic, TopicValue, WorktreeFlow, WorktreesView,
};
use support::fake_model::{self as fm, FakeModel, Reply};

const GATEWAY: &str = env!("CARGO_BIN_EXE_omp-deck-gateway");
const WAIT: Duration = Duration::from_secs(60);

struct Sandbox {
    root: PathBuf,
    ns: String,
    profile: Profile,
}

impl Sandbox {
    fn new() -> Sandbox {
        let ns = format!("omp-deck-test-wt-{}", std::process::id());
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
            "worktrees",
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
        let envs: [(&str, PathBuf); 9] = [
            ("HOME", root.join("home")),
            ("USERPROFILE", root.join("home")),
            ("XDG_CONFIG_HOME", root.join("config")),
            ("XDG_DATA_HOME", root.join("data")),
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_RUNTIME_DIR", root.join("run")),
            ("PI_CODING_AGENT_DIR", agent.clone()),
            // Managed worktrees inside the sandbox.
            ("OMP_WORKTREE_DIR", root.join("worktrees")),
            ("GIT_CONFIG_GLOBAL", root.join("home/.gitconfig")),
        ];
        // SAFETY: the only test of this binary; set before any thread exists.
        unsafe {
            for (k, v) in envs {
                std::env::set_var(k, v);
            }
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::set_var("OMP_DECK_HOST_NS", &ns);
            std::env::set_var("OMP_DECK_TMUX_SOCKET", &ns);
            std::env::set_var("PI_NOTIFICATIONS", "off");
            // Never the desktop's session bus: the owner's notifier must not pop up for test sessions.
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", root.join("run/no-bus").display()),
            );
            let host = PathBuf::from(GATEWAY).with_file_name(if cfg!(windows) {
                "omp-deck-host.exe"
            } else {
                "omp-deck-host"
            });
            if host.is_file() {
                std::env::set_var("OMP_DECK_HOST_EXE", host);
            }
        }
        std::fs::write(
            root.join("home/.gitconfig"),
            "[user]\n\tname = Deck Test\n\temail = deck@test.invalid\n[init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n",
        )
        .expect("git config");
        std::fs::create_dir_all(&agent).expect("agent dir");
        // Sessions use the scripted model (its `models.yml` is written once it serves).
        std::fs::write(
            agent.join("config.yml"),
            format!(
                "startup:\n  setupWizard: false\ncollab:\n  autoStart: off\nmodelRoles:\n  default: {}\n",
                fm::MODEL
            ),
        )
        .expect("omp config");
        let profile = Profile {
            host_namespace: ns.clone(),
            settings: root.join("config/omp-deck/state.json"),
            sessions_root: agent.join("sessions"),
        };
        Sandbox { root, ns, profile }
    }

    fn client(&self) -> GatewayClient {
        let mut opts = GatewayOptions::new(self.profile.clone(), Role::Gui, Arc::new(|| {}));
        opts.gateway_exe = Some(PathBuf::from(GATEWAY));
        GatewayClient::start(opts)
    }

    fn owner_pid(&self) -> Option<u32> {
        let out = Command::new(GATEWAY)
            .args(["status", "--json", "--ns", &self.ns, "--settings"])
            .arg(&self.profile.settings)
            .arg("--sessions-root")
            .arg(&self.profile.sessions_root)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v["status"]["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Our own test backend only (never the default tmux server or the real namespace).
        if !cfg!(windows) {
            let _ = Command::new("tmux")
                .args(["-L", &self.ns, "kill-server"])
                .output();
        }
        if let Some(pid) = self.owner_pid() {
            let _ = if cfg!(windows) {
                Command::new("taskkill")
                    .args(["/F", "/PID", &pid.to_string()])
                    .output()
            } else {
                Command::new("kill").arg(pid.to_string()).output()
            };
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn commit(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).expect("write file");
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", &format!("change {file}")]);
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(repo)
        .output()
        .is_ok_and(|o| o.status.success())
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

/// Runs `op` to its end: the outcome, `Err` for a refusal at admission.
fn run(client: &GatewayClient, op: Mutation) -> Result<Outcome, GatewayError> {
    let m = client.mutate(op);
    m.accepted.wait(WAIT).expect("answered in time")?;
    m.outcome
        .wait(Duration::from_secs(120))
        .expect("outcome in time")
}

fn done(client: &GatewayClient, op: Mutation) -> Done {
    let what = format!("{op:?}");
    match run(client, op) {
        Ok(Outcome::Done(d)) => *d,
        other => panic!("{what}: {other:?}"),
    }
}

fn failure(client: &GatewayClient, op: Mutation) -> String {
    let what = format!("{op:?}");
    match run(client, op) {
        Ok(Outcome::Failed(e)) => e,
        Err(GatewayError::Refused(e) | GatewayError::Failed(e)) => e,
        other => panic!("{what} should fail: {other:?}"),
    }
}

fn config(client: &GatewayClient) -> Arc<AppConfig> {
    match client.query(Query::Topic(Topic::Settings)).wait(WAIT) {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Settings(s) => s.config,
            other => panic!("settings value: {other:?}"),
        },
        other => panic!("settings query: {other:?}"),
    }
}

/// The repository view of `dir` once looked at (followed from here on; the subscription lands
/// asynchronously).
fn view(client: &GatewayClient, dir: &Path) -> WorktreesView {
    let topic = Topic::Worktrees(dir.to_path_buf());
    client.subscribe(topic.clone());
    wait_for("the worktrees view", WAIT, || {
        match client.query(Query::Topic(topic.clone())).wait(WAIT) {
            Some(Ok(Answer::Topic(v))) => match *v {
                TopicValue::Worktrees(v) => v.repo.is_some().then_some(*v),
                other => panic!("worktrees value: {other:?}"),
            },
            Some(Err(GatewayError::Refused(_))) => None,
            other => panic!("worktrees query: {other:?}"),
        }
    })
}

fn verdict(client: &GatewayClient, wt: &Worktree) -> Retirement {
    match client.query(Query::WorktreeVerdict(wt.clone())).wait(WAIT) {
        Some(Ok(Answer::WorktreeVerdict(v))) => v.verdict,
        other => panic!("verdict query: {other:?}"),
    }
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

/// A new worktree of `repo` on `branch` at the default location (not opened).
fn new_worktree(client: &GatewayClient, repo: &Path, branch: &str) -> Worktree {
    let dest = match done(
        client,
        Mutation::WorktreeNew {
            repo: repo.to_path_buf(),
            branch: branch.into(),
            base_ref: None,
            dest: None,
            open: false,
        },
    ) {
        Done::Path(p) => p,
        other => panic!("worktree-new result: {other:?}"),
    };
    deck_core::worktree::resolve(&dest).expect("a linked worktree")
}

fn retired(d: Done) -> (Vec<String>, u32, bool) {
    match d {
        Done::Retired {
            sessions,
            moved,
            parked,
            ..
        } => (sessions, moved, parked),
        other => panic!("retire result: {other:?}"),
    }
}

/// Launches omp in `dir` (a tmux session named `name`) and waits until it runs.
fn launch_omp(client: &GatewayClient, dir: &Path, name: &str) -> SessionTarget {
    let launch = client.mutate(Mutation::Launch(LaunchSpec {
        mux_name: Some(format!("{name}-{}", std::process::id())),
        cwd: dir.to_path_buf(),
        session_path: None,
        label: name.into(),
        argv: vec!["omp".into()],
        role: SessionRole::Session,
        copy_of: None,
        view: Default::default(),
    }));
    launch
        .accepted
        .wait(WAIT)
        .expect("accepted in time")
        .expect("launch accepted");
    let target = match launch.outcome.wait(WAIT) {
        Some(Ok(Outcome::Done(d))) => match *d {
            Done::Session(t, _) => t,
            other => panic!("launch result: {other:?}"),
        },
        other => panic!("launch outcome: {other:?}"),
    };
    wait_for("the session running", Duration::from_secs(90), || {
        controllers(client)
            .into_iter()
            .any(|c| c.target == target && c.exit_code.is_none())
            .then_some(())
    });
    target
}

/// How the finished worktree's session is there when the finish starts.
enum FinishSession<'a> {
    /// Running in a tab.
    Running,
    /// Closed: a stored session (the store root given) the finish opens itself.
    Closed(&'a Path),
}

/// Finishing a clean worktree already merged into `main` (`squash`: by a squash merge): the
/// session is asked, answers, moves into the main checkout by `/move` and keeps running; the
/// worktree and its branch are gone.
fn finish_removes(
    client: &GatewayClient,
    log: &Path,
    repo: &Path,
    branch: &str,
    squash: bool,
    session: FinishSession<'_>,
) {
    let w = new_worktree(client, repo, branch);
    commit(&w.path, &format!("{branch}.txt"), "done\n");
    if squash {
        git(repo, &["merge", "-q", "--squash", branch]);
        git(repo, &["commit", "-qm", &format!("squash {branch}")]);
    } else {
        git(
            repo,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {branch}"),
                branch,
            ],
        );
    }
    assert_eq!(verdict(client, &w), Retirement::Remove);
    let seed = format!("{branch}-seed.jsonl");
    let target = match session {
        FinishSession::Running => Some(launch_omp(client, &w.path, &format!("omp-fin-{branch}"))),
        FinishSession::Closed(store) => {
            let dir = store.join(format!("--{branch}--"));
            std::fs::create_dir_all(&dir).expect("store dir");
            let header = serde_json::json!({
                "type": "session",
                "id": format!("{branch}-seed"),
                "cwd": w.path.to_string_lossy(),
                "timestamp": "2026-10-29T00:00:00Z",
            });
            let title = serde_json::json!({ "type": "title", "title": "Seeded" });
            std::fs::write(dir.join(&seed), format!("{header}\n{title}\n")).expect("seed session");
            client.subscribe(Topic::Sessions);
            done(client, Mutation::RefreshSessions);
            wait_for("the seeded session scanned", WAIT, || {
                match client.query(Query::Topic(Topic::Sessions)).wait(WAIT) {
                    Some(Ok(Answer::Topic(v))) => match *v {
                        TopicValue::Sessions(s) => s
                            .groups
                            .iter()
                            .flat_map(|g| &g.sessions)
                            .any(|e| e.path.file_name().is_some_and(|n| n == seed.as_str()))
                            .then_some(()),
                        other => panic!("sessions value: {other:?}"),
                    },
                    other => panic!("sessions query: {other:?}"),
                }
            });
            None
        }
    };
    let finish = client.mutate(Mutation::WorktreeFinish {
        worktree: w.clone(),
    });
    finish
        .accepted
        .wait(WAIT)
        .expect("accepted in time")
        .expect("finish accepted");
    let outcome = finish.outcome.wait(Duration::from_secs(150));
    let Some(Ok(Outcome::Done(d))) = outcome else {
        panic!(
            "{branch}: finish outcome {outcome:?}\ncontrollers: {:#?}\ngateway log:\n{}",
            controllers(client),
            std::fs::read_to_string(log).unwrap_or_default()
        );
    };
    let (_, moved, parked) = retired(*d);
    assert_eq!((moved, parked), (1, false), "{branch}");
    assert!(!w.path.exists(), "{branch}: the worktree folder is gone");
    assert!(!branch_exists(repo, branch), "{branch}: the branch is gone");
    let listed = git(repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains(&*w.path.to_string_lossy()),
        "{branch}: still listed\n{listed}"
    );
    let c = controllers(client)
        .into_iter()
        .find(|c| match &target {
            Some(t) => c.target == *t,
            None => c
                .session_path
                .as_deref()
                .and_then(Path::file_name)
                .is_some_and(|n| n == seed.as_str()),
        })
        .expect("the moved session's controller");
    assert!(c.exit_code.is_none(), "{branch}: the moved session runs on");
    assert_eq!(
        c.cwd, repo,
        "{branch}: the session runs in the main checkout"
    );
}

#[test]
fn worktree_flows_run_in_the_gateway() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none()
        || which(if cfg!(windows) { "git.exe" } else { "git" }).is_none()
    {
        eprintln!("skipped: omp or git is not installed");
        return;
    }
    let sb = Sandbox::new();
    // Every session turn is answered by the scripted model (the finish prompt included).
    let model = FakeModel::start(|_| Reply::text("Already merged and clean."));
    std::fs::write(
        sb.root.join("home/.omp/agent/models.yml"),
        model.models_yml(),
    )
    .expect("models.yml");
    let remote = sb.root.join("remote.git");
    let repo = sb.root.join("proj/repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&sb.root, &["init", "-q", "--bare", "remote.git"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "README", "hello\n");
    git(
        &repo,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    let repo = deck_core::worktree::repo_info(&repo)
        .expect("repo info")
        .primary;

    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    client.subscribe(Topic::Worktrees(repo.clone()));

    // Refused before anything runs.
    assert!(
        failure(
            &client,
            Mutation::WorktreeNew {
                repo: repo.clone(),
                branch: "  ".into(),
                base_ref: None,
                dest: None,
                open: false,
            }
        )
        .contains("Name the new branch")
    );

    // The destination check the dialog shows is the one WorktreeNew applies.
    let asked = |branch: &str, dest: Option<PathBuf>| match client
        .query(Query::WorktreeDestination {
            repo: repo.clone(),
            branch: branch.into(),
            dest,
        })
        .wait(WAIT)
    {
        Some(Ok(Answer::WorktreeDestination(v))) => v,
        other => panic!("destination query: {other:?}"),
    };
    let default = asked("feat-d", None);
    assert!(
        default.dest.starts_with(sb.root.join("worktrees")),
        "{default:?}"
    );
    assert_eq!(default.error, None);
    let file = sb.root.join("in-the-way");
    std::fs::write(&file, "x").expect("file");
    let blocked = asked("feat-d", Some(file.clone()));
    assert_eq!(blocked.dest, file);
    assert_eq!(blocked.error.as_deref(), Some("A file is in the way"));
    assert!(
        failure(
            &client,
            Mutation::WorktreeNew {
                repo: repo.clone(),
                branch: "feat-d".into(),
                base_ref: None,
                dest: Some(file),
                open: false,
            }
        )
        .contains("A file is in the way")
    );

    // A new worktree lands at the managed default location with its merge target recorded.
    let a = new_worktree(&client, &repo, "feat-a");
    assert!(a.path.starts_with(sb.root.join("worktrees")), "{a:?}");
    assert_eq!(a.branch.as_deref(), Some("feat-a"));
    assert_eq!(
        config(&client)
            .worktree_bases
            .get(&a.path)
            .map(String::as_str),
        Some("main")
    );
    let entry = wait_for("the worktree in the view", WAIT, || {
        view(&client, &repo)
            .linked
            .into_iter()
            .find(|e| e.worktree.path == a.path)
    });
    assert_eq!(entry.base.as_deref(), Some("main"));
    assert!(entry.base_recorded && !entry.parkable && entry.flow.is_none());
    let own = view(&client, &a.path);
    assert_eq!(own.worktree.as_ref().map(|w| &w.path), Some(&a.path));
    assert!(own.repo.as_ref().is_some_and(|r| r.is_ok()));

    // Unmerged work cannot be removed.
    commit(&a.path, "a.txt", "a\n");
    assert!(matches!(verdict(&client, &a), Retirement::Blocked(_)));
    assert!(
        failure(
            &client,
            Mutation::WorktreeRetire {
                worktree: a.clone(),
                park: false,
            }
        )
        .starts_with("Cannot remove “feat-a”")
    );

    // The finish flow needs a session to ask.
    assert!(
        failure(
            &client,
            Mutation::WorktreeFinish {
                worktree: a.clone()
            }
        )
        .contains("no session to ask")
    );

    // Regular merge: removed, records forgotten, branch deleted.
    git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge feat-a", "feat-a"],
    );
    assert_eq!(verdict(&client, &a), Retirement::Remove);
    let (_, moved, parked) = retired(done(
        &client,
        Mutation::WorktreeRetire {
            worktree: a.clone(),
            park: false,
        },
    ));
    assert_eq!((moved, parked), (0, false));
    assert!(!a.path.exists());
    assert!(!branch_exists(&repo, "feat-a"));
    let cfg = config(&client);
    assert!(!cfg.worktree_bases.contains_key(&a.path));
    assert!(cfg.removed_worktrees.contains_key(&a.path));
    wait_for("the removed worktree gone from the view", WAIT, || {
        (!view(&client, &repo)
            .linked
            .iter()
            .any(|e| e.worktree.path == a.path))
        .then_some(())
    });

    // Squash merge: the same.
    let b = new_worktree(&client, &repo, "feat-b");
    commit(&b.path, "b.txt", "b\n");
    git(&repo, &["merge", "-q", "--squash", "feat-b"]);
    git(&repo, &["commit", "-qm", "squash feat-b"]);
    assert_eq!(verdict(&client, &b), Retirement::Remove);
    let (_, _, parked) = retired(done(
        &client,
        Mutation::WorktreeRetire {
            worktree: b.clone(),
            park: false,
        },
    ));
    assert!(!parked);
    assert!(!b.path.exists());

    // Pushed but not merged (an open PR): removing is refused, parking keeps the branch.
    let c = new_worktree(&client, &repo, "feat-c");
    commit(&c.path, "c.txt", "c\n");
    git(&c.path, &["push", "-q", "-u", "origin", "feat-c"]);
    assert_eq!(verdict(&client, &c), Retirement::Park);
    assert!(
        failure(
            &client,
            Mutation::WorktreeRetire {
                worktree: c.clone(),
                park: false,
            }
        )
        .ends_with("park it instead")
    );
    let (_, _, parked) = retired(done(
        &client,
        Mutation::WorktreeRetire {
            worktree: c.clone(),
            park: true,
        },
    ));
    assert!(parked);
    assert!(!c.path.exists());
    assert!(branch_exists(&repo, "feat-c"));
    let rec = config(&client)
        .parked_branches
        .iter()
        .find(|r| r.primary == repo && r.branch == "feat-c")
        .cloned()
        .expect("parked record");
    assert_eq!(rec.base, "main");
    wait_for("the parked branch in the view", WAIT, || {
        view(&client, &repo)
            .parked
            .iter()
            .any(|p| p.record.branch == "feat-c")
            .then_some(())
    });

    // Continue in the main checkout, then switch back: still parked.
    let continue_in = |place| Mutation::WorktreeContinue {
        primary: repo.clone(),
        branch: "feat-c".into(),
        session: None,
        place,
    };
    assert!(matches!(
        done(&client, continue_in(ContinueIn::Main)),
        Done::Unit
    ));
    assert_eq!(git(&repo, &["branch", "--show-current"]), "feat-c");
    assert!(matches!(
        done(&client, continue_in(ContinueIn::SwitchBack)),
        Done::Unit
    ));
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert!(
        config(&client)
            .parked_branches
            .iter()
            .any(|r| r.branch == "feat-c")
    );

    // Continue in a new worktree: no longer parked, its base recorded again.
    let dest = match done(&client, continue_in(ContinueIn::Worktree)) {
        Done::Path(p) => p,
        other => panic!("continue result: {other:?}"),
    };
    assert!(dest.starts_with(sb.root.join("worktrees")));
    assert_eq!(git(&dest, &["branch", "--show-current"]), "feat-c");
    let cfg = config(&client);
    assert!(!cfg.parked_branches.iter().any(|r| r.branch == "feat-c"));
    assert_eq!(
        cfg.worktree_bases.get(&dest).map(String::as_str),
        Some("main")
    );
    assert!(
        failure(&client, continue_in(ContinueIn::Main)).contains("not parked anymore"),
        "a branch no longer parked cannot be continued"
    );

    // The finish flow waits on the worktree's session and is cancellable while it does.
    let c2 = deck_core::worktree::resolve(&dest).expect("the continued worktree");
    launch_omp(&client, &dest, "omp-new-wt");
    let finish = client.mutate(Mutation::WorktreeFinish {
        worktree: c2.clone(),
    });
    finish
        .accepted
        .wait(WAIT)
        .expect("accepted in time")
        .expect("finish accepted");
    assert!(
        failure(
            &client,
            Mutation::WorktreeFinish {
                worktree: c2.clone()
            }
        )
        .contains("being finished already")
    );
    wait_for("the finish flow in the view", WAIT, || {
        view(&client, &repo)
            .linked
            .into_iter()
            .find(|e| e.worktree.path == c2.path)
            .and_then(|e| e.flow)
            .filter(|f| matches!(f, WorktreeFlow::Finishing { job, .. } if *job == finish.key))
    });
    let cancel = client.mutate(Mutation::CancelJob(finish.key.clone()));
    cancel
        .accepted
        .wait(WAIT)
        .expect("answered in time")
        .expect("cancel accepted");
    match finish.outcome.wait(WAIT) {
        Some(Ok(Outcome::Cancelled)) => {}
        other => panic!("cancelled finish outcome: {other:?}"),
    }
    assert!(c2.path.is_dir(), "a cancelled finish keeps the worktree");

    // Finishing a clean, merged worktree removes it: regular merge and squash merge alike.
    let log = sb.profile.state_dir().join("gateway.log");
    let store = sb.profile.sessions_root.clone();
    finish_removes(
        &client,
        &log,
        &repo,
        "fin-reg",
        false,
        FinishSession::Running,
    );
    finish_removes(&client, &log, &repo, "fin-sq", true, FinishSession::Running);
    // The finish opens the worktree's closed session itself (a regular and a squash merge).
    finish_removes(
        &client,
        &log,
        &repo,
        "fin-reg-c",
        false,
        FinishSession::Closed(&store),
    );
    finish_removes(
        &client,
        &log,
        &repo,
        "fin-sq-c",
        true,
        FinishSession::Closed(&store),
    );
}
