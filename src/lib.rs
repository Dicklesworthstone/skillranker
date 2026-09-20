//! Pure contracts for SkillRanker. Effects belong at explicit application boundaries.
#![forbid(unsafe_code)]

pub mod adapter;
pub mod authorized_read;
pub mod blocking;
pub mod cache;
pub mod capabilities;
pub mod cli;
pub mod config;
pub mod context;
pub mod demo;
pub mod effects;
pub mod eligibility;
pub mod evaluation;
pub mod identity;
pub mod installer;
pub mod jev;
pub mod limits;
pub mod output;
pub mod pipeline;
mod platform_path;
pub mod privacy;
pub mod readiness;
pub mod replay;
pub mod roster;
pub mod runtime;
pub mod scoring;
pub mod sqlite_engine;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod storage;
pub mod subprocess;
