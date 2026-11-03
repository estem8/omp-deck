//! GUI-free core of OmpDeck: omp session store scanning, session backends (tmux / OmpDeck host),
//! app config, and the per-OS glue in [`platform`].

pub mod agent_defs;
pub mod agents;
pub mod backup;
pub mod changes;
pub mod config;
pub mod ctl;
pub mod fleet;
pub mod folders;
pub(crate) mod git;
pub mod inventory;
pub mod mux;
pub mod omp;
pub mod pins;
pub mod platform;
pub mod selfupdate;
pub mod sessions;
pub mod sharpshooter;
pub mod status;
pub mod theme;
pub mod tmux;
pub mod tree;
pub mod usage;
pub mod worktree;
