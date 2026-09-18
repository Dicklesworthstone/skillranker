//! Pure contracts for SkillRanker. Effects belong at explicit application boundaries.
#![forbid(unsafe_code)]

pub mod adapter;
pub mod authorized_read;
pub mod blocking;
pub mod cli;
pub mod config;
pub mod context;
pub mod eligibility;
pub mod identity;
pub mod jev;
pub mod limits;
pub mod output;
pub mod privacy;
pub mod roster;
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod storage;
pub mod subprocess;
