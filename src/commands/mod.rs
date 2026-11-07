//! One module per subcommand.

pub mod add;
pub mod apply;
pub mod diff;
pub mod forget;
pub mod init;
pub mod list;
pub mod status;

/// How a command that hit no error ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Done,
    /// Exit 3: paths were left undecided, blocked or reported for action.
    Attention,
}
