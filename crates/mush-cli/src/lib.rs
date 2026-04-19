//! mush cli crate - exposes the config + helpers shared between the
//! `mush` binary and the schema-emitter bin. keep this thin: the
//! actual program lives in `src/main.rs`

pub mod commands;
pub mod config;
pub mod logging;
pub mod setup;
pub mod timing;
