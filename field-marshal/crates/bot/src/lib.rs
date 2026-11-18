//! The ladder bot: state tracker, policy and session, shared by every bot process.
//!
//! One binary (`marshal`) is started once per bot name. Each process has its own token,
//! log and playbook choices; they share this code and the engine.

pub mod arena;
pub mod fleet_hook;
pub mod policy;
pub mod session;
pub mod tracker;
