//! The fleet: everything around the bots that is not playing the game.
//!
//! - `store`: the shared SQLite results store.
//! - `plan`: the experiment plan and the arm a bot plays against a target opponent.
//! - `stats`: records, intervals and the tests the release pipeline uses.
//! - `collect`: the replay collector and the corpus index.
//! - `config`: per-bot settings from `fleet/bots.json`.
//! - `ingest`: backfill from `marshal` logs.
//! - `supervise`: `fleet run <bot>`, the process launchd keeps alive for each bot.
//! - `release`: build from a commit, arena gate, ladder evidence, promotion, hot swap.
//!
//! The bot binary links this crate only for `plan` and `store`; nothing here knows about
//! the search.

pub mod collect;
pub mod config;
pub mod ingest;
pub mod plan;
pub mod release;
pub mod stats;
pub mod store;
pub mod supervise;
