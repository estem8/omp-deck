//! Resume mode of the session manager (omp's `/resume` picker, natively): which source is listed,
//! the project scope and filter, the pick, and the small state machine of what the primary
//! action is doing (resume into the origin tab, with the busy confirmation, or import a Claude
//! Code / Codex session first). Pure logic — [`crate::sessions_view`] paints it and
//! `app::session_manager` runs the gateway requests.

use std::path::{Path, PathBuf};

use deck_core::omp::{ForeignSession, ForeignSource};
use deck_service::controller::{Delivery, Refusal, Rejected};

/// Where the listed sessions come from: omp's own store, or another coding agent's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    Omp,
    Foreign(ForeignSource),
}

impl Source {
    pub const ALL: [Source; 3] = [
        Source::Omp,
        Source::Foreign(ForeignSource::Claude),
        Source::Foreign(ForeignSource::Codex),
    ];

    pub fn of(foreign: Option<ForeignSource>) -> Source {
        foreign.map_or(Source::Omp, Source::Foreign)
    }

    pub fn foreign(self) -> Option<ForeignSource> {
        match self {
            Source::Omp => None,
            Source::Foreign(f) => Some(f),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Omp => "omp",
            Source::Foreign(ForeignSource::Claude) => "Claude Code",
            Source::Foreign(ForeignSource::Codex) => "Codex",
        }
    }
}

/// The foreign list of one source as far as the gateway got.
#[derive(Clone, Debug, Default)]
pub enum ForeignLoad {
    #[default]
    Idle,
    Loading,
    Ready(Vec<ForeignSession>),
    Failed(String),
}

/// The list entry the user picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// An omp session, by its file.
    Session(PathBuf),
    /// A foreign session, by its own id.
    Foreign(String),
}

/// What the primary action is doing now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Flow {
    #[default]
    Idle,
    /// `ResumeHere` is on its way (`force`: the running turn is stopped first).
    Resuming { file: PathBuf, force: bool },
    /// omp is mid-turn: the user is being asked whether resuming may stop it.
    AskingForce { file: PathBuf },
    /// A foreign session is being imported; `then` says what follows.
    Importing { id: String, then: Then },
    /// An imported session is waiting to show up in the list, then opens in a tab.
    Listing { file: PathBuf },
}

/// What happens with an imported session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Resume it into the origin tab.
    ResumeHere,
    /// Open it in a tab of its own.
    NewTab,
}

/// What a finished resume request means for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeNext {
    /// omp took the command: close the manager and show the tab.
    Done,
    /// A turn is running: ask the user, then [`Flow::force`].
    AskForce(PathBuf),
    Failed(String),
}

/// What a finished import means for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportNext {
    /// Resume `file` into the origin tab (the flow is now [`Flow::Resuming`]).
    ResumeHere(PathBuf),
    /// Open `file` in a new tab.
    NewTab(PathBuf),
    Failed(String),
}

/// Why a resume request did not succeed. Busy is the gateway's typed refusal because omp is
/// mid-turn (nothing was sent); it is the only failure that asks the user a question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Busy,
    Other(String),
}

impl Failure {
    /// What to show the user.
    pub fn text(self) -> String {
        match self {
            Failure::Busy => Rejected::Busy.to_string(),
            Failure::Other(text) => text,
        }
    }
}

impl From<crate::gw::MutateError> for Failure {
    fn from(e: crate::gw::MutateError) -> Self {
        use crate::gw::MutateError;
        match e {
            MutateError::Rejected(Rejected::Busy) => Failure::Busy,
            MutateError::Rejected(other) => Failure::Other(other.to_string()),
            MutateError::Failed(text) => Failure::Other(text),
        }
    }
}

