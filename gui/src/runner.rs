//! Fixed, non-interactive subprocess transport.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const STDOUT_LIMIT: usize = 32 * 1024 * 1024;
const STDERR_LIMIT: usize = 1024 * 1024;
const PIPE_CLOSE_GRACE: Duration = Duration::from_millis(100);

/// A bounded commit message suitable for native confirmation and one CLI argument.
pub struct CommitMessage(String);

impl CommitMessage {
    /// Rejects empty, oversized, or visually unsafe drafts.
    ///
    /// # Errors
    /// Returns an error if the draft cannot be safely confirmed.
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.len() > 1024
            || value.split('\n').count() > 6
            || value.trim().is_empty()
            || value.chars().any(|c| c != '\n' && unsafe_display_char(c))
        {
            return Err("Invalid commit message");
        }
        Ok(Self(value))
    }

    /// Returns the exact validated draft.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Characters that cannot be shown literally in a native confirmation.
#[must_use]
pub fn unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

/// The bounded bytes and authoritative process status; stderr is not protocol.
pub struct Receipt {
    /// CLI stdout, to be validated by the protocol parser.
    pub stdout: Vec<u8>,
    /// Uninterpreted CLI stderr, retained on the trusted side.
    pub stderr: Vec<u8>,
    /// The child's actual exit status.
    pub status: ExitStatus,
}

/// Owns one CLI process. Polling and cancellation require exclusive access.
///
/// Dropping a live run attempts SIGTERM and hands reaping to a background thread.
/// Drop cannot report cleanup failures; a SIGTERM-resistant child may never exit.
pub struct StatusRun {
    child: Option<Child>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    out: Vec<u8>,
    err: Vec<u8>,
    out_eof: bool,
    err_eof: bool,
    cancelled: bool,
    failure: Option<io::Error>,
    input: Option<PendingInput>,
    input_delivery: InputDelivery,
    setup_parent: Option<Arc<std::fs::File>>,
    stdout_limit: usize,
    stderr_limit: usize,
    exit_observed: Option<Instant>,
}

#[derive(PartialEq, Eq)]
enum InputDelivery {
    Incomplete,
    Complete,
}

struct PendingInput {
    pipe: ChildStdin,
    bytes: Vec<u8>,
    offset: usize,
}

/// Starts the read-only CLI companion in the trusted executable directory.
///
/// # Errors
/// Returns an error if the process or its pipes cannot be created.
pub fn start_status() -> io::Result<StatusRun> {
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    command.args(["status", "--json"]);
    start(command)
}

/// Starts a guarded setup with the retained parent pinned until the child exits.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_setup(setup: &crate::setup::SetupIntent) -> io::Result<StatusRun> {
    let bytes = setup.encode().map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    setup_args(&mut command, setup);
    spawn_pinned(command, Some(bytes), Some(setup.parent()))
}

/// Starts the fixed static-import preview for a native-selected directory.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_import_preview(import: &crate::setup::ImportIntent) -> io::Result<StatusRun> {
    let bytes = import.encode_preview().map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    setup_args(&mut command, import.setup());
    spawn_pinned(command, Some(bytes), Some(import.setup().parent()))
}

/// Starts the fixed import using the retained CLI review.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_import(review: &crate::setup::ReviewedImport) -> io::Result<StatusRun> {
    let bytes = review.encode().map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    setup_args(&mut command, review.setup());
    spawn_pinned(command, Some(bytes), Some(review.setup().parent()))
}

/// Starts the fixed reviewed identity operation with a bounded request and receipt.
///
/// # Errors
/// Returns an error if the request or process cannot be prepared.
pub fn start_key(home: &Path, bytes: Vec<u8>) -> io::Result<StatusRun> {
    if bytes.len() > 65_536 {
        return Err(io::Error::other("identity request too large"));
    }
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    command.current_dir(home).args(["key", "reviewed"]);
    let mut run = spawn(command, Some(bytes))?;
    run.stdout_limit = 4096;
    run.stderr_limit = 4096;
    Ok(run)
}

/// Starts a fixed held-file review or execution.
///
/// # Errors
/// Returns an error if the request is too large or the process cannot start.
pub fn start_remedy(home: &Path, bytes: Vec<u8>) -> io::Result<StatusRun> {
    if bytes.len() > 65_536 {
        return Err(io::Error::other("remedy request too large"));
    }
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    command
        .current_dir(home)
        .args(["remedy", "--from", "-", "--json"]);
    let mut run = spawn(command, Some(bytes))?;
    run.stdout_limit = 65_536;
    run.stderr_limit = 4096;
    Ok(run)
}

