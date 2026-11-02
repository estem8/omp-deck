//! tmux integration on a dedicated socket. Every command carries `-L <socket> -f <config>`;
//! the user's default tmux server is never touched.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::config::config_dir;
use crate::mux::{LaunchSpec, MuxKind, OMP_ENV, Pane, RunningSession, effective_cwd, terminal_env};
use crate::platform;
use crate::status::breadcrumb_key_for_tty;

pub const APP_SOCKET: &str = "omp-deck";

/// How long [`Tmux::kill`] waits for detached clients to leave, and how often it looks.
const KILL_DETACH_WAIT: Duration = Duration::from_secs(2);
const KILL_DETACH_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("cannot run tmux: {0}")]
    Io(#[from] io::Error),
    #[error("tmux {command} failed: {stderr}")]
    Failed { command: String, stderr: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tmux {
    program: PathBuf,
    socket: String,
    config: PathBuf,
}

impl Tmux {
    /// tmux on PATH (`tmux -V` runs ok) → Some, socket [`APP_SOCKET`], config `config_dir()/tmux.conf`.
    pub fn detect() -> Option<Tmux> {
        Tmux::detect_on(APP_SOCKET)
    }

    /// [`detect`](Self::detect) on another socket (tests).
    pub fn detect_on(socket: &str) -> Option<Tmux> {
        let ok = Command::new("tmux")
            .arg("-V")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        ok.then(|| Tmux::with_socket("tmux", socket, config_dir().join("tmux.conf")))
    }

    pub fn with_socket(
        program: impl Into<PathBuf>,
        socket: impl Into<String>,
        config: impl Into<PathBuf>,
    ) -> Tmux {
        Tmux {
            program: program.into(),
            socket: socket.into(),
            config: config.into(),
        }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.env_remove("TMUX")
            .arg("-L")
            .arg(&self.socket)
            .arg("-f")
            .arg(&self.config)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> io::Result<Output> {
        self.command().args(args).output()
    }

    /// Writes the generated config (creating dirs) and, if this socket's server is already
    /// running (`-f` only applies at server start), applies it with `source-file`.
    pub fn ensure_config(&self) -> io::Result<PathBuf> {
        if let Some(dir) = self.config.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.config, config_text())?;
        if self.server_running() {
            let conf = self.config.to_string_lossy();
            let out = self.run(&["source-file", &conf])?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "tmux source-file failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Ok(self.config.clone())
    }

    fn server_running(&self) -> bool {
        self.run(&["list-sessions", "-F", "#{session_name}"])
            .is_ok_and(|o| o.status.success())
    }

    /// `env -u TMUX <tmux> -L <sock> -f <conf> new-session -A -s <name> -c <cwd'> -- <argv...>`
    pub fn attach_or_create(&self, name: &str, cwd: &Path, argv: &[String]) -> LaunchSpec {
        let cwd = effective_cwd(cwd);
        let mut args: Vec<String> = vec![
            "-u".into(),
            "TMUX".into(),
            "-u".into(),
            "WINDOWID".into(),
            "-u".into(),
            "ALACRITTY_WINDOW_ID".into(),
            self.program.to_string_lossy().into_owned(),
            "-L".into(),
            self.socket.clone(),
            "-f".into(),
            self.config.to_string_lossy().into_owned(),
            "new-session".into(),
            "-A".into(),
            "-s".into(),
            name.into(),
            "-c".into(),
            cwd.to_string_lossy().into_owned(),
            "--".into(),
        ];
        args.extend(argv.iter().cloned());
        LaunchSpec {
            program: "env".into(),
            args,
            // The tmux server inherits its first client's process cwd; a pane folder that is later
            // deleted (a removed worktree) must not become the cwd of every future pane. The pane
            // folder travels only in `-c`.
            cwd: platform::root_dir(),
            env: terminal_env(),
        }
    }

    /// Live session names; no server running → empty.
    pub fn alive_sessions(&self) -> HashSet<String> {
        match self.run(&["list-sessions", "-F", "#{session_name}"]) {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            _ => HashSet::new(),
        }
    }

    /// Output of `display -p -t =<session>: <format>` for the session's active pane; `None` when
    /// the command fails or prints nothing (no such session).
    fn pane_format(&self, session: &str, format: &str) -> Option<String> {
        let out = self
            .run(&["display", "-p", "-t", &format!("={session}:"), format])
            .ok()
            .filter(|o| o.status.success())?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    /// The session's active pane: omp's breadcrumb key (from the pane's tty, `/dev/pts/6` →
    /// `pts-6`) and when the pane's process started (`/proc`, Linux; elsewhere "now", which only
    /// trusts breadcrumbs written from here on). A breadcrumb older than the start belongs to a
    /// previous owner of the same tty.
    pub fn pane(&self, session: &str) -> Option<Pane> {
        let text = self.pane_format(session, "#{pane_tty} #{pane_pid}")?;
        let (tty, pid) = text.split_once(' ')?;
        let started = pid
            .trim()
            .parse::<u32>()
            .ok()
            .and_then(platform::process_start)
            .unwrap_or_else(SystemTime::now);
        Some(Pane {
            breadcrumb_key: breadcrumb_key_for_tty(tty),
            started,
        })
    }

    /// Session name → title of its active pane (the `#T` the pane's program set), one
    /// `list-panes -a` for every session. Each line is `name<TAB>active<TAB>title`: the title is
    /// last, so a tab inside it would survive; session names never contain a tab (tmux rewrites
    /// it) and tmux strips control characters from titles, so a title cannot span lines. Empty
    /// titles are omitted; no server or any failure → empty map.
    pub fn pane_titles(&self) -> HashMap<String, String> {
        let format =
            "#{session_name}\t#{?#{&&:#{pane_active},#{window_active}},1,0}\t#{pane_title}";
        let Ok(out) = self.run(&["list-panes", "-a", "-F", format]) else {
            return HashMap::new();
        };
        if !out.status.success() {
            return HashMap::new();
        }
        parse_pane_titles(&String::from_utf8_lossy(&out.stdout))
    }

    pub fn has_session(&self, name: &str) -> bool {
        // `=` forces exact-name matching instead of prefix matching.
        self.run(&["has-session", "-t", &format!("={name}")])
            .is_ok_and(|o| o.status.success())
    }

    /// Ends session `name`. Its clients are detached first and given time to go away: tmux (3.7c,
    /// `tty-keys.c:1014`, still unguarded on master) runs `window_update_focus(c->session->curw
    /// ->window)` for a focus-in report (`ESC [ I`) still readable on the tty of a client whose
    /// `c->session` is NULL, and `kill-session` NULLs that for every attached client. The server
    /// then segfaults and every other session on the socket dies with it. A detached client keeps
    /// its session pointer until it is gone (`server_client_detach` records `exit_session`;
    /// only `server_client_lost` clears it), so waiting for the client list to empty first closes
    /// the window. OmpDeck tabs report focus changes constantly, e.g. while the stop dialog is open.
    pub fn kill(&self, name: &str) -> Result<(), TmuxError> {
        let target = format!("={name}");
        // Failure (no such session, no server) is reported by `kill-session` below.
        if self
            .run(&["detach-client", "-s", &target])
            .is_ok_and(|o| o.status.success())
        {
            let deadline = Instant::now() + KILL_DETACH_WAIT;
            while self.has_clients(&target) && Instant::now() < deadline {
                std::thread::sleep(KILL_DETACH_POLL);
            }
        }
        let out = self.run(&["kill-session", "-t", &target])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(TmuxError::Failed {
                command: "kill-session".into(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            })
        }
    }

    /// True while any client is attached to `target` (`=name`). Unknown session: false.
    fn has_clients(&self, target: &str) -> bool {
        self.run(&["list-clients", "-t", target, "-F", "#{client_pid}"])
            .is_ok_and(|o| o.status.success() && !o.stdout.iter().all(u8::is_ascii_whitespace))
    }

    /// See contract: maps each live session to the top-level session `.jsonl` files (direct
    /// children of a project dir under `root`) held open by any process in its pane tree.
    pub fn session_files(&self, root: &Path) -> HashMap<String, Vec<PathBuf>> {
        let mut result = HashMap::new();
        if !cfg!(target_os = "linux") {
            return result;
        }
        let Ok(out) = self.run(&["list-panes", "-a", "-F", "#{session_name} #{pane_pid}"]) else {
            return result;
        };
        if !out.status.success() {
            return result;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let panes: Vec<(&str, u32)> = text
            .lines()
            .filter_map(|l| {
                let (name, pid) = l.rsplit_once(' ')?;
                Some((name, pid.trim().parse().ok()?))
            })
            .collect();
        if panes.is_empty() {
            return result;
        }
        let canon_root = std::fs::canonicalize(root).ok();
        let pids: Vec<u32> = panes.iter().map(|(_, pid)| *pid).collect();
        for ((name, _), held) in panes.iter().zip(platform::tree_open_files(&pids)) {
            let files: &mut Vec<PathBuf> = result.entry((*name).to_owned()).or_default();
            add_top_level(files, held, root, canon_root.as_deref());
            files.sort();
        }
        result
    }

    /// Every live session in one `list-panes -a` plus one pass over the process table: the
    /// active pane's pid, path, title and the session's creation time; files and memory over all
    /// of the session's panes. No server or any failure → empty.
    pub fn running(&self, root: &Path) -> Vec<RunningSession> {
        let Ok(out) = self.run(&["list-panes", "-a", "-F", RUNNING_FORMAT]) else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
        let rows = parse_pane_rows(&String::from_utf8_lossy(&out.stdout));
        let pids: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        let held = platform::tree_open_files(&pids);
        let memory = platform::tree_memory(&pids);
        let canon_root = std::fs::canonicalize(root).ok();
        let mut sessions: Vec<RunningSession> = Vec::new();
        let mut index: HashMap<&str, usize> = HashMap::new();
        for ((row, held), memory) in rows.iter().zip(held).zip(memory) {
            let at = *index.entry(row.session.as_str()).or_insert_with(|| {
                sessions.push(RunningSession {
                    name: row.session.clone(),
                    backend: MuxKind::Tmux,
                    run_state: None,
                    tsp_facts: None,
                    files: Vec::new(),
                    title: None,
                    cwd: None,
                    started: row.created,
                    pid: None,
                    memory: None,
                });
                sessions.len() - 1
            });
            let session = &mut sessions[at];
            add_top_level(&mut session.files, held, root, canon_root.as_deref());
            if let Some(m) = memory {
                let total = session.memory.get_or_insert_default();
                total.bytes = total.bytes.saturating_add(m.bytes);
                total.processes += m.processes;
            }
            // The active pane speaks for the session; without one, the first pane does.
            if row.active || session.pid.is_none() {
                session.pid = Some(row.pid);
                session.cwd = row.path.clone();
                session.title = row.title.clone();
            }
        }
        for s in &mut sessions {
            s.files.sort();
        }
        sessions
    }

    /// Memory of the tmux server process alone (`#{pid}`, not its panes' trees); `None` when no
    /// server runs or the OS cannot tell.
    pub fn server_memory(&self) -> Option<platform::TreeMemory> {
        let out = self
            .run(&["display-message", "-p", "#{pid}"])
            .ok()
            .filter(|o| o.status.success())?;
        let pid = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        platform::process_memory(pid)
    }
}

/// Adds the top-level session files among `held` to `files` (no duplicates).
fn add_top_level(
    files: &mut Vec<PathBuf>,
    held: Vec<PathBuf>,
    root: &Path,
    canon_root: Option<&Path>,
) {
    for target in held {
        if is_top_level_session(&target, root, canon_root) && !files.contains(&target) {
            files.push(target);
        }
    }
}

/// `list-panes -a` format of [`Tmux::running`]: the title comes last (a tab inside it survives
/// `splitn`); tmux strips control characters from titles, so a title cannot span lines.
const RUNNING_FORMAT: &str = "#{session_name}\t#{?#{&&:#{pane_active},#{window_active}},1,0}\t#{pane_pid}\t#{session_created}\t#{pane_current_path}\t#{pane_title}";

/// One pane of [`Tmux::running`]'s query.
#[derive(Debug, PartialEq, Eq)]
struct PaneRow {
    session: String,
    /// The active pane of the active window.
    active: bool,
    pid: u32,
    created: Option<SystemTime>,
    path: Option<PathBuf>,
    title: Option<String>,
}

/// Parses [`RUNNING_FORMAT`] output; a line without a numeric pid is skipped.
fn parse_pane_rows(text: &str) -> Vec<PaneRow> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(6, '\t');
            let session = f.next()?.to_owned();
            let active = f.next()? == "1";
            let pid = f.next()?.parse().ok()?;
            let created = f
                .next()?
                .parse::<u64>()
                .ok()
                .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s));
            let path = Some(f.next()?).filter(|p| !p.is_empty()).map(PathBuf::from);
            let title = Some(f.next()?).filter(|t| !t.is_empty()).map(str::to_owned);
            Some(PaneRow {
                session,
                active,
                pid,
                created,
                path,
                title,
            })
        })
        .collect()
}

