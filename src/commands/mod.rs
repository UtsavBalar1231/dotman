//! One module per subcommand.

pub mod add;
pub mod apply;
pub mod diff;
pub mod doctor;
pub mod edit;
pub mod forget;
pub mod git;
pub mod import;
pub mod init;
pub mod key;
pub mod list;
pub mod log;
pub mod restore;
pub mod status;
pub mod sync;
pub mod wizard;

/// How a command that hit no error ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Done,
    /// Exit 3: paths were left undecided, blocked or reported for action.
    Attention,
    /// Exit 4: a rebase conflict was found and aborted.
    Conflict,
}
