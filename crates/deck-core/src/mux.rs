//! Session backends behind one type: [`Mux::Tmux`] (a dedicated tmux server, Linux default) and
//! [`Mux::Host`] (the OmpDeck session host, `deck-host`: Windows always, Linux when tmux is
//! missing or chosen explicitly). Everything the app needs from a backend — launching a tab's
//! command, listing and killing sessions, finding the session file a pane's omp is on — goes
//! through here so no caller names tmux or the host.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deck_host::{RunState, TspFacts};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::platform;
use crate::status::{breadcrumb_key_for_term_session, breadcrumb_key_for_tty, trusted_breadcrumb};
use crate::tmux::{self, Tmux, TmuxError};

/// Namespace of the app's own host sessions (tests use `omp-deck-test-<unique>`).
pub const HOST_NAMESPACE: &str = "omp-deck";

/// Env var that selects another tmux socket (tests): [`Mux::detect`] uses it instead of
/// [`tmux::APP_SOCKET`].
pub const TMUX_SOCKET_ENV: &str = "OMP_DECK_TMUX_SOCKET";
/// Env var that selects another host namespace (tests).
pub const HOST_NS_ENV: &str = "OMP_DECK_HOST_NS";

/// The user's backend setting (`AppConfig.session_backend`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendChoice {
    /// tmux when installed, else the OmpDeck host.
    #[default]
    Auto,
    Tmux,
    Host,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MuxKind {
    Tmux,
    Host,
}

/// What omp shows of its state in one live session (see [`Mux::pane_status`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneStatus {
    /// Terminal title of the (active) pane.
    pub title: Option<String>,
    /// omp's OSC 7501 run state.
    pub run_state: Option<RunState>,
    /// What omp's TSP document shows of its run (host TSP sessions only).
    pub tsp_facts: Option<TspFacts>,
}

/// One live mux session as the Running view shows it ([`Mux::running`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSession {
    /// Mux session name (what [`Mux::kill`] takes).
    pub name: String,
    pub backend: MuxKind,
    /// Top-level session files its omp has open: the same mapping as [`Mux::session_files`].
    pub files: Vec<PathBuf>,
    /// Terminal title of the (active) pane; `None` when it set none.
    pub title: Option<String>,
    /// omp's run status from its OSC 7501 reports (host sessions only; `None` when none is
    /// held). Under TSP omp drops the `π` title, so this is its only state besides the
    /// document.
    #[serde(default)]
    pub run_state: Option<RunState>,
    /// What omp's TSP document shows of its run (host TSP sessions only).
    #[serde(default)]
    pub tsp_facts: Option<TspFacts>,
    /// tmux: the pane's current path. Host: the session's working directory.
    pub cwd: Option<PathBuf>,
    /// tmux `session_created`; host: when its child started.
    pub started: Option<SystemTime>,
    /// Pid of the (active) pane's process / the host child.
    pub pid: Option<u32>,
    /// tmux: every pane's process tree. Host: the daemon's tree (daemon, child, descendants).
    pub memory: Option<platform::TreeMemory>,
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Host(#[from] deck_host::Error),
}

/// The active pane of a session, as far as omp's terminal breadcrumb is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    /// File name of omp's breadcrumb under `<agent dir>/terminal-sessions/`: `pts-6` (tty) or
    /// `apple-<id>` (`TERM_SESSION_ID`).
    pub breadcrumb_key: String,
    /// When the pane's process started: a breadcrumb older than this is a previous owner's.
    pub started: SystemTime,
}

/// A process to run in a local PTY: `program args…` in `cwd` with `env` added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// How to start a tab's terminal process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Run in a local PTY the GUI owns (the tmux client, which attaches or creates the session).
    Pty(LaunchSpec),
    /// Ask the host daemon (`deck_host::create_or_attach`) to run this and attach.
    Host(deck_host::SessionSpec),
}

/// A backend handle: cheap to clone, every method blocks (run them on workers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mux {
    Tmux(Tmux),
    Host(HostMux),
}