fn setup_args(command: &mut Command, setup: &crate::setup::SetupIntent) {
    command
        .current_dir(&setup.context().home.path)
        .args(["init", "--setup-from", "-", "--json"]);
}

/// Starts a fixed read-only outgoing sync preview.
///
/// # Errors
/// Returns an error if the process or its pipes cannot be created.
pub fn start_review(context: &crate::protocol::StatusContext) -> io::Result<StatusRun> {
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    review_args(&mut command, context);
    start(command)
}

/// Starts a fixed local policy preflight.
///
/// # Errors
/// Returns an error if the companion cannot start.
pub fn start_policy_preflight(
    context: &crate::protocol::StatusContext,
    action: crate::policy_trust::Action,
) -> io::Result<StatusRun> {
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    command.current_dir(&context.home.path).args([
        "apply",
        "--review-trust",
        action.argument(),
        "--dry-run",
        "--json",
    ]);
    start(command)
}

/// Starts the retained local policy writer.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_policy_writer(
    review: &crate::policy_trust::PolicyReview,
    action: crate::policy_trust::Action,
) -> io::Result<StatusRun> {
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    command.current_dir(&review.context().home.path).args([
        "apply",
        "--review-trust",
        action.argument(),
        "--decide-from",
        "-",
        "--json",
    ]);
    spawn(command, Some(review.encode().map_err(io::Error::other)?))
}

fn review_args(command: &mut Command, context: &crate::protocol::StatusContext) {
    command
        .current_dir(&context.home.path)
        .args(["sync", "--dry-run", "--json"]);
}

/// Starts an exact reviewed sync with hooks deferred for whole syncs.
///
/// Cancellation after the CLI has read its input is not a rollback.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_reviewed_sync(
    review: crate::decisions::CompleteReview<'_>,
    message: Option<&CommitMessage>,
    selected: Option<&std::collections::BTreeMap<String, Vec<u32>>>,
    mass_delete: bool,
) -> io::Result<StatusRun> {
    let home = review.home();
    let bytes = review
        .encode_mode(selected, mass_delete)
        .map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    reviewed_args(&mut command, home, message, selected.is_some());
    spawn(command, Some(bytes))
}

fn reviewed_args(
    command: &mut Command,
    home: &Path,
    message: Option<&CommitMessage>,
    selected: bool,
) {
    command
        .current_dir(home)
        .args(["sync", "--json", "--decide-from", "-"])
        .arg(if selected {
            "--no-hooks"
        } else {
            "--defer-hooks"
        });
    if let Some(message) = message {
        command.arg(format!("--message={}", message.as_str()));
    }
}

/// Starts an exact reviewed apply with hooks disabled.
///
/// Cancellation after the CLI has read its input is not a rollback.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_reviewed_apply(review: &crate::decisions::ReviewedApply<'_>) -> io::Result<StatusRun> {
    let bytes = review.encode().map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    reviewed_apply_args(&mut command, review);
    spawn(command, Some(bytes))
}

fn reviewed_apply_args(command: &mut Command, review: &crate::decisions::ReviewedApply<'_>) {
    command
        .current_dir(review.home())
        .args(["apply", "--json", "--decide-from", "-", "--no-hooks", "--"])
        .args(review.operands());
}

/// Starts a reviewed identity approval with hooks disabled.
///
/// Cancellation after input delivery is not a rollback.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_reviewed_identity(
    review: &crate::decisions::ReviewedIdentity<'_>,
) -> io::Result<StatusRun> {
    let bytes = review.encode().map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    reviewed_identity_args(&mut command, review);
    spawn(command, Some(bytes))
}

/// Starts the fixed pending-hook approval using its retained status envelope.
///
/// # Errors
/// Returns an error if encoding or process creation fails.
pub fn start_reviewed_hooks(review: &crate::hook_trust::HookReview<'_>) -> io::Result<StatusRun> {
    let bytes = review.encode().map_err(io::Error::other)?;
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    command.current_dir(&review.context().home.path).args([
        "apply",
        "--review-hooks",
        "--decide-from",
        "-",
        "--json",
    ]);
    spawn(command, Some(bytes))
}

fn reviewed_identity_args(command: &mut Command, review: &crate::decisions::ReviewedIdentity<'_>) {
    command.current_dir(&review.context().home.path).args([
        "apply",
        "--json",
        "--decide-from",
        "-",
        "--no-hooks",
    ]);
}

/// Starts one repository-bound exact comparison.
///
/// # Errors
/// Returns an error if the process or its pipes cannot be created.
pub fn start_diff(
    selection: &crate::protocol::DiffSelection,
    approved: bool,
) -> io::Result<StatusRun> {
    let mut command = Command::new(companion_path(&std::env::current_exe()?)?);
    diff_args(&mut command, selection, approved)?;
    start(command)
}

