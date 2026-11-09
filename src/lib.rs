//! Dotfiles manager with git-like semantics.

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub(crate) mod bootstrap;
pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub(crate) mod engine;
pub(crate) mod git;
pub(crate) mod hooks;
pub(crate) mod identity;
pub(crate) mod import;
pub mod lock;
pub mod name;
pub mod output;
pub mod paths;
pub(crate) mod proc;
pub(crate) mod prompt;
pub(crate) mod remote;
pub mod repo_config;
pub(crate) mod secrets;
pub mod state;
#[allow(unsafe_code)]
pub mod sys;
pub mod system;
pub(crate) mod template;
pub(crate) mod variant;

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