/// The OmpDeck session host in one namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostMux {
    ns: deck_host::Namespace,
}

impl HostMux {
    /// The host for `namespace` (`omp-deck` or `omp-deck-test-<unique>`).
    pub fn new(namespace: &str) -> Result<HostMux, deck_host::Error> {
        Ok(HostMux {
            ns: deck_host::Namespace::new(namespace)?,
        })
    }

    /// The host when its daemon executable exists next to the app (or at `OMP_DECK_HOST_EXE`).
    pub fn detect(namespace: &str) -> Option<HostMux> {
        if deck_host::daemon_exe().is_none() {
            log::debug!("omp-deck-host executable not found");
            return None;
        }
        HostMux::new(namespace)
            .inspect_err(|e| log::warn!("host namespace {namespace}: {e}"))
            .ok()
    }

    pub fn namespace(&self) -> &deck_host::Namespace {
        &self.ns
    }

    /// breadcrumb key + child start time of a live session, from its daemon.
    fn pane(&self, name: &str) -> Option<Pane> {
        pane_of_info(&self.ns.info(name).ok()?)
    }

    /// `info` of every live session, concurrently (one stuck daemon costs one control timeout,
    /// not one per session); sessions whose daemon fails to answer are left out.
    fn infos(&self) -> Vec<deck_host::HostInfo> {
        let names = self.ns.list();
        std::thread::scope(|scope| {
            let jobs: Vec<_> = names
                .iter()
                .map(|name| scope.spawn(|| self.ns.info(name).ok()))
                .collect();
            jobs.into_iter()
                .filter_map(|job| job.join().ok().flatten())
                .collect()
        })
    }

    fn running(&self, root: &Path) -> Vec<RunningSession> {
        let infos = self.infos();
        let roots: Vec<u32> = infos.iter().map(|i| i.daemon_pid).collect();
        let memory = platform::tree_memory(&roots);
        let files = files_from_breadcrumbs(
            infos.iter().map(|i| (i.name.clone(), pane_of_info(i))),
            root,
        );
        infos
            .into_iter()
            .zip(memory)
            .map(|(info, memory)| RunningSession {
                files: files.get(&info.name).cloned().unwrap_or_default(),
                name: info.name,
                backend: MuxKind::Host,
                title: info.title.filter(|t| !t.is_empty()),
                run_state: info.run_state,
                tsp_facts: info.tsp_facts,
                cwd: Some(info.cwd),
                started: Some(UNIX_EPOCH + Duration::from_millis(info.child_started_unix_ms)),
                pid: Some(info.child_pid),
                memory,
            })
            .collect()
    }

    /// Titles and run states reported by the live sessions' daemons (one `info` each, so one
    /// stuck daemon costs one control timeout, not one per session).
    fn status(&self) -> HashMap<String, PaneStatus> {
        let names = self.ns.list();
        std::thread::scope(|scope| {
            let jobs: Vec<_> = names
                .iter()
                .map(|name| {
                    scope.spawn(|| {
                        let info = self.ns.info(name).ok()?;
                        Some(PaneStatus {
                            title: info.title.filter(|t| !t.is_empty()),
                            run_state: info.run_state,
                            tsp_facts: info.tsp_facts,
                        })
                    })
                })
                .collect();
            names
                .iter()
                .zip(jobs)
                .filter_map(|(name, job)| Some((name.clone(), job.join().ok()??)))
                .filter(|(_, s)| {
                    s.title.is_some() || s.run_state.is_some() || s.tsp_facts.is_some()
                })
                .collect()
        })
    }
}

/// `value` when it is a test name, `omp-deck-test-` plus `[A-Za-z0-9_-]+`, at most 40 bytes; otherwise (unset,
/// empty or anything else, with a warning for the latter) `default`. A stray environment variable
/// can therefore never point the app at another tmux server or host namespace.
fn checked_override(value: Option<&str>, default: &str, var: &str) -> String {
    match value {
        None | Some("") => default.to_owned(),
        // `valid_namespace` is `[A-Za-z0-9_-]{1,40}`: the host's namespace rule, and a bound that
        // keeps a tmux socket path short too.
        Some(v)
            if v.strip_prefix("omp-deck-test-")
                .is_some_and(|rest| !rest.is_empty())
                && deck_host::valid_namespace(v) =>
        {
            v.to_owned()
        }
        Some(v) => {
            log::warn!("ignoring {var}={v:?}: only omp-deck-test-<name> is accepted");
            default.to_owned()
        }
    }
}

