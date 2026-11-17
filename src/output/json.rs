//! Experimental NDJSON transport; command reports own their explicit projections.

use super::escape_line;
use crate::commands::status::Observation;
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Stdio;

pub(crate) fn stored(root: &Path, path: &Path) -> Result<Observation> {
    Ok(crate::commands::review::Stored::read(root, path)?.observation())
}

pub(crate) fn child_stdout() -> Result<Stdio> {
    Ok(std::io::stderr().as_fd().try_clone_to_owned()?.into())
}

#[derive(Serialize)]
pub(crate) struct PhaseEvent {
    pub record: &'static str,
    pub stage: &'static str,
    pub dry_run: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct UndecidedEvent {
    pub record: &'static str,
    pub reason: &'static str,
    pub key: Option<Text>,
    pub scope: &'static str,
    pub pass: &'static str,
    pub attention: bool,
    pub skip_next_pass: bool,
}

/// A terminal recovery recipe, not an executable IPC request.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryStep {
    /// Literal argv, including the program. Path arguments retain their Unix bytes.
    Command {
        /// Unquoted arguments, in execution order.
        argv: Vec<Argument>,
        /// Escaped display only; not arguments to execute.
        shown: Vec<String>,
        /// When this command is appropriate; absent for an unconditional step.
        when: Option<String>,
    },
    /// A manual action or a constraint on the following commands.
    Instruction {
        /// Plain explanatory text with raw/shown framing.
        text: Text,
    },
}

/// An argv element uses the same byte-preserving representation as a reported path.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Argument {
    /// An exact UTF-8 argument, without shell quoting.
    Text(String),
    /// A non-UTF-8 Unix argument.
    Bytes {
        /// Original bytes, without shell quoting.
        bytes: Vec<u8>,
    },
}

impl RecoveryStep {
    pub(crate) fn command(args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> Self {
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        Self::Command {
            argv: args
                .iter()
                .map(|arg| {
                    arg.to_str().map_or_else(
                        || Argument::Bytes {
                            bytes: arg.as_bytes().to_vec(),
                        },
                        |arg| Argument::Text(arg.to_owned()),
                    )
                })
                .collect(),
            shown: args
                .iter()
                .map(|arg| escape_line(&arg.to_string_lossy()).into_owned())
                .collect(),
            when: None,
        }
    }

    pub(crate) fn when(mut self, condition: &str) -> Self {
        if let Self::Command { when, .. } = &mut self {
            *when = Some(condition.to_owned());
        }
        self
    }

    pub(crate) fn instruction(text: &str) -> Self {
        Self::Instruction {
            text: Text::new(text),
        }
    }
}

/// An already-reported pull stop; `main` maps it to exit 4 without printing it twice.
#[derive(Debug)]
pub struct PullConflict {
    /// The source-built terminal recipe.
    pub recovery: Vec<RecoveryStep>,
}

impl std::fmt::Display for PullConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the pull needs terminal recovery")
    }
}

impl std::error::Error for PullConflict {}

/// Exact UTF-8 metadata paired with terminal-safe display text.
#[derive(Debug, Clone, Serialize)]
pub struct Text {
    raw: String,
    shown: String,
}

impl Text {
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let shown = escape_line(&raw).into_owned();
        Self { raw, shown }
    }

    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PathValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<Vec<u8>>,
    shown: String,
}

impl PathValue {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            raw: path.to_str().map(str::to_owned),
            bytes: path
                .to_str()
                .is_none()
                .then(|| path.as_os_str().as_bytes().to_vec()),
            shown: escape_line(&path.to_string_lossy()).into_owned(),
        }
    }
}

pub(crate) fn write(out: &mut impl Write, record: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, record).context("cannot write JSON to stdout")?;
    writeln!(out).context("cannot write to stdout")
}

/// Writes the single terminal record after `main` has mapped the process result.
///
/// # Errors
/// Fails if stdout cannot be written.
pub fn finish(exit_code: i32, error_kind: Option<&str>) -> Result<()> {
    finish_with_recovery(exit_code, error_kind, None)
}

/// Writes a final result with a terminal-only recovery recipe.
///
/// # Errors
/// Fails if stdout cannot be written.
pub fn finish_with_recovery(
    exit_code: i32,
    error_kind: Option<&str>,
    recovery: Option<&[RecoveryStep]>,
) -> Result<()> {
    #[derive(Serialize)]
    struct ResultRecord<'a> {
        record: &'static str,
        outcome: &'static str,
        exit_code: i32,
        error_kind: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<&'a [RecoveryStep]>,
    }
    write(
        &mut std::io::stdout().lock(),
        &ResultRecord {
            record: "result",
            outcome: if exit_code == 0 { "done" } else { "error" },
            exit_code,
            error_kind,
            recovery,
        },
    )
}
