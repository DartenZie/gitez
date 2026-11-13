//! Sandboxes GitEZ runs coding agents in. A sandbox wraps an agent's whole
//! process tree so that it can write only where its policy allows and reach
//! only the hosts its [`NetworkProfile`] allows.
//!
//! Everything here blocks: run it off the UI thread.

mod network;

pub use network::NetworkProfile;