/// Parses `pane_titles` output; only active-pane lines with a non-empty title count.
fn parse_pane_titles(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let (name, active, title) = (parts.next()?, parts.next()?, parts.next()?);
            (active == "1" && !title.is_empty()).then(|| (name.to_owned(), title.to_owned()))
        })
        .collect()
}

pub(crate) fn is_top_level_session(p: &Path, root: &Path, canon_root: Option<&Path>) -> bool {
    if p.extension().is_none_or(|e| e != "jsonl") {
        return false;
    }
    let Some(grand) = p.parent().and_then(Path::parent) else {
        return false;
    };
    grand == root || canon_root.is_some_and(|c| grand == c)
}

const CONFIG_TEXT: &str = r#"# OmpDeck generated tmux.conf — rewritten at startup; do not edit
# -q on options newer than tmux 3.2 (extended-keys-format 3.5, allow-passthrough 3.3): older tmux ignores them
set -g status off
set -g prefix None
set -g prefix2 None
unbind -aq -T prefix
set -s escape-time 0
set -g mouse on
unbind -nq MouseDown3Pane
unbind -nq M-MouseDown3Pane
unbind -nq MouseDown3Status
unbind -nq MouseDown3StatusLeft
unbind -nq M-MouseDown3Status
unbind -nq M-MouseDown3StatusLeft
set -g history-limit 50000
set -s extended-keys on
set -sq extended-keys-format csi-u
set -s set-clipboard on
set -gq allow-passthrough on
set -g focus-events on
set -g set-titles on
set -g set-titles-string '#T'
set -g default-terminal tmux-256color
set -s terminal-features[100] 'xterm-256color:RGB:extkeys:clipboard:hyperlinks:usstyle:sync:focus:title'
set -g allow-rename off
set -g exit-empty on
# copy-mode: wheel scroll + mouse selection only; any key cancels copy-mode and is replayed to the pane
set -g mode-keys emacs
unbind -aq -T copy-mode
unbind -aq -T copy-mode-vi
bind -T copy-mode MouseDown1Pane select-pane
bind -T copy-mode MouseDrag1Pane select-pane \; send-keys -X begin-selection
bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel
bind -T copy-mode WheelDownPane select-pane \; send-keys -X -N 5 scroll-down
bind -T copy-mode WheelUpPane select-pane \; send-keys -X -N 5 scroll-up
bind -T copy-mode DoubleClick1Pane select-pane \; send-keys -X select-word \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel
bind -T copy-mode TripleClick1Pane select-pane \; send-keys -X select-line \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel
bind -T copy-mode Any if -F '#{!=:#{mouse_x},}' '' { send-keys -X cancel ; send-keys }
"#;

/// The generated tmux.conf: makes tmux invisible (no status bar, no prefix), passes through
/// everything omp needs (extended keys, clipboard, hyperlinks, focus, titles) and puts omp's
/// image/hyperlink overrides into the global environment (new sessions of an already running
/// server get them). Idempotent under repeated `source-file` (`-q` unbinds, indexed
/// terminal-features, `set-environment` overwrites).
pub fn config_text() -> String {
    let mut text = CONFIG_TEXT.to_owned();
    for (k, v) in OMP_ENV {
        text.push_str(&format!("set-environment -g {k} {v}\n"));
    }
    text
}