fn diff_args(
    command: &mut Command,
    selection: &crate::protocol::DiffSelection,
    approved: bool,
) -> io::Result<()> {
    let entry = &selection.entry;
    let repo_id = selection
        .context
        .repo_id
        .as_ref()
        .ok_or_else(|| io::Error::other("missing repository identity"))?;
    let path = match entry.scope.as_str() {
        "home" => selection.context.home.path.join(&entry.key.raw),
        "system" => PathBuf::from(&entry.key.raw),
        _ => return Err(io::Error::other("unsupported scope")),
    };
    command
        .current_dir(&selection.context.home.path)
        .args(["diff", "--json"])
        .arg(format!("--exact-repo={}", repo_id.raw));
    if entry.encrypted {
        if !approved {
            return Err(io::Error::other("encrypted diff not approved"));
        }
        let source = entry
            .source
            .as_ref()
            .ok_or_else(|| io::Error::other("missing encrypted source"))?;
        command.arg(format!("--allow-encrypted-source={}", source.raw));
    }
    command.arg("--").arg(path);
    Ok(())
}

fn companion_path(executable: &Path) -> io::Result<PathBuf> {
    let path = executable.with_file_name("dotman");
    if !path.is_absolute() {
        return Err(io::Error::other("GUI executable path is not absolute"));
    }
    if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(io::Error::other("CLI companion is not a regular file"));
    }
    Ok(path)
}

fn start(command: Command) -> io::Result<StatusRun> {
    spawn(command, None)
}

fn spawn(command: Command, input: Option<Vec<u8>>) -> io::Result<StatusRun> {
    spawn_pinned(command, input, None)
}

