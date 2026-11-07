//! Dotfiles manager with git-like semantics.

#![warn(missing_docs)]
#![deny(unsafe_code)]
// reason: owner rule (2026-11-07): docs only where they carry information; `# Errors` sections would restate anyhow contexts.
#![allow(clippy::missing_errors_doc)]

pub mod cli;
pub mod git;
pub mod output;
#[allow(unsafe_code)]
pub mod sys;

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
