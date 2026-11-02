//! Tern Surface Protocol (TSP) core for OmpDeck: the terminal side of omp's native UI.
//!
//! Facts and decisions: `docs/notes/tsp.md`, contract `docs/notes/contracts.md` "Native chat
//! view (TSP) step 1". Everything here is pure (no threads, no I/O, no clocks of its own;
//! callers pass an [`std::time::Instant`]):
//!
//! - [`wire`]: the message shapes of omp `v18.15.0` `packages/wire/src/tsp.ts`.
//! - [`stream`]: [`stream::Splitter`], a passive incremental scanner for TSP APC strings in a
//!   PTY output stream (chunk reassembly, caps, idle/interrupt handling).
//! - [`document`]: [`document::Document`], the per-surface node trees built from `o`/`f`/`x`/
//!   `s`/`b` messages, with blobs.
//! - [`terminal`]: [`terminal::Responder`], the terminal role: hello reply, blob answers, acks
//!   and error events, plus the encoders for events written to the child's input.

pub mod document;
pub mod mirror;
pub mod stream;
pub mod terminal;
pub mod wire;
