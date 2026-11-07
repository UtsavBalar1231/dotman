//! Dotfiles manager with git-like semantics.

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod cli;
pub mod git;
pub mod output;
#[allow(unsafe_code)]
pub mod sys;

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