/// The host namespace in effect: `OMP_DECK_HOST_NS` when it is a test name, else [`HOST_NAMESPACE`].
pub fn host_namespace() -> String {
    checked_override(
        std::env::var(HOST_NS_ENV).ok().as_deref(),
        HOST_NAMESPACE,
        HOST_NS_ENV,
    )
}

/// The tmux socket in effect: `OMP_DECK_TMUX_SOCKET` when it is a test name, else
/// [`tmux::APP_SOCKET`].
pub fn tmux_socket() -> String {
    checked_override(
        std::env::var(TMUX_SOCKET_ENV).ok().as_deref(),
        tmux::APP_SOCKET,
        TMUX_SOCKET_ENV,
    )
}

impl Mux {
    /// The backend for `choice`. Windows always uses the host. Elsewhere `Auto` is tmux when
    /// installed, else the host when its daemon exists; `Tmux` / `Host` demand that backend.
    /// `OMP_DECK_TMUX_SOCKET` / `OMP_DECK_HOST_NS` move either backend to a test namespace.
    /// `None`: nothing usable (the caller falls back to [`direct_launch`]).
    pub fn detect(choice: BackendChoice) -> Option<Mux> {
        let choice = if cfg!(windows) {
            BackendChoice::Host
        } else {
            choice
        };
        let tmux = || Tmux::detect_on(&tmux_socket()).map(Mux::Tmux);
        let host = || HostMux::detect(&host_namespace()).map(Mux::Host);
        match choice {
            BackendChoice::Tmux => tmux(),
            BackendChoice::Host => host(),
            BackendChoice::Auto => tmux().or_else(host),
        }
    }

    pub fn kind(&self) -> MuxKind {
        match self {
            Mux::Tmux(_) => MuxKind::Tmux,
            Mux::Host(_) => MuxKind::Host,
        }
    }

    /// The tmux socket name or the host namespace: what ctl endpoints are scoped by.
    pub fn namespace(&self) -> &str {
        match self {
            Mux::Tmux(t) => t.socket(),
            Mux::Host(h) => h.ns.as_str(),
        }
    }

    /// [`launch_with_env`](Self::launch_with_env) without extra environment.
    pub fn launch(&self, name: &str, cwd: &Path, argv: &[String]) -> Launch {
        self.launch_with_env(name, cwd, argv, &[])
    }

    /// How to start session `name` running `argv` in `cwd` (a missing dir falls back to `$HOME`).
    /// `extra_env` reaches the program itself, on both backends (tmux cannot pass a client's
    /// environment on to its panes, so there it is an `env K=V` prefix of the pane command).
    pub fn launch_with_env(
        &self,
        name: &str,
        cwd: &Path,
        argv: &[String],
        extra_env: &[(String, String)],
    ) -> Launch {
        match self {
            Mux::Tmux(t) => {
                if extra_env.is_empty() {
                    return Launch::Pty(t.attach_or_create(name, cwd, argv));
                }
                let mut wrapped = vec!["env".to_owned()];
                wrapped.extend(extra_env.iter().map(|(k, v)| format!("{k}={v}")));
                wrapped.extend(argv.iter().cloned());
                Launch::Pty(t.attach_or_create(name, cwd, &wrapped))
            }
            Mux::Host(_) => Launch::Host(host_spec(name, cwd, argv, extra_env, cfg!(windows))),
        }
    }

