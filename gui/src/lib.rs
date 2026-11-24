#![deny(unsafe_code)]

/// The CLI's bounded status transport, retained on the trusted side.
pub mod protocol;

/// Projects terminal-only recovery steps into display rows.
pub mod recovery;

/// Encodes a checked outgoing review for a later trusted writer.
pub mod decisions;
/// Validates held-file remedy reviews and receipts.
pub mod held;
/// Validates native-only held encryption reviews.
pub mod held_encrypt;
/// Validates exact-key forget reviews and receipts.
pub mod held_forget;
pub mod hook_trust;
/// Validates standalone directory and ignore trust reviews.
pub mod policy_trust;

/// Parses reviewed writer receipts on the trusted side.
pub mod writer_outcome;

/// Runs the fixed read-only CLI command.
pub mod runner;

/// Validates a guarded setup intent and its receipt.
pub mod setup;

#[allow(unsafe_code)]
mod sys;

/// Exchanges the existing source and its prepared replacement atomically.
pub use sys::{exchange_files, native_ui_font};
