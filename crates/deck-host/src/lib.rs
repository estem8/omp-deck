//! OmpDeck session host: keeps a session's program running in a PTY (Unix) or ConPTY (Windows)
//! owned by a per-session daemon (`omp-deck-host serve`), so the GUI can detach and re-attach
//! (tmux replacement). Contract: `docs/notes/contracts.md` "Wave 12".
//!
//! Client side (blocking; run it on worker threads): [`Namespace`] lists, inspects and kills
//! sessions, [`create_or_attach`] starts a daemon when needed and returns an [`Attached`]
//! connection that splits into [`AttachedRx`] / [`AttachedTx`]. The attached client is the
//! session's single controller (input, resize, palette); a new attach displaces it.
//! [`observe`] is a read-only, non-displacing subscription ([`ObserverRx`] /
//! [`ObserverTx`]) for any number of additional viewers; the daemon enforces read-only.

mod client;
pub mod daemon;
mod error;
mod logfile;
mod namespace;
pub mod protocol;
pub mod pty;
pub mod run_state;
pub mod snapshot;
mod spawn;
pub mod transport;
#[cfg(windows)]
mod winproc;

pub use client::{
    Attached, AttachedRx, AttachedTx, HostEvent, Observed, ObserverEvent, ObserverRx, ObserverTx,
    attach, create_or_attach, daemon_exe, observe, peek, start, tsp_peek,
};
pub use error::{Error, Result};
pub use namespace::{Namespace, valid_namespace, valid_session_name};
pub use protocol::{AttachParams, Grid, HostInfo, PROTOCOL, Palette, SessionSpec};
pub use run_state::{BlockedKind, RunPhase, RunState, TspFacts};
pub use spawn::{Launcher, launch_detached};

/// Version of this crate, reported in `Hello` and [`HostInfo::host_version`].
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment override for [`daemon_exe`].
pub const HOST_EXE_ENV: &str = "OMP_DECK_HOST_EXE";