    /// Every live session with the facts the Running view needs, from one tmux `list-panes -a`
    /// (or one `info` per host daemon) and one pass over the process table. Sorted by name.
    /// Blocking: call on a worker.
    pub fn running(&self, root: &Path) -> Vec<RunningSession> {
        let mut sessions = match self {
            Mux::Tmux(t) => t.running(root),
            Mux::Host(h) => h.running(root),
        };
        sessions.sort_by(|a, b| a.name.cmp(&b.name));
        sessions
    }

    /// Memory of the tmux server process alone (not its panes); `None` for the host, or when
    /// there is no server or the OS cannot tell.
    pub fn server_memory(&self) -> Option<platform::TreeMemory> {
        match self {
            Mux::Tmux(t) => t.server_memory(),
            Mux::Host(_) => None,
        }
    }

    /// Names of the live sessions.
    pub fn alive_sessions(&self) -> HashSet<String> {
        match self {
            Mux::Tmux(t) => t.alive_sessions(),
            Mux::Host(h) => h.ns.list().into_iter().collect(),
        }
    }

    pub fn has_session(&self, name: &str) -> bool {
        match self {
            Mux::Tmux(t) => t.has_session(name),
            Mux::Host(h) => h.ns.has(name),
        }
    }

    pub fn kill(&self, name: &str) -> Result<(), MuxError> {
        match self {
            Mux::Tmux(t) => Ok(t.kill(name)?),
            Mux::Host(h) => Ok(h.ns.kill(name)?),
        }
    }

    /// Maps each live session to the top-level session `.jsonl` files (direct children of a
    /// project dir under `root`) its omp has open. tmux: the files held open by the pane's
    /// process tree (`/proc`, Linux). Host: the file named by the session's trusted breadcrumb.
    pub fn session_files(&self, root: &Path) -> HashMap<String, Vec<PathBuf>> {
        match self {
            Mux::Tmux(t) => t.session_files(root),
            Mux::Host(h) => host_session_files(h, root),
        }
    }

    /// The session's active pane; `None` when it is gone or has no breadcrumb key.
    pub fn pane(&self, name: &str) -> Option<Pane> {
        match self {
            Mux::Tmux(t) => t.pane(name),
            Mux::Host(h) => h.pane(name),
        }
    }

    /// Session name → what omp shows of its state in each live session: the terminal title of
    /// the active pane and, on the host, omp's OSC 7501 run state (tmux 3.7 does not forward
    /// unknown OSC to its clients, so there only the title exists). tmux asks its server once;
    /// the host asks each session's daemon (`info`, concurrently). Sessions with neither, and
    /// failures, have no entry.
    pub fn pane_status(&self) -> HashMap<String, PaneStatus> {
        match self {
            Mux::Tmux(t) => t
                .pane_titles()
                .into_iter()
                .map(|(name, title)| {
                    (
                        name,
                        PaneStatus {
                            title: Some(title),
                            run_state: None,
                            tsp_facts: None,
                        },
                    )
                })
                .collect(),
            Mux::Host(h) => h.status(),
        }
    }
}

/// breadcrumb key + child start time of a host session, from its `info`.
fn pane_of_info(info: &deck_host::HostInfo) -> Option<Pane> {
    let key = match (&info.tty, &info.term_session_id) {
        (Some(tty), _) => breadcrumb_key_for_tty(tty),
        (None, Some(id)) => breadcrumb_key_for_term_session(id),
        (None, None) => return None,
    };
    Some(Pane {
        breadcrumb_key: key,
        started: UNIX_EPOCH + Duration::from_millis(info.child_started_unix_ms),
    })
}

fn host_session_files(host: &HostMux, root: &Path) -> HashMap<String, Vec<PathBuf>> {
    let panes = host.ns.list().into_iter().map(|name| {
        let pane = host.pane(&name);
        (name, pane)
    });
    files_from_breadcrumbs(panes, root)
}