fn spawn_pinned(
    mut command: Command,
    input: Option<Vec<u8>>,
    parent: Option<Arc<std::fs::File>>,
) -> io::Result<StatusRun> {
    let has_input = input.is_some();
    command
        .env_remove("DOTMAN_AGE_KEY")
        .env("DOTMAN_NON_INTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .stdin(if has_input {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::sys::new_session(&mut command);
    let mut child = command.spawn()?;
    let result = (|| {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        crate::sys::nonblocking(&stdout)?;
        crate::sys::nonblocking(&stderr)?;
        let input = match input {
            Some(bytes) => {
                let pipe = child
                    .stdin
                    .take()
                    .ok_or_else(|| io::Error::other("missing stdin"))?;
                crate::sys::nonblocking(&pipe)?;
                Some(PendingInput {
                    pipe,
                    bytes,
                    offset: 0,
                })
            }
            None => None,
        };
        Ok((stdout, stderr, input))
    })();
    match result {
        Ok((stdout, stderr, input)) => Ok(StatusRun {
            child: Some(child),
            stdout,
            stderr,
            out: Vec::new(),
            err: Vec::new(),
            out_eof: false,
            err_eof: false,
            cancelled: false,
            failure: None,
            input,
            input_delivery: if has_input {
                InputDelivery::Incomplete
            } else {
                InputDelivery::Complete
            },
            setup_parent: parent,
            stdout_limit: STDOUT_LIMIT,
            stderr_limit: STDERR_LIMIT,
            exit_observed: None,
        }),
        Err(error) => {
            reap_later(child, false, parent);
            Err(error)
        }
    }
}

fn reap_later(mut child: Child, signalled: bool, parent: Option<Arc<std::fs::File>>) {
    if !signalled {
        let _ = crate::sys::terminate_group(&child);
    }
    std::thread::spawn(move || {
        let _ = child.wait();
        drop(parent);
    });
}

fn drain(
    pipe: &mut impl Read,
    bytes: &mut Vec<u8>,
    limit: usize,
    eof: &mut bool,
) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    while !*eof {
        match pipe.read(&mut buffer) {
            Ok(0) => *eof = true,
            Ok(n) => {
                if n > limit.saturating_sub(bytes.len()) {
                    return Err(io::Error::other("child output limit exceeded"));
                }
                let chunk = buffer
                    .get(..n)
                    .ok_or_else(|| io::Error::other("invalid read length"))?;
                bytes.extend_from_slice(chunk);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn discard(pipe: &mut impl Read, eof: &mut bool) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    for _ in 0..8 {
        if *eof {
            break;
        }
        match pipe.read(&mut buffer) {
            Ok(0) => *eof = true,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl StatusRun {
    /// Borrow accumulated stdout after polling; a final receipt takes these bytes.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.out
    }

    /// Whether this owner has reaped the direct child, not whether descendants exited.
    ///
    /// Cancellation and poll errors alone do not establish this. Keep polling within
    /// a caller-controlled deadline and do not auto-refresh while cleanup is unresolved.
    /// Dropping an unreaped run hands it to a background reaper without confirmation.
    #[must_use]
    pub const fn is_reaped(&self) -> bool {
        self.child.is_none()
    }

    /// Waits up to 20 ms for child output, input readiness or pipe closure.
    ///
    /// # Errors
    /// Returns an error if descriptor readiness cannot be checked.
    pub fn wait_readable(&self) -> io::Result<()> {
        crate::sys::wait_readable(
            (!self.out_eof).then(|| self.stdout.as_fd()),
            (!self.err_eof).then(|| self.stderr.as_fd()),
            self.input.as_ref().map(|input| input.pipe.as_fd()),
        )?;
        Ok(())
    }

    /// Sends SIGTERM to the verified owned process group, while the child remains unreaped.
    ///
    /// # Errors
    /// Returns an error if the group cannot be verified or signalled.
    pub fn cancel(&mut self) -> io::Result<()> {
        if self.cancelled {
            return Ok(());
        }
        self.input = None;
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| io::Error::other("child already reaped"))?;
        if let Err(error) = crate::sys::terminate_group(child) {
            if self.failure.is_none() {
                self.failure = Some(io::Error::new(error.kind(), error.to_string()));
            }
            return Err(error);
        }
        self.cancelled = true;
        Ok(())
    }

    /// Drains both pipes without blocking and returns a receipt only after complete output and exit.
    ///
    /// # Errors
    /// Returns an error for cancellation, incomplete pipes, excess output or I/O failure.
    pub fn poll(&mut self) -> io::Result<Option<Receipt>> {
        if self.child.is_none() {
            return Err(io::Error::other("child already reaped"));
        }
        if self.failure.is_none() {
            let result = drain(
                &mut self.stdout,
                &mut self.out,
                self.stdout_limit,
                &mut self.out_eof,
            )
            .and_then(|()| {
                drain(
                    &mut self.stderr,
                    &mut self.err,
                    self.stderr_limit,
                    &mut self.err_eof,
                )
            });
            if let Err(error) = result {
                self.failure = Some(error);
                self.cancel()?;
            }
        }
        if self.failure.is_none()
            && let Err(error) = self.write_input()
        {
            self.failure = Some(error);
            self.cancel()?;
        }
        if self.failure.is_some() {
            discard(&mut self.stdout, &mut self.out_eof)?;
            discard(&mut self.stderr, &mut self.err_eof)?;
        }
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| io::Error::other("child already reaped"))?;
        if !crate::sys::exited(child)? {
            return Ok(None);
        }
        if self.failure.is_none() {
            let result = drain(
                &mut self.stdout,
                &mut self.out,
                self.stdout_limit,
                &mut self.out_eof,
            )
            .and_then(|()| {
                drain(
                    &mut self.stderr,
                    &mut self.err,
                    self.stderr_limit,
                    &mut self.err_eof,
                )
            });
            if let Err(error) = result {
                self.failure = Some(error);
            }
        }
        let pipes_open = !self.out_eof || !self.err_eof;
        // An unrelated concurrent spawn can temporarily hold a close-on-exec pipe.
        // Keep the leader unreaped while waiting for EOF, so group signalling stays safe.
        if pipes_open && self.failure.is_none() && !self.cancelled {
            let observed = self.exit_observed.get_or_insert_with(Instant::now);
            if observed.elapsed() < PIPE_CLOSE_GRACE {
                return Ok(None);
            }
        }
        let signal_result = if pipes_open && !self.cancelled {
            self.cancel()
        } else {
            Ok(())
        };
        let status = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("child already reaped"))?
            .wait()?;
        self.child = None;
        signal_result?;
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if pipes_open {
            return Err(io::Error::other(
                "child output pipes remain open; descendants may ignore SIGTERM",
            ));
        }
        if self.cancelled {
            return Err(io::Error::other("child cancelled"));
        }
        if self.input_delivery == InputDelivery::Incomplete && status.success() {
            return Err(io::Error::other("child exited before complete input"));
        }
        Ok(Some(Receipt {
            stdout: std::mem::take(&mut self.out),
            stderr: std::mem::take(&mut self.err),
            status,
        }))
    }

    fn write_input(&mut self) -> io::Result<()> {
        let Some(input) = &mut self.input else {
            return Ok(());
        };
        let remaining = input
            .bytes
            .get(input.offset..)
            .ok_or_else(|| io::Error::other("invalid input offset"))?;
        if remaining.is_empty() {
            self.input = None;
            self.input_delivery = InputDelivery::Complete;
            return Ok(());
        }
        match input.pipe.write(remaining) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "cannot write decisions",
            )),
            Ok(written) => {
                input.offset = input
                    .offset
                    .checked_add(written)
                    .ok_or_else(|| io::Error::other("invalid input length"))?;
                if input.offset == input.bytes.len() {
                    self.input = None;
                    self.input_delivery = InputDelivery::Complete;
                }
                Ok(())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.input = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for StatusRun {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            reap_later(child, self.cancelled, self.setup_parent.take());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove fixture directory");
        }
    }

    #[test]
    fn setup_has_fixed_arguments_and_home() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = crate::setup::tests::Fixture::new()?;
        let setup = crate::setup::SetupIntent::from_status(&fixture.status()?)?;
        let mut command = Command::new("/bin/false");
        setup_args(&mut command, &setup);
        assert_eq!(command.get_current_dir(), Some(fixture.0.as_path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["init", "--setup-from", "-", "--json"]
        );
        let setup = setup.for_clone(crate::setup::CloneSource::parse(
            "file:///fixture/source".into(),
        )?);
        let mut command = Command::new("/bin/false");
        setup_args(&mut command, &setup);
        assert_eq!(command.get_current_dir(), Some(fixture.0.as_path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["init", "--setup-from", "-", "--json"]
        );
        let pinned = setup.parent();
        let mut run = spawn_pinned(
            Command::new("/bin/cat"),
            Some(setup.encode()?),
            Some(setup.parent()),
        )?;
        drop(setup);
        assert!(
            run.setup_parent
                .as_ref()
                .is_some_and(|parent| Arc::ptr_eq(&pinned, parent))
        );
        let receipt = finish(&mut run)?;
        assert!(receipt.status.success());
        assert!(
            run.setup_parent
                .as_ref()
                .is_some_and(|parent| Arc::ptr_eq(&pinned, parent))
        );
        Ok(())
    }

    #[test]
    fn companion_is_fixed_regular_sibling() {
        let dir = std::env::temp_dir().join(format!("dotman-gui-companion-{}", std::process::id()));
        std::fs::create_dir(&dir).expect("create fixture directory");
        let dir = TestDir(dir);
        let gui = dir.0.join("gui");
        let cli = dir.0.join("dotman");
        std::fs::write(&gui, b"fixture").expect("create GUI fixture");
        assert!(companion_path(&gui).is_err());
        std::fs::write(&cli, b"fixture").expect("create companion");
        assert_eq!(companion_path(&gui).expect("companion"), cli);
        std::fs::remove_file(&cli).expect("remove companion");
        symlink(&gui, &cli).expect("create symlink");
        assert!(companion_path(&gui).is_err());
    }

    fn fixture(script: &str) -> StatusRun {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .env_clear()
            .env("HOME", "/nonexistent")
            .env("XDG_CONFIG_HOME", "/nonexistent/config")
            .env("DOTMAN_AGE_KEY", "fixture-key")
            .env("DOTMAN_HOST", "fixture-host");
        start(command).expect("spawn fixture")
    }

    fn input_fixture(script: &str, bytes: Vec<u8>) -> StatusRun {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .env_clear()
            .env("HOME", "/nonexistent")
            .env("XDG_CONFIG_HOME", "/nonexistent/config");
        spawn(command, Some(bytes)).expect("spawn fixture")
    }

    fn finish(run: &mut StatusRun) -> io::Result<Receipt> {
        let start = Instant::now();
        loop {
            if let Some(receipt) = run.poll()? {
                return Ok(receipt);
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "fixture timed out"
            );
            run.wait_readable()?;
        }
    }

    #[test]
    fn environment_stdin_session_and_exit() {
        let mut run = fixture(
            "test -z \"$DOTMAN_AGE_KEY\" && test \"$DOTMAN_HOST\" = fixture-host && test \"$DOTMAN_NON_INTERACTIVE\" = 1 && test \"$NO_COLOR\" = 1 && ! read x && test \"$(/bin/ps -o sid= -p $$ | /usr/bin/tr -d ' ')\" = \"$$\" && printf ok && printf err >&2; exit 7",
        );
        let result = finish(&mut run).expect("receipt");
        assert!(run.is_reaped());
        assert_eq!(result.stdout, b"ok");
        assert_eq!(result.stderr, b"err");
        assert_eq!(result.status.code(), Some(7));
    }

    #[test]
    fn drains_both_full_pipes() {
        let mut run = fixture(
            "/usr/bin/head -c 200000 /dev/zero & /usr/bin/head -c 200000 /dev/zero >&2 & wait",
        );
        let result = finish(&mut run).expect("receipt");
        assert!(result.status.success());
        assert_eq!(result.stdout.len(), 200_000);
        assert_eq!(result.stderr.len(), 200_000);
    }

    #[test]
    fn sends_full_input_then_closes_stdin() {
        let input = vec![b'Z'; 200_000];
        let mut run = input_fixture("/bin/cat; printf ':eof'", input.clone());
        let receipt = finish(&mut run).expect("receipt");
        assert!(receipt.status.success());
        let received = receipt.stdout.strip_suffix(b":eof").expect("EOF marker");
        assert_eq!(received, input);
        assert!(run.input_delivery == InputDelivery::Complete);
    }

    #[test]
    fn sends_input_while_draining_both_outputs() {
        let input = vec![b'X'; 200_000];
        let mut run = input_fixture(
            "/usr/bin/head -c 200000 /dev/zero; /usr/bin/head -c 200000 /dev/zero >&2; /bin/cat",
            input.clone(),
        );
        let receipt = finish(&mut run).expect("receipt");
        assert!(receipt.status.success());
        assert_eq!(receipt.stderr.len(), 200_000);
        assert_eq!(
            receipt
                .stdout
                .strip_suffix(input.as_slice())
                .map(<[u8]>::len),
            Some(200_000)
        );
    }

    #[test]
    fn incomplete_input_preserves_nonzero_exit_but_rejects_success() {
        let mut refused = input_fixture(
            "exec 0<&-; printf refused >&2; exit 7",
            vec![b'x'; 1_000_000],
        );
        let receipt = finish(&mut refused).expect("nonzero receipt");
        assert_eq!(receipt.status.code(), Some(7));
        assert_eq!(receipt.stderr, b"refused");
        assert!(refused.input_delivery == InputDelivery::Incomplete);
        let mut premature = input_fixture("exec 0<&-; exit 0", vec![b'x'; 1_000_000]);
        assert_eq!(
            finish(&mut premature)
                .err()
                .expect("incomplete input")
                .to_string(),
            "child exited before complete input"
        );
        assert!(premature.input_delivery == InputDelivery::Incomplete);
    }

    #[test]
    fn exited_child_waits_for_pipe_closure_and_keeps_late_output() {
        // A noninteractive shell may replace a background job's stdin with /dev/null.
        let mut run = input_fixture(
            "exec 3<&0; (IFS= read -r x <&3 && printf late) & exit 7",
            Vec::new(),
        );
        let mut control = run.input.take().expect("control pipe").pipe;
        let start = Instant::now();
        while !crate::sys::exited(run.child.as_ref().expect("child")).expect("exit observation") {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        assert!(run.poll().expect("poll").is_none());
        assert!(!run.is_reaped());
        control.write_all(b"go\n").expect("release writer");
        drop(control);
        let receipt = finish(&mut run).expect("complete receipt");
        assert_eq!(receipt.status.code(), Some(7));
        assert_eq!(receipt.stdout, b"late");
    }

    #[test]
    fn cancelling_blocked_input_terminates_owned_group() {
        let mut run = input_fixture("exec /bin/sleep 30", vec![b'x'; 1_000_000]);
        let start = Instant::now();
        while run.input.as_ref().is_some_and(|input| input.offset == 0) {
            assert!(run.poll().expect("poll").is_none());
            assert!(start.elapsed() < Duration::from_secs(5));
            run.wait_readable().expect("wait");
        }
        assert!(run.input.is_some());
        assert!(!run.is_reaped());
        run.cancel().expect("cancel");
        assert!(!run.is_reaped());
        assert!(run.input.is_none());
        assert!(finish(&mut run).is_err());
        assert!(run.is_reaped());
    }

    #[test]
    fn rejects_overflow() {
        let mut run = fixture("/usr/bin/head -c 1100000 /dev/zero >&2; /bin/sleep 1");
        assert!(finish(&mut run).is_err());
    }

    #[test]
    fn output_failure_can_precede_direct_child_reap() {
        let mut run =
            fixture("trap '' TERM; /usr/bin/head -c 1100000 /dev/zero >&2; exec /bin/sleep 1");
        let start = Instant::now();
        while run.failure.is_none() {
            assert!(run.poll().expect("poll while active").is_none());
            assert!(start.elapsed() < Duration::from_secs(5));
            run.wait_readable().expect("wait");
        }
        assert!(!run.is_reaped());
        assert!(finish(&mut run).is_err());
        assert!(run.is_reaped());
    }

    #[test]
    fn inherited_pipe_does_not_block_after_exit() {
        let mut run = fixture(
            "trap 'exit 0' USR1; /bin/sh -c 'trap \"printf child-term; exit 0\" TERM; printf ready; kill -USR1 \"$1\"; for x in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do /bin/sleep 0.1; done' sh \"$$\" & wait",
        );
        assert_eq!(
            finish(&mut run).err().expect("inherited pipe").to_string(),
            "child output pipes remain open; descendants may ignore SIGTERM"
        );
        assert!(run.is_reaped());
        let start = Instant::now();
        while !run.out_eof {
            drain(
                &mut run.stdout,
                &mut run.out,
                STDOUT_LIMIT,
                &mut run.out_eof,
            )
            .expect("drain child");
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(run.out.windows(5).any(|chunk| chunk == b"ready"));
        assert!(run.out.windows(10).any(|chunk| chunk == b"child-term"));
    }

    #[test]
    fn closed_pipes_do_not_mean_child_exit() {
        let mut run = fixture("exec 1>&- 2>&-; /bin/sleep 30");
        std::thread::sleep(Duration::from_millis(50));
        assert!(run.poll().expect("poll").is_none());
        run.cancel().expect("cancel");
        assert!(finish(&mut run).is_err());
    }

    #[test]
    fn cancel_owned_group() {
        let mut run = fixture(
            "/bin/sh -c 'trap \"printf child-term; exit\" TERM; printf ready; while :; do /bin/sleep 0.05; done' & wait",
        );
        let start = Instant::now();
        while !run.out.windows(5).any(|chunk| chunk == b"ready") {
            assert!(run.poll().expect("poll").is_none());
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        run.cancel().expect("cancel");
        assert!(finish(&mut run).is_err());
        std::thread::sleep(Duration::from_millis(100));
        drain(
            &mut run.stdout,
            &mut run.out,
            STDOUT_LIMIT,
            &mut run.out_eof,
        )
        .expect("drain child");
        assert!(run.out.windows(10).any(|chunk| chunk == b"child-term"));
    }

    #[test]
    fn exact_arguments_are_literal_and_encrypted_source_requires_consent()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/home","shown":"/home"},"repo":{"raw":"/repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/config","shown":"/repo/config"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"id","shown":"id"},"machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
        let entry = r#"{"record":"entry","status":"changed-locally","key":{"raw":"--approve-hooks","shown":"--approve-hooks"},"scope":"home","path":null,"source":{"raw":"--approve-hooks{age}","shown":"source"},"recorded_source":null,"encrypted":true,"template":false,"system":false,"row":"changed-locally","row_number":9,"category":null,"r":{"state":"missing"},"source_tuple":{"state":"missing"},"h":{"state":"missing"},"raw_h":{"state":"missing"},"s":{"state":"missing"},"identity_changed":false,"update_state":false}"#;
        let review = r#"{"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}}"#;
        let summary = r#"{"record":"summary","rows":1,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
        let result = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
        let input = format!("{context}\n{entry}\n{review}\n{summary}\n{result}\n");
        let crate::protocol::StatusReply::Report(report) =
            crate::protocol::parse_status(input.as_bytes(), Some(0))?
        else {
            return Err("expected report".into());
        };
        let selection = crate::protocol::DiffSelection {
            context: report.context,
            entry: report.entries.into_iter().next().expect("entry"),
        };
        let mut review = Command::new("/bin/false");
        review_args(&mut review, &selection.context);
        assert_eq!(review.get_current_dir(), Some(Path::new("/home")));
        assert_eq!(
            review.get_args().collect::<Vec<_>>(),
            ["sync", "--dry-run", "--json"]
        );
        let mut writer = Command::new("/bin/false");
        reviewed_args(&mut writer, &selection.context.home.path, None, false);
        assert_eq!(writer.get_current_dir(), Some(Path::new("/home")));
        assert_eq!(
            writer.get_args().collect::<Vec<_>>(),
            ["sync", "--json", "--decide-from", "-", "--defer-hooks"]
        );
        let mut selected = Command::new("/bin/false");
        reviewed_args(&mut selected, &selection.context.home.path, None, true);
        assert_eq!(
            selected.get_args().collect::<Vec<_>>(),
            ["sync", "--json", "--decide-from", "-", "--no-hooks"]
        );
        let message = CommitMessage::new("--approve-hooks <é>'\"\n--no-hooks".into())?;
        let mut writer = Command::new("/bin/false");
        reviewed_args(
            &mut writer,
            &selection.context.home.path,
            Some(&message),
            false,
        );
        assert_eq!(
            writer.get_args().collect::<Vec<_>>(),
            [
                "sync",
                "--json",
                "--decide-from",
                "-",
                "--defer-hooks",
                "--message=--approve-hooks <é>'\"\n--no-hooks"
            ]
        );
        assert!(diff_args(&mut Command::new("/bin/false"), &selection, false).is_err());
        let mut command = Command::new("/bin/false");
        diff_args(&mut command, &selection, true).expect("arguments");
        assert_eq!(command.get_current_dir(), Some(Path::new("/home")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "diff",
                "--json",
                "--exact-repo=id",
                "--allow-encrypted-source=--approve-hooks{age}",
                "--",
                "/home/--approve-hooks"
            ]
        );
        Ok(())
    }

    #[test]
    fn reviewed_apply_arguments_use_only_literal_reviewed_operands()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/home space","shown":"/home space"},"repo":{"raw":"/repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/config","shown":"/repo/config"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"id","shown":"id"},"machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
        let review = r#"{"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}}"#;
        let summary = r#"{"record":"summary","rows":0,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
        let result = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
        let input = format!("{context}\n{review}\n{summary}\n{result}\n");
        let crate::protocol::StatusReply::Report(report) =
            crate::protocol::parse_status(input.as_bytes(), Some(0))?
        else {
            return Err("expected report".into());
        };
        let entry: crate::protocol::StatusEntry = serde_json::from_str(
            r#"{"status":"both-changed","key":{"raw":"--option é","shown":"alias"},"scope":"home","path":null,"source":{"raw":"--option é","shown":"alias"},"recorded_source":null,"encrypted":false,"template":false,"system":false,"row":"both-changed","row_number":11,"category":"conflicts","r":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"source_tuple":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"h":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"raw_h":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"s":{"state":"missing"},"identity_changed":false,"update_state":false}"#,
        )?;
        let reviewed = crate::decisions::ReviewedApply::new(
            &report.context,
            vec![(&entry, crate::decisions::ApplyAction::TakeRepo)],
        )?;
        let mut command = Command::new("/bin/false");
        reviewed_apply_args(&mut command, &reviewed);
        assert_eq!(command.get_current_dir(), Some(Path::new("/home space")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "apply",
                "--json",
                "--decide-from",
                "-",
                "--no-hooks",
                "--",
                "/home space/--option é"
            ]
        );
        Ok(())
    }

    #[test]
    fn reviewed_identity_arguments_have_no_operands() -> Result<(), Box<dyn std::error::Error>> {
        let context = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/home space","shown":"/home space"},"repo":{"raw":"/repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/config","shown":"/repo/config"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"id","shown":"id"},"machine":{"host":{"raw":"beta","shown":"beta"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
        let entry = r#"{"record":"entry","status":"identity-changed","key":{"raw":"profile","shown":"profile"},"scope":"home","path":null,"source":{"raw":"profile","shown":"profile"},"recorded_source":{"raw":"profile{host=alpha}","shown":"profile{host=alpha}"},"encrypted":false,"template":false,"system":false,"row":"changed-in-repo","row_number":10,"category":null,"r":{"state":"missing"},"source_tuple":{"state":"missing"},"h":{"state":"missing"},"raw_h":{"state":"missing"},"s":{"state":"missing"},"identity_changed":true,"update_state":false}"#;
        let context_value: serde_json::Value = serde_json::from_str(context)?;
        let current = context_value.get("machine").ok_or("machine")?.clone();
        let mut recorded = current.clone();
        *recorded.pointer_mut("/host/raw").ok_or("host")? = "alpha".into();
        let marker = serde_json::json!({"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null},"current_identity":current,"recorded_identity":recorded});
        let summary = r#"{"record":"summary","rows":1,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
        let result = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
        let input = format!("{context}\n{entry}\n{marker}\n{summary}\n{result}\n");
        let crate::protocol::StatusReply::Report(report) =
            crate::protocol::parse_status(input.as_bytes(), Some(0))?
        else {
            return Err("expected report".into());
        };
        let review = crate::decisions::ReviewedIdentity::new(&report)?;
        let mut command = Command::new("/bin/false");
        reviewed_identity_args(&mut command, &review);
        assert_eq!(command.get_current_dir(), Some(Path::new("/home space")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["apply", "--json", "--decide-from", "-", "--no-hooks"]
        );
        Ok(())
    }

    #[test]
    fn commit_message_bounds_and_unsafe_characters() {
        for value in [
            "",
            " \n ",
            "a\0b",
            "a\tb",
            "a\r",
            "a\u{202e}",
            "a\u{200b}",
            "a\nb\nc\nd\ne\nf\ng",
        ] {
            assert!(CommitMessage::new(value.into()).is_err());
        }
        assert!(CommitMessage::new("x".repeat(1024)).is_ok());
        assert!(CommitMessage::new("x".repeat(1025)).is_err());
        assert!(CommitMessage::new("é".repeat(513)).is_err());
        assert!(CommitMessage::new("a\nb\nc\nd\ne\nf".into()).is_ok());
    }
}
