//! deck-term: VT emulation (alacritty_terminal) + egui terminal widget, fed by a local PTY
//! (Unix), a remote session host, or — as a gateway frontend — the terminal's owner.
//!
//! Layering: `core` (Term, listener), `engine` (atomic parse + publication), `out` (byte
//! path), `pty`/`remote`/`frontend` (the child links), `scan` and `input` (pure
//! scanners/encoders) have no egui widget types; `render` and `widget` adapt them to egui.

mod clipboard;
mod core;
mod engine;
mod fonts;
mod frontend;
mod graphics;
pub mod input;
mod links;
mod out;
#[cfg(unix)]
mod pty;
mod remote;
mod render;
mod scan;
mod theme;
mod tsp;
mod wake;
mod widget;

pub use crate::clipboard::Clipboard;
#[cfg(unix)]
pub use crate::core::SpawnSpec;
pub use crate::core::{ScreenText, TermOptions, Terminal, TerminalEvents};
pub use crate::engine::{OutputEvent, OutputNotify, OutputSubscription};
pub use crate::fonts::{FontReport, install_fonts};
pub use crate::frontend::{
    ClipboardPolicy, Frontend, FrontendState, FrontendTx, InputMeta, ReplyRejected,
};
pub use crate::out::{InputProgress, wait_detached};
pub use crate::remote::{Observer, ObserverTx, Remote, RemoteEvent, RemoteRx, RemoteTx};
pub use crate::theme::{TermColors, TermTheme};
pub use crate::tsp::{TspHandle, TspRefused};
pub use crate::wake::WakeCallback;
pub use crate::widget::{TerminalResponse, TerminalView};
pub use deck_host::run_state::{BlockedKind, RunPhase, RunState, TspFacts};