/// Each session's top-level session file as its trusted breadcrumb names it (under the agent dir
/// next to `root`); a session without a pane, breadcrumb or trusted breadcrumb maps to no files.
fn files_from_breadcrumbs(
    panes: impl IntoIterator<Item = (String, Option<Pane>)>,
    root: &Path,
) -> HashMap<String, Vec<PathBuf>> {
    let agent_dir = root.parent().unwrap_or(root);
    let canon_root = std::fs::canonicalize(root).ok();
    panes
        .into_iter()
        .map(|(name, pane)| {
            let file = pane
                .and_then(|p| {
                    trusted_breadcrumb(agent_dir, &p.breadcrumb_key, Some(p.started), None)
                })
                .map(|crumb| crumb.session)
                .filter(|s| tmux::is_top_level_session(s, root, canon_root.as_deref()));
            (name, file.into_iter().collect())
        })
        .collect()
}

// ---- session names, working dir, terminal environment --------------------------------------------

/// `"omp-"` + id with every char outside `[A-Za-z0-9_-]` replaced by `_`.
pub fn session_name_for(id: &str) -> String {
    let mut s = String::from("omp-");
    s.extend(id.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            c
        } else {
            '_'
        }
    }));
    s
}

/// `omp-new-<unix-ms>`.
pub fn new_session_name(now: SystemTime) -> String {
    let ms = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("omp-new-{ms}")
}