/// The end of a typed command (`/resume …`): `Ok` when omp's terminal took it, else why not.
pub fn delivery_result(d: &Delivery) -> Result<(), String> {
    let why = match d {
        Delivery::Delivered | Delivery::Unobserved => return Ok(()),
        Delivery::Refused(Refusal::Precondition(_)) => {
            "omp was not ready for the command (it is working or waiting for an answer)"
        }
        Delivery::Refused(Refusal::DialogOpen) => "an omp dialog is open in the tab",
        Delivery::Refused(Refusal::NoBracketedPaste) => "omp is not ready to take input yet",
        Delivery::Declined => "the unsent draft in the tab was kept",
        Delivery::Aborted => "omp started working before the command went in",
        Delivery::Stalled => "omp did not react in time; the command may sit in its input",
        Delivery::Cancelled(_) => "the tab closed or its session changed first",
        Delivery::Queued | Delivery::AwaitingConfirm { .. } | Delivery::Writing => {
            "the command was still being sent"
        }
    };
    Err(format!("The command was not delivered: {why}."))
}

impl Flow {
    pub fn is_idle(&self) -> bool {
        *self == Flow::Idle
    }

    /// Starts resuming `file`; only from idle.
    pub fn begin_resume(&mut self, file: PathBuf) -> bool {
        if !self.is_idle() {
            return false;
        }
        *self = Flow::Resuming { file, force: false };
        true
    }

    /// The gateway answered the resume request.
    pub fn resume_answer(&mut self, result: Result<(), Failure>) -> ResumeNext {
        let Flow::Resuming { file, force } = std::mem::take(self) else {
            return ResumeNext::Failed("no resume was waiting".to_owned());
        };
        match result {
            Ok(()) => ResumeNext::Done,
            Err(Failure::Busy) if !force => {
                *self = Flow::AskingForce { file: file.clone() };
                ResumeNext::AskForce(file)
            }
            Err(f) => ResumeNext::Failed(f.text()),
        }
    }

    /// The imported file is in the list (or the wait was given up): back to idle.
    pub fn listed(&mut self) {
        if matches!(self, Flow::Listing { .. }) {
            *self = Flow::Idle;
        }
    }

    /// Forgets whatever was in flight: its answers are stale from here on (see [`Epoch`]).
    pub fn reset(&mut self) {
        *self = Flow::Idle;
    }

    /// The busy question was answered: `Some(file)` = repeat the request with `force`.
    pub fn force(&mut self, confirmed: bool) -> Option<PathBuf> {
        let Flow::AskingForce { file } = std::mem::take(self) else {
            return None;
        };
        if !confirmed {
            return None;
        }
        *self = Flow::Resuming {
            file: file.clone(),
            force: true,
        };
        Some(file)
    }

    /// Starts importing foreign session `id`; only from idle.
    pub fn begin_import(&mut self, id: String, then: Then) -> bool {
        if !self.is_idle() {
            return false;
        }
        *self = Flow::Importing { id, then };
        true
    }

    /// The import finished with the new session file (or why not).
    pub fn import_answer(&mut self, result: Result<PathBuf, String>) -> ImportNext {
        let Flow::Importing { then, .. } = std::mem::take(self) else {
            return ImportNext::Failed("no import was waiting".to_owned());
        };
        match (result, then) {
            (Err(e), _) => ImportNext::Failed(e),
            (Ok(file), Then::NewTab) => {
                *self = Flow::Listing { file: file.clone() };
                ImportNext::NewTab(file)
            }
            (Ok(file), Then::ResumeHere) => {
                *self = Flow::Resuming {
                    file: file.clone(),
                    force: false,
                };
                ImportNext::ResumeHere(file)
            }
        }
    }
}

/// The folder an imported session gets as its project (omp's `fallbackCwd` rule: the foreign
/// session keeps its own folder while that exists; otherwise the session lands in the project
/// the resume is for — the origin tab's, else the listed one). With neither known the foreign
/// folder is passed on and omp says what is wrong.
pub fn import_cwd(
    foreign_cwd: &str,
    origin: Option<&Path>,
    scope: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let own = Path::new(foreign_cwd);
    if !foreign_cwd.is_empty() && exists(own) {
        return own.to_path_buf();
    }
    origin
        .or(scope)
        .map_or_else(|| own.to_path_buf(), Path::to_path_buf)
}

