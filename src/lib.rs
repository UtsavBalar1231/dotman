//! Dotfiles manager with git-like semantics.

#![warn(missing_docs)]
#![deny(unsafe_code)]
// reason: owner rule (2026-11-07): docs only where they carry information; `# Errors` sections would restate anyhow contexts.
#![allow(clippy::missing_errors_doc)]

pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod engine;
pub mod git;
pub mod hooks;
pub mod identity;
pub mod import;
pub mod lock;
pub mod output;
pub mod paths;
pub mod proc;
pub mod prompt;
pub mod repo_config;
pub mod secrets;
pub mod state;
#[allow(unsafe_code)]
pub mod sys;
pub mod template;
pub mod variant;

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