/// Degraded mode (no tmux): `env -u WINDOWID -u ALACRITTY_WINDOW_ID <argv...>`; same env/cwd rules.
pub fn direct_launch(cwd: &Path, argv: &[String]) -> LaunchSpec {
    // alacritty forces WINDOWID/ALACRITTY_WINDOW_ID into the child env; strip them via env(1).
    let mut args: Vec<String> = ["-u", "WINDOWID", "-u", "ALACRITTY_WINDOW_ID"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    args.extend(argv.iter().cloned());
    LaunchSpec {
        program: "env".into(),
        args,
        cwd: effective_cwd(cwd),
        env: terminal_env(),
    }
}

/// `cwd` if it is an existing dir, else `$HOME`, else the filesystem root.
pub fn effective_cwd(cwd: &Path) -> PathBuf {
    if cwd.is_dir() {
        cwd.to_path_buf()
    } else {
        platform::home_or_root()
    }
}

/// Overrides omp honours: inside tmux it would otherwise disable images (OmpDeck renders kitty
/// Unicode-placeholder images) and never emit OSC 8 hyperlinks. `PI_NOTIFICATIONS=off` silences
/// omp's own BEL / desktop notifications: OmpDeck's notifier is the single source.
pub(crate) const OMP_ENV: [(&str, &str); 4] = [
    ("PI_FORCE_IMAGE_PROTOCOL", "kitty"),
    ("PI_KITTY_PLACEHOLDERS", "1"),
    ("PI_FORCE_HYPERLINKS", "1"),
    ("PI_NOTIFICATIONS", "off"),
];

/// Environment of the PTY child (tmux client or omp itself); a tmux server started by it
/// inherits the omp overrides, and the generated config sets them for running servers too.
pub fn terminal_env() -> Vec<(String, String)> {
    [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "omp-deck"),
    ]
    .into_iter()
    .chain(OMP_ENV)
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

// ---- host launch -------------------------------------------------------------------------------

/// Variables of the parent environment that make omp believe it runs in another terminal or
/// multiplexer (its terminal-breadcrumb key, image/keyboard protocol choices, SSH mode):
/// removed from every host session's environment. `TERM_SESSION_ID` is inherited noise here; on
/// Windows the host sets its own.
pub const HOST_ENV_REMOVE: [&str; 24] = [
    "WT_SESSION",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "ZELLIJ",
    "ZELLIJ_PANE_ID",
    "ZELLIJ_SESSION_NAME",
    "CMUX_SURFACE_ID",
    "CMUX_WORKSPACE_ID",
    "WMUX",
    "WMUX_SURFACE_ID",
    "KITTY_WINDOW_ID",
    "WEZTERM_PANE",
    "GHOSTTY_RESOURCES_DIR",
    "ITERM_SESSION_ID",
    "LC_TERMINAL",
    "VSCODE_PID",
    "ALACRITTY_WINDOW_ID",
    "WINDOWID",
    "TERM_FEATURES",
    "PASEO_TERMINAL_ID",
    "SSH_CONNECTION",
    "SSH_TTY",
    "SSH_CLIENT",
];

/// `TERM_SESSION_ID` of a Windows host session: 16 hex digits of sha256 over the session name,
/// so omp's breadcrumb of that terminal (`apple-<id>`) is stable across GUI restarts.
pub fn term_session_id(session: &str) -> String {
    Sha256::digest(session.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The environment a host session's program gets: `(set, remove)`. `set` is the terminal
/// identity of [`terminal_env`] (+ `TERM_SESSION_ID` when `windows`) followed by `extra`;
/// `remove` is [`HOST_ENV_REMOVE`] (the host removes first, then sets).
pub fn host_env(
    session: &str,
    extra: &[(String, String)],
    windows: bool,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut set = terminal_env();
    if windows {
        set.push(("TERM_SESSION_ID".to_owned(), term_session_id(session)));
    }
    for (k, v) in extra {
        set.retain(|(sk, _)| sk != k);
        set.push((k.clone(), v.clone()));
    }
    (
        set,
        HOST_ENV_REMOVE.iter().map(|s| (*s).to_owned()).collect(),
    )
}

/// The host session spec for `argv`. A bare program name is resolved on PATH here (`PATHEXT` on
/// Windows) so the daemon, which may run with another PATH, starts the same binary the GUI sees.
fn host_spec(
    name: &str,
    cwd: &Path,
    argv: &[String],
    extra_env: &[(String, String)],
    windows: bool,
) -> deck_host::SessionSpec {
    let (program, args) = argv
        .split_first()
        .map_or((String::new(), &[][..]), |(p, a)| (p.clone(), a));
    let program = if Path::new(&program).components().count() == 1 {
        platform::find_on_path(&program).unwrap_or_else(|| PathBuf::from(&program))
    } else {
        PathBuf::from(&program)
    };
    let (env_set, env_remove) = host_env(name, extra_env, windows);
    deck_host::SessionSpec {
        name: name.to_owned(),
        program,
        args: args.to_vec(),
        cwd: effective_cwd(cwd),
        env_set,
        env_remove,
        tsp: false,
    }
}

// ---- terminal titles ---------------------------------------------------------------------------

/// True when a terminal title carries no information about the session: empty, the machine's
/// hostname (tmux's default pane title) or, on Windows, a bare `omp` (omp's helper children
/// reset the console title to it).
pub fn is_noise_title(title: &str) -> bool {
    static HOST: LazyLock<String> = LazyLock::new(|| platform::hostname().unwrap_or_default());
    title_is_noise(title, &HOST, cfg!(windows))
}

/// [`is_noise_title`] for a given hostname and OS: `title` equals `host` or its short form
/// (before the first `.`), or, with `windows`, is `omp`; surrounding whitespace is ignored.
pub fn title_is_noise(title: &str, host: &str, windows: bool) -> bool {
    let t = title.trim();
    if t.is_empty() {
        return true;
    }
    if windows && t.eq_ignore_ascii_case("omp") {
        return true;
    }
    if host.is_empty() {
        return false;
    }
    let short = host.split('.').next().unwrap_or(host);
    t == host || t == short
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn host_env_sets_terminal_identity_and_removes_foreign_terminals() {
        let (set, remove) = host_env("omp-abc", &[], false);
        assert_eq!(
            set,
            [
                kv("TERM", "xterm-256color"),
                kv("COLORTERM", "truecolor"),
                kv("TERM_PROGRAM", "omp-deck"),
                kv("PI_FORCE_IMAGE_PROTOCOL", "kitty"),
                kv("PI_KITTY_PLACEHOLDERS", "1"),
                kv("PI_FORCE_HYPERLINKS", "1"),
                kv("PI_NOTIFICATIONS", "off"),
            ]
        );
        assert_eq!(remove.len(), 24);
        for var in [
            "WT_SESSION",
            "TMUX",
            "TMUX_PANE",
            "STY",
            "ZELLIJ",
            "ZELLIJ_PANE_ID",
            "ZELLIJ_SESSION_NAME",
            "CMUX_SURFACE_ID",
            "CMUX_WORKSPACE_ID",
            "WMUX",
            "WMUX_SURFACE_ID",
            "KITTY_WINDOW_ID",
            "WEZTERM_PANE",
            "GHOSTTY_RESOURCES_DIR",
            "ITERM_SESSION_ID",
            "LC_TERMINAL",
            "VSCODE_PID",
            "ALACRITTY_WINDOW_ID",
            "WINDOWID",
            "TERM_FEATURES",
            "PASEO_TERMINAL_ID",
            "SSH_CONNECTION",
            "SSH_TTY",
            "SSH_CLIENT",
        ] {
            assert!(remove.iter().any(|r| r == var), "{var} not removed");
        }
        assert!(
            !set.iter().any(|(k, _)| remove.contains(k)),
            "nothing is both set and removed on Unix"
        );
    }

    #[test]
    fn windows_host_env_adds_a_stable_term_session_id() {
        let (set, _) = host_env("omp-abc", &[], true);
        let id = set
            .iter()
            .find(|(k, _)| k == "TERM_SESSION_ID")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(id.len(), 16);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(id, term_session_id("omp-abc"));
        assert_ne!(id, term_session_id("omp-abd"));
        // sha256("omp-abc") starts a1 e4... only pin the derivation: first 8 bytes of the digest.
        let digest = Sha256::digest(b"omp-abc");
        let want: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(id, want);
    }

    #[test]
    fn extra_env_overrides_defaults_and_keeps_order() {
        let (set, _) = host_env(
            "s",
            &[kv("TERM", "dumb"), kv("OMPDECK_CTL_PIPE", r"\\.\pipe\x")],
            false,
        );
        assert_eq!(set.iter().filter(|(k, _)| k == "TERM").count(), 1);
        assert!(set.contains(&kv("TERM", "dumb")));
        assert_eq!(set.last(), Some(&kv("OMPDECK_CTL_PIPE", r"\\.\pipe\x")));
    }

    #[test]
    fn host_spec_splits_argv_and_falls_back_to_home_for_a_missing_cwd() {
        let argv: Vec<String> = ["/opt/omp/omp", "--resume", "/s/a.jsonl"]
            .map(str::to_owned)
            .to_vec();
        let spec = host_spec(
            "omp-x",
            Path::new("/definitely/not/here"),
            &argv,
            &[],
            false,
        );
        assert_eq!(spec.name, "omp-x");
        assert_eq!(spec.program, PathBuf::from("/opt/omp/omp"));
        assert_eq!(spec.args, ["--resume", "/s/a.jsonl"]);
        assert_eq!(spec.cwd, platform::home_or_root());
        assert_eq!(spec.env_remove.len(), HOST_ENV_REMOVE.len());
    }

    fn crumb(agent: &Path, key: &str, cwd: &Path, session: &Path) {
        let dir = agent.join("terminal-sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(key),
            format!("{}\n{}\ncwdstat 1 2\n", cwd.display(), session.display()),
        )
        .unwrap();
    }

    #[test]
    fn host_sessions_map_to_the_session_file_their_trusted_breadcrumb_names() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let root = agent.join("sessions");
        let proj = root.join("--work--proj--");
        std::fs::create_dir_all(&proj).unwrap();
        let main = proj.join("2026_main.jsonl");
        let other = proj.join("2026_other.jsonl");
        std::fs::write(&main, "{}\n").unwrap();
        std::fs::write(&other, "{}\n").unwrap();
        let sub = proj.join("2026_main").join("0-Agent.jsonl");
        std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
        std::fs::write(&sub, "{}\n").unwrap();

        crumb(&agent, "pts-6", Path::new("/work/proj"), &main);
        crumb(
            &agent,
            "apple-0123456789abcdef",
            Path::new("/work/proj"),
            &other,
        );
        crumb(&agent, "pts-9", Path::new("/work/proj"), &sub); // a subagent file is no tab's session
        let now = SystemTime::now();
        let pane = |key: &str, started: SystemTime| {
            Some(Pane {
                breadcrumb_key: key.to_owned(),
                started,
            })
        };
        let files = files_from_breadcrumbs(
            [
                (
                    "tty".to_owned(),
                    pane("pts-6", now - Duration::from_secs(60)),
                ),
                (
                    "conpty".to_owned(),
                    pane("apple-0123456789abcdef", now - Duration::from_secs(60)),
                ),
                (
                    "sub".to_owned(),
                    pane("pts-9", now - Duration::from_secs(60)),
                ),
                // The breadcrumb is older than this pane's process: a previous owner's.
                (
                    "stale".to_owned(),
                    pane("pts-6", now + Duration::from_secs(3600)),
                ),
                ("nocrumb".to_owned(), pane("pts-77", now)),
                ("gone".to_owned(), None),
            ],
            &root,
        );
        assert_eq!(files["tty"], [main]);
        assert_eq!(files["conpty"], [other]);
        for name in ["sub", "stale", "nocrumb", "gone"] {
            assert!(files[name].is_empty(), "{name}: {:?}", files[name]);
        }
        assert_eq!(files.len(), 6);
    }

    #[test]
    fn tmux_launch_hands_extra_env_to_the_pane_command_not_the_client() {
        let t = Tmux::with_socket("tmux", "sock-x", PathBuf::from("/c/t.conf"));
        let mux = Mux::Tmux(t.clone());
        let argv = vec!["omp".to_owned(), "--resume".into()];
        let cwd = Path::new("/");
        // Without extra env the tmux launch is exactly the plain one.
        assert_eq!(
            mux.launch("omp-1", cwd, &argv),
            Launch::Pty(t.attach_or_create("omp-1", cwd, &argv))
        );
        let Launch::Pty(spec) = mux.launch_with_env(
            "omp-1",
            cwd,
            &argv,
            &[kv("OMPDECK_CTL_DIR", "/run/d"), kv("A", "b c")],
        ) else {
            panic!("tmux launches through a PTY");
        };
        let tail = &spec.args[spec.args.len() - 5..];
        assert_eq!(
            tail,
            ["env", "OMPDECK_CTL_DIR=/run/d", "A=b c", "omp", "--resume"]
        );
        assert_eq!(spec.args[spec.args.len() - 6], "--");
    }

    #[test]
    fn env_overrides_only_accept_test_names() {
        let check = |v: Option<&str>| checked_override(v, "omp-deck", "X");
        assert_eq!(
            check(Some("omp-deck-test-123_a-b")),
            "omp-deck-test-123_a-b"
        );
        for bad in [
            "",
            "default",
            "omp-deck",
            "omp-deck-test-",
            "omp-deck-test-a b",
            "omp-deck-test-a/b",
            "omp-deck-test-é",
            "x-omp-deck-test-1",
            "OMP-DECK-TEST-1",
            // 41 bytes: too long for a host namespace.
            "omp-deck-test-aaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(check(Some(bad)), "omp-deck", "{bad:?}");
        }
        // Exactly 40 bytes is the longest accepted name.
        let longest = format!("omp-deck-test-{}", "a".repeat(26));
        assert_eq!(longest.len(), 40);
        assert_eq!(check(Some(&longest)), longest);
        assert_eq!(check(None), "omp-deck");
    }

    #[test]
    fn noise_titles() {
        assert!(title_is_noise("  ", "pc.lan", false));
        assert!(title_is_noise("pc.lan", "pc.lan", false));
        assert!(title_is_noise(" pc\n", "pc.lan", false));
        assert!(!title_is_noise("pc2", "pc.lan", false));
        assert!(!title_is_noise("pc", "", false));
        assert!(
            !title_is_noise("omp", "pc", false),
            "bare omp is a title on Unix"
        );
        assert!(title_is_noise("omp", "pc", true));
        assert!(title_is_noise(" OMP ", "pc", true));
        assert!(!title_is_noise("omp — fix the build", "pc", true));
    }
}