/// Indices into `list` of the foreign sessions to show: those whose folder is `scope` (`None` =
/// every project) and that match `text` (title, folder, first message, id), newest first.
pub fn foreign_order(list: &[ForeignSession], scope: Option<&Path>, text: &str) -> Vec<usize> {
    let needle = text.trim().to_lowercase();
    let mut out: Vec<usize> = (0..list.len())
        .filter(|&i| {
            let s = &list[i];
            scope.is_none_or(|p| Path::new(&s.cwd) == p)
                && (needle.is_empty()
                    || [
                        s.title.as_deref(),
                        Some(s.cwd.as_str()),
                        s.first_message.as_deref(),
                        Some(s.id.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|t| t.to_lowercase().contains(&needle)))
        })
        .collect();
    out.sort_by(|&a, &b| {
        list[b]
            .modified
            .cmp(&list[a].modified)
            .then_with(|| list[a].id.cmp(&list[b].id))
    });
    out
}

/// What the order list was computed for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OrderKey {
    rev: u64,
    source: Source,
    all: bool,
    scope: Option<PathBuf>,
    text: String,
}

/// The view-local state of resume mode.
#[derive(Debug, Default)]
pub struct ResumeState {
    pub source: Source,
    /// List every project instead of the current one.
    pub all_projects: bool,
    /// The current project (the folder `/resume` was asked in); `None` = nothing to scope to.
    pub project: Option<PathBuf>,
    pub filter: String,
    pub pick: Option<Pick>,
    pub flow: Flow,
    /// The last failure, shown above the list until the next action.
    pub error: Option<String>,
    /// Focus the filter box on the next frame.
    pub focus_filter: bool,
    claude: ForeignLoad,
    codex: ForeignLoad,
    /// Bumped whenever a foreign list changes.
    foreign_rev: u64,
    /// Scroll the list to this offset on the next frame (the pick moved off screen).
    pub scroll_to: Option<f32>,
    /// The list's scroll offset and height as last drawn.
    pub viewport: (f32, f32),
    order: Vec<usize>,
    order_key: Option<OrderKey>,
}

impl ResumeState {
    /// Fresh state for opening resume mode on `source`.
    pub fn open(&mut self, source: Source, project: Option<PathBuf>) {
        self.source = source;
        // Without a project there is nothing to scope to.
        self.all_projects = project.is_none();
        self.project = project;
        self.filter.clear();
        self.pick = None;
        self.error = None;
        self.focus_filter = true;
        self.order_key = None;
    }

    pub fn foreign(&self, src: ForeignSource) -> &ForeignLoad {
        match src {
            ForeignSource::Claude => &self.claude,
            ForeignSource::Codex => &self.codex,
        }
    }

    pub fn set_foreign(&mut self, src: ForeignSource, load: ForeignLoad) {
        match src {
            ForeignSource::Claude => self.claude = load,
            ForeignSource::Codex => self.codex = load,
        }
        self.foreign_rev += 1;
    }

    fn order_fresh(&self, rev: u64) -> bool {
        let scope = self.scope();
        self.order_key.as_ref().is_some_and(|k| {
            k.rev == rev
                && k.source == self.source
                && k.all == self.all_projects
                && k.scope.as_deref() == scope
                && k.text == self.filter
        })
    }

    fn order_stamp(&mut self, rev: u64) {
        self.order_key = Some(OrderKey {
            rev,
            source: self.source,
            all: self.all_projects,
            scope: self.scope().map(Path::to_path_buf),
            text: self.filter.clone(),
        });
    }

