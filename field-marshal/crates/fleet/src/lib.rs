//! The fleet: everything around the bots that is not playing the game.
//!
//! - `store`: the shared SQLite results store.
//! - `plan`: the experiment plan and the arm a bot plays against a target opponent.
//! - `report`: the status page and the daily digest.
//! - `stats`: records, intervals and the tests the release pipeline uses.
//! - `autopilot`: the pipeline run by itself: build, gate, canary, evidence, scouts' base.
//! - `collect`: the replay collector and the corpus index.
//! - `config`: per-bot settings from `fleet/bots.json`.
//! - `ingest`: backfill from `marshal` logs.
//! - `online`: the offline watchdog (read-only: asks the ladder, raises alerts).
//! - `supervise`: `fleet run <bot>`, the process launchd keeps alive for each bot.
//! - `release`: build from a commit, arena gate, ladder evidence, promotion, hot swap.
//!
//! The bot binary links this crate only for `plan` and `store`; nothing here knows about
//! the search.

pub mod autopilot;
pub mod collect;
pub mod config;
pub mod ingest;
pub mod online;
pub mod plan;
pub mod release;
pub mod report;
pub mod stats;
pub mod store;
pub mod supervise;