    /// Makes the list of omp sessions (indices into the model's rows) for the current scope and
    /// filter current; recomputed only when an input changed. `rev` is the model's revision;
    /// `compute` gets the scope (`None` = every project) and the filter text.
    pub fn ensure_order(
        &mut self,
        rev: u64,
        compute: impl FnOnce(Option<&Path>, &str) -> Vec<usize>,
    ) {
        if !self.order_fresh(rev) {
            self.order = compute(self.scope(), &self.filter);
            self.order_stamp(rev);
        }
    }

    /// [`ensure_order`](Self::ensure_order) for the foreign list of `src` (indices into it).
    pub fn ensure_foreign_order(&mut self, src: ForeignSource) {
        let rev = self.foreign_rev;
        if !self.order_fresh(rev) {
            let list: &[ForeignSession] = match self.foreign(src) {
                ForeignLoad::Ready(l) => l,
                _ => &[],
            };
            self.order = foreign_order(list, self.scope(), &self.filter);
            self.order_stamp(rev);
        }
    }

    /// The list [`ensure_order`](Self::ensure_order) made.
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    /// The project the list is scoped to now (`None` = every project).
    pub fn scope(&self) -> Option<&Path> {
        if self.all_projects {
            None
        } else {
            self.project.as_deref()
        }
    }

    /// Switches the source; the pick is dropped (it names an entry of the other list).
    pub fn set_source(&mut self, source: Source) {
        if self.source != source {
            self.source = source;
            self.pick = None;
            self.error = None;
        }
    }
}

/// Which opening of the session manager a request belongs to. Every close and every opening
/// advances it; an answer that arrives under another epoch than it was asked in is stale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Epoch(u64);

impl Epoch {
    pub fn advance(&mut self) {
        self.0 += 1;
    }
}

/// A resume request's answer, applied to `flow` only when it was asked in the epoch `now`;
/// `None` = stale (nothing changed).
pub fn settle_resume(
    now: Epoch,
    asked: Epoch,
    flow: &mut Flow,
    result: Result<(), Failure>,
) -> Option<ResumeNext> {
    (now == asked).then(|| flow.resume_answer(result))
}

/// What an import's answer means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportSettled {
    /// Asked in this opening of the manager: follow the flow.
    Live(ImportNext),
    /// Asked in an earlier opening: the import happened (or failed), nothing follows from it.
    Stale(Result<PathBuf, String>),
}

pub fn settle_import(
    now: Epoch,
    asked: Epoch,
    flow: &mut Flow,
    result: Result<PathBuf, String>,
) -> ImportSettled {
    if now == asked {
        ImportSettled::Live(flow.import_answer(result))
    } else {
        ImportSettled::Stale(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn foreign(id: &str, cwd: &str, modified: i64, title: Option<&str>) -> ForeignSession {
        ForeignSession {
            source: ForeignSource::Claude,
            id: id.to_owned(),
            path: PathBuf::from(format!("/c/{id}.jsonl")),
            cwd: cwd.to_owned(),
            title: title.map(str::to_owned),
            created: 0,
            modified,
            message_count: Some(2),
            first_message: Some(format!("first of {id}")),
        }
    }

    #[test]
    fn only_the_typed_busy_refusal_asks_the_question() {
        use crate::gw::MutateError;
        assert_eq!(
            Failure::from(MutateError::Rejected(Rejected::Busy)),
            Failure::Busy
        );
        // Another refusal, or text that merely reads like the busy one, is a plain failure.
        assert_eq!(
            Failure::from(MutateError::Rejected(Rejected::Exited)),
            Failure::Other(Rejected::Exited.to_string())
        );
        assert_eq!(
            Failure::from(MutateError::Failed(Rejected::Busy.to_string())),
            Failure::Other(Rejected::Busy.to_string())
        );
    }

    #[test]
    fn a_busy_refusal_asks_then_repeats_with_force() {
        let mut f = Flow::Idle;
        assert!(f.begin_resume(PathBuf::from("/s/a.jsonl")));
        assert!(
            !f.begin_resume(PathBuf::from("/s/b.jsonl")),
            "one at a time"
        );
        assert_eq!(
            f.resume_answer(Err(Failure::Busy)),
            ResumeNext::AskForce(PathBuf::from("/s/a.jsonl"))
        );
        assert_eq!(
            f,
            Flow::AskingForce {
                file: PathBuf::from("/s/a.jsonl")
            }
        );
        // Confirmed: the same file again, now forced.
        assert_eq!(f.force(true), Some(PathBuf::from("/s/a.jsonl")));
        assert_eq!(
            f,
            Flow::Resuming {
                file: PathBuf::from("/s/a.jsonl"),
                force: true
            }
        );
        // A forced request that is still refused as busy is a failure, not another question.
        assert_eq!(
            f.resume_answer(Err(Failure::Busy)),
            ResumeNext::Failed(Rejected::Busy.to_string())
        );
        assert!(f.is_idle());
    }

    #[test]
    fn declining_the_busy_question_changes_nothing() {
        let mut f = Flow::Idle;
        f.begin_resume(PathBuf::from("/s/a.jsonl"));
        f.resume_answer(Err(Failure::Busy));
        assert_eq!(f.force(false), None);
        assert!(f.is_idle());
        // Nothing is waiting any more.
        assert_eq!(f.force(true), None);
    }

    #[test]
    fn an_idle_resume_finishes_and_other_failures_are_reported() {
        let mut f = Flow::Idle;
        f.begin_resume(PathBuf::from("/s/a.jsonl"));
        assert_eq!(f.resume_answer(Ok(())), ResumeNext::Done);
        assert!(f.is_idle());
        f.begin_resume(PathBuf::from("/s/a.jsonl"));
        assert_eq!(
            f.resume_answer(Err(Failure::Other(
                "omp has exited in this session".to_owned()
            ))),
            ResumeNext::Failed("omp has exited in this session".to_owned())
        );
        assert!(f.is_idle());
    }

    #[test]
    fn an_imported_session_is_resumed_through_the_same_busy_path() {
        let mut f = Flow::Idle;
        assert!(f.begin_import("claude-1".to_owned(), Then::ResumeHere));
        assert!(!f.begin_resume(PathBuf::from("/s/x.jsonl")), "importing");
        assert_eq!(
            f.import_answer(Ok(PathBuf::from("/s/new.jsonl"))),
            ImportNext::ResumeHere(PathBuf::from("/s/new.jsonl"))
        );
        assert_eq!(
            f,
            Flow::Resuming {
                file: PathBuf::from("/s/new.jsonl"),
                force: false
            }
        );
        // The new session can meet a running turn like any other: the question follows.
        assert_eq!(
            f.resume_answer(Err(Failure::Busy)),
            ResumeNext::AskForce(PathBuf::from("/s/new.jsonl"))
        );
    }

    #[test]
    fn an_imported_session_can_open_in_a_new_tab_and_a_failed_import_ends() {
        let mut f = Flow::Idle;
        f.begin_import("c".to_owned(), Then::NewTab);
        assert_eq!(
            f.import_answer(Ok(PathBuf::from("/s/new.jsonl"))),
            ImportNext::NewTab(PathBuf::from("/s/new.jsonl"))
        );
        assert!(!f.is_idle(), "the file is awaited in the list first");
        f.listed();
        assert!(f.is_idle(), "opening in a tab needs nothing more");
        f.begin_import("c".to_owned(), Then::ResumeHere);
        assert_eq!(
            f.import_answer(Err("no such session".to_owned())),
            ImportNext::Failed("no such session".to_owned())
        );
        assert!(f.is_idle());
    }

    #[test]
    fn an_import_keeps_the_foreign_folder_while_it_exists() {
        let exists = |p: &Path| p == Path::new("/work/app");
        let origin = Some(Path::new("/work/other"));
        let scope = Some(Path::new("/work/scope"));
        assert_eq!(
            import_cwd("/work/app", origin, scope, exists),
            PathBuf::from("/work/app")
        );
        // Gone: the origin tab's project, else the listed project, else it is passed on.
        assert_eq!(
            import_cwd("/gone", origin, scope, exists),
            PathBuf::from("/work/other")
        );
        assert_eq!(
            import_cwd("/gone", None, scope, exists),
            PathBuf::from("/work/scope")
        );
        assert_eq!(
            import_cwd("/gone", None, None, exists),
            PathBuf::from("/gone")
        );
        assert_eq!(
            import_cwd("", origin, scope, exists),
            PathBuf::from("/work/other")
        );
    }

    #[test]
    fn foreign_sessions_are_newest_first_scoped_and_filterable() {
        let list = [
            foreign("old", "/p", 100, Some("Old one")),
            foreign("new", "/p", 300, None),
            foreign("elsewhere", "/q", 500, Some("Other project")),
        ];
        assert_eq!(foreign_order(&list, None, ""), vec![2, 1, 0]);
        assert_eq!(foreign_order(&list, Some(Path::new("/p")), ""), vec![1, 0]);
        // Title, folder, first message and id all match.
        assert_eq!(foreign_order(&list, None, "OLD ONE"), vec![0]);
        assert_eq!(foreign_order(&list, None, "first of new"), vec![1]);
        assert_eq!(foreign_order(&list, None, "/q"), vec![2]);
        assert!(foreign_order(&list, None, "nothing like it").is_empty());
    }

    #[test]
    fn the_order_is_recomputed_only_when_an_input_changed() {
        let mut s = ResumeState {
            project: Some(PathBuf::from("/p")),
            ..ResumeState::default()
        };
        let runs = std::cell::Cell::new(0);
        let ask = |s: &mut ResumeState, rev: u64| {
            s.ensure_order(rev, |_, _| {
                runs.set(runs.get() + 1);
                vec![0]
            });
        };
        ask(&mut s, 1);
        ask(&mut s, 1);
        assert_eq!(runs.get(), 1, "unchanged inputs reuse the list");
        ask(&mut s, 2);
        assert_eq!(runs.get(), 2, "a new model revision recomputes");
        s.filter.push('x');
        ask(&mut s, 2);
        assert_eq!(runs.get(), 3, "a filter edit recomputes");
        s.all_projects = true;
        ask(&mut s, 2);
        assert_eq!(runs.get(), 4, "the scope toggle recomputes");
        s.set_source(Source::Foreign(ForeignSource::Claude));
        ask(&mut s, 2);
        assert_eq!(runs.get(), 5, "another source recomputes");
    }

    #[test]
    fn the_foreign_order_follows_its_list_and_scope() {
        let mut s = ResumeState::default();
        s.open(
            Source::Foreign(ForeignSource::Claude),
            Some(PathBuf::from("/p")),
        );
        s.ensure_foreign_order(ForeignSource::Claude);
        assert!(s.order().is_empty(), "nothing is listed before the read");
        s.set_foreign(
            ForeignSource::Claude,
            ForeignLoad::Ready(vec![
                foreign("a", "/p", 100, None),
                foreign("b", "/q", 200, None),
                foreign("c", "/p", 300, None),
            ]),
        );
        s.ensure_foreign_order(ForeignSource::Claude);
        assert_eq!(s.order(), [2, 0], "the current project, newest first");
        s.all_projects = true;
        s.ensure_foreign_order(ForeignSource::Claude);
        assert_eq!(s.order(), [2, 1, 0]);
    }

    #[test]
    fn opening_on_a_source_starts_fresh() {
        let mut s = ResumeState {
            filter: "old".to_owned(),
            all_projects: true,
            pick: Some(Pick::Foreign("x".to_owned())),
            ..ResumeState::default()
        };
        s.open(
            Source::of(Some(ForeignSource::Codex)),
            Some(PathBuf::from("/p")),
        );
        assert_eq!(s.source, Source::Foreign(ForeignSource::Codex));
        assert!(s.filter.is_empty() && !s.all_projects && s.pick.is_none());
        assert_eq!(s.scope(), Some(Path::new("/p")));
        // No project: every project is listed, there is nothing to scope to.
        s.open(Source::Omp, None);
        assert!(s.all_projects && s.scope().is_none());
        assert_eq!(Source::of(None), Source::Omp);
        assert_eq!(
            Source::ALL.map(Source::label),
            ["omp", "Claude Code", "Codex"]
        );
    }

    #[test]
    fn a_typed_command_counts_as_resumed_only_when_the_terminal_took_it() {
        assert_eq!(delivery_result(&Delivery::Delivered), Ok(()));
        assert_eq!(delivery_result(&Delivery::Unobserved), Ok(()));
        for d in [
            Delivery::Refused(Refusal::DialogOpen),
            Delivery::Declined,
            Delivery::Aborted,
            Delivery::Stalled,
        ] {
            let e = delivery_result(&d).unwrap_err();
            assert!(e.starts_with("The command was not delivered"), "{e}");
        }
    }

    #[test]
    fn a_resume_answer_from_a_closed_manager_changes_nothing() {
        let mut epoch = Epoch::default();
        let mut flow = Flow::Idle;
        assert!(flow.begin_resume(PathBuf::from("/s/a.jsonl")));
        let asked = epoch;
        // The manager closes (and is opened again): the old request is no longer its business.
        epoch.advance();
        flow.reset();
        assert!(flow.begin_resume(PathBuf::from("/s/b.jsonl")));
        assert_eq!(settle_resume(epoch, asked, &mut flow, Ok(())), None);
        assert_eq!(
            flow,
            Flow::Resuming {
                file: PathBuf::from("/s/b.jsonl"),
                force: false
            },
            "the new request is untouched"
        );
        // A stale busy refusal does not open a question either.
        assert_eq!(
            settle_resume(epoch, asked, &mut flow, Err(Failure::Busy)),
            None
        );
        // The current one settles as usual.
        assert_eq!(
            settle_resume(epoch, epoch, &mut flow, Ok(())),
            Some(ResumeNext::Done)
        );
    }

    #[test]
    fn an_import_that_ends_after_the_manager_closed_resumes_nothing() {
        let mut epoch = Epoch::default();
        let mut flow = Flow::Idle;
        assert!(flow.begin_import("claude-1".to_owned(), Then::ResumeHere));
        let asked = epoch;
        epoch.advance();
        flow.reset();
        assert!(flow.is_idle(), "a closed manager forgets its waits");
        // Reopened in another tab and idle: the late answer is only reported.
        let late = settle_import(epoch, asked, &mut flow, Ok(PathBuf::from("/s/new.jsonl")));
        assert_eq!(
            late,
            ImportSettled::Stale(Ok(PathBuf::from("/s/new.jsonl")))
        );
        assert!(flow.is_idle(), "nothing was resumed or awaited");
        // The same while another import is running in the reopened manager.
        assert!(flow.begin_import("claude-2".to_owned(), Then::ResumeHere));
        let late = settle_import(epoch, asked, &mut flow, Err("gone".to_owned()));
        assert_eq!(late, ImportSettled::Stale(Err("gone".to_owned())));
        assert_eq!(
            flow,
            Flow::Importing {
                id: "claude-2".to_owned(),
                then: Then::ResumeHere
            }
        );
        // Its own answer resumes.
        assert_eq!(
            settle_import(epoch, epoch, &mut flow, Ok(PathBuf::from("/s/two.jsonl"))),
            ImportSettled::Live(ImportNext::ResumeHere(PathBuf::from("/s/two.jsonl")))
        );
    }
}
