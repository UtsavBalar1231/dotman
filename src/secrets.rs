//! `{age}` sources, through the `age` or `rage` CLI. Plaintext only travels through pipes: it goes
//! to the CLI on stdin and comes back on stdout, so the repo only ever holds ciphertext.

use crate::config::Config;
use crate::context::Context;
use crate::engine::observe;
use crate::engine::write::{WriteOptions, WriteOutcome, safe_write};
use crate::name;
use crate::output;
use crate::paths::{Paths, write_atomic};
use crate::proc::{self, AGE_KEY_VAR};
use crate::state::Kind;
use crate::sys;
use crate::variant::Flag;
use anyhow::{Context as _, Result, anyhow, bail};
use std::cell::{Cell, OnceCell};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SECRET_KEY_MARKER: &[u8] = b"AGE-SECRET-KEY-";
const ARMOR: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";
// age reads identities from `-i -` since 1.1.0.
const STDIN_IDENTITIES: (u32, u32, u32) = (1, 1, 0);
// The recipient set the `{age}` sources were last all encrypted to. `.dotman/` is reserved, so
// this is never a managed key.
const RECORD: &str = ".dotman/age-recipients";
const INSTALL: &str =
    "pacman -S age, apt install age, dnf install age, apk add age or brew install age";
/// Where a key can come from, for messages about a missing one.
#[must_use]
pub fn key_hint() -> String {
    format!(
        "set DOTMAN_AGE_KEY or DOTMAN_AGE_KEY_FILE, or run `{} key import`",
        name::bin()
    )
}

pub(crate) fn no_recipients() -> String {
    format!(
        "[age] recipients in .dotman.toml is empty; `{} key init` creates a key and adds its recipient",
        name::bin()
    )
}

/// Whether `bytes` contain an age identity.
#[must_use]
pub fn holds_secret_key(bytes: &[u8]) -> bool {
    bytes
        .windows(SECRET_KEY_MARKER.len())
        .any(|window| window == SECRET_KEY_MARKER)
}

/// No key here decrypts a source. Report-only: the path is left alone.
#[derive(Debug)]
pub struct Locked(pub String);

impl fmt::Display for Locked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "locked: {}", self.0)
    }
}

impl std::error::Error for Locked {}

/// A plain capture would store an age identity in the repo.
#[derive(Debug)]
pub struct HoldsKey;

impl fmt::Display for HoldsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "holds an age secret key (AGE-SECRET-KEY-), which dotman never stores in plain text; `{} add --encrypt` stores it encrypted",
            name::bin()
        )
    }
}

impl std::error::Error for HoldsKey {}

#[derive(Debug, Clone)]
struct Cli {
    program: PathBuf,
    rage: bool,
}

#[derive(Debug)]
struct Reader {
    program: PathBuf,
    stdin_key: bool,
    files: Vec<PathBuf>,
    locked: Option<String>,
    /// Printed once, by the first decryption.
    notes: Vec<String>,
    /// With `DOTMAN_AGE_KEY` set: the version of the age that reads it, or why none can.
    stdin_version: Option<Result<String, String>>,
}

/// The age CLI and the identities this machine has, resolved once per run.
#[derive(Debug)]
pub struct Age {
    command: Option<String>,
    identity: PathBuf,
    key: Option<String>,
    key_file: Option<PathBuf>,
    cli: OnceCell<Result<Cli, String>>,
    reader: OnceCell<Reader>,
    noted: Cell<bool>,
}

impl Age {
    /// `command` is `[age] command`; `key` is the text of `DOTMAN_AGE_KEY`; `key_file` is
    /// `DOTMAN_AGE_KEY_FILE`. Nothing is run until a method needs the CLI.
    #[must_use]
    pub const fn new(
        command: Option<String>,
        identity: PathBuf,
        key: Option<String>,
        key_file: Option<PathBuf>,
    ) -> Self {
        Self {
            command,
            identity,
            key,
            key_file,
            cli: OnceCell::new(),
            reader: OnceCell::new(),
            noted: Cell::new(false),
        }
    }

    /// From the machine config, the resolved paths and the process environment.
    #[must_use]
    pub fn from_env(machine: &Config, paths: &Paths) -> Self {
        let key = std::env::var(AGE_KEY_VAR)
            .ok()
            .filter(|key| !key.trim().is_empty());
        Self::new(
            machine.age.command.clone(),
            paths.age_identity.clone(),
            key,
            paths.age_key_file.clone(),
        )
    }

    /// Whether an identity is available here: `DOTMAN_AGE_KEY`, an existing `DOTMAN_AGE_KEY_FILE`
    /// or the identity file.
    #[must_use]
    pub fn has_key_source(&self) -> bool {
        let exists = |path: &Path| fs::symlink_metadata(path).is_ok();
        self.key.is_some() || self.key_file.as_deref().is_some_and(exists) || exists(&self.identity)
    }

    fn cli(&self) -> Result<&Cli> {
        self.cli
            .get_or_init(|| select(self.command.as_deref()))
            .as_ref()
            .map_err(|e| anyhow!("{e}"))
    }

    /// Fails with the install hints when no age CLI can be used.
    pub fn require_cli(&self) -> Result<()> {
        self.cli().map(drop)
    }

    /// The keygen tool that comes with the selected CLI, beside it: `age-keygen` or `rage-keygen`.
    pub fn keygen(&self) -> Result<PathBuf> {
        let program = &self.cli()?.program;
        let name = program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let keygen = program.with_file_name(format!("{name}-keygen"));
        proc::find_program(&keygen.to_string_lossy()).ok_or_else(|| {
            anyhow!(
                "{} is missing; it is installed with {name}",
                keygen.display()
            )
        })
    }

    fn reader(&self) -> &Reader {
        self.reader.get_or_init(|| self.plan_reader())
    }

    fn plan_reader(&self) -> Reader {
        let cli = match self.cli() {
            Ok(cli) => cli.clone(),
            Err(e) => {
                return Reader {
                    program: PathBuf::new(),
                    stdin_key: false,
                    files: Vec::new(),
                    locked: Some(format!("{e:#}")),
                    notes: Vec::new(),
                    stdin_version: self.key.as_ref().map(|_| Err(format!("{e:#}"))),
                };
            }
        };
        let mut notes = Vec::new();
        let mut program = cli.program.clone();
        let key_reader = self.key.as_ref().map(|_| stdin_reader(&cli));
        let stdin_key = match &key_reader {
            Some(Ok((age, _))) => {
                program.clone_from(age);
                true
            }
            Some(Err(why)) => {
                notes.push(format!(
                    "{AGE_KEY_VAR} is set but cannot be used: {why}; install age 1.1.0 or newer, or use DOTMAN_AGE_KEY_FILE or `{} key import`",
                    name::bin()
                ));
                false
            }
            None => false,
        };
        let mut files = Vec::new();
        if let Some(file) = &self.key_file {
            if fs::symlink_metadata(file).is_ok() {
                files.push(file.clone());
            } else {
                notes.push(format!(
                    "DOTMAN_AGE_KEY_FILE is {}, which does not exist; ignored",
                    file.display()
                ));
            }
        }
        if fs::symlink_metadata(&self.identity).is_ok() {
            files.push(self.identity.clone());
        }
        let locked = (!stdin_key && files.is_empty()).then(|| {
            if self.key.is_some() {
                format!("{AGE_KEY_VAR} cannot be used here and there is no identity file")
            } else {
                format!("no age key; {}", key_hint())
            }
        });
        Reader {
            program,
            stdin_key,
            files,
            locked,
            notes,
            stdin_version: key_reader.map(|found| found.map(|(_, version)| version)),
        }
    }

    /// `None` when `DOTMAN_AGE_KEY` is unset; else the version of the age that reads it from
    /// stdin, or why no installed CLI can.
    #[must_use]
    pub fn key_reader(&self) -> Option<Result<String, String>> {
        self.reader().stdin_version.clone()
    }

    /// The plaintext of the age file at `ciphertext`. A [`Locked`] error means no key here
    /// matches; any other error is a failure.
    pub fn decrypt(&self, ciphertext: &Path) -> Result<Vec<u8>> {
        let reader = self.reader();
        if !self.noted.replace(true) {
            for note in &reader.notes {
                output::warning(note);
            }
        }
        if let Some(reason) = &reader.locked {
            return Err(Locked(reason.clone()).into());
        }
        let mut cmd = proc::command(&reader.program);
        cmd.arg("-d");
        if reader.stdin_key {
            cmd.args(["-i", "-"]);
        }
        for file in &reader.files {
            cmd.arg("-i").arg(file);
        }
        cmd.arg("--").arg(ciphertext);
        let key = self
            .key
            .as_ref()
            .filter(|_| reader.stdin_key)
            .map(|key| format!("{key}\n"));
        let out = run(cmd, key.as_deref().map(str::as_bytes))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        // age prints "no identity matched any of the recipients". rage's "no matching keys" is
        // taken from its documentation and was not seen, because rage was not installed.
        if err.contains("no identity matched") || err.contains("no matching keys") {
            return Err(Locked("no key here matches its recipients".to_owned()).into());
        }
        Err(failure(&reader.program, "-d", &out))
    }

    /// Armored ciphertext of `plaintext` for every recipient.
    pub fn encrypt(&self, plaintext: &[u8], recipients: &[String]) -> Result<Vec<u8>> {
        let recipients = recipient_set(recipients);
        if recipients.is_empty() {
            bail!(no_recipients());
        }
        let program = &self.cli()?.program;
        let mut cmd = proc::command(program);
        cmd.args(["-e", "-a"]);
        for recipient in &recipients {
            cmd.arg("-r").arg(recipient);
        }
        let out = run(cmd, Some(plaintext))?;
        if !out.status.success() {
            return Err(failure(program, "-e", &out));
        }
        if !out.stdout.starts_with(ARMOR) {
            bail!("{} printed no armored age file", program.display());
        }
        Ok(out.stdout)
    }

    /// Runs keygen `-o`, which refuses an existing file and creates the new one at 0600.
    pub fn generate(&self, identity: &Path) -> Result<()> {
        let keygen = self.keygen()?;
        let mut cmd = proc::command(&keygen);
        cmd.arg("-o").arg(identity);
        let out = run(cmd, None)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failure(&keygen, "-o", &out))
        }
    }

    /// The recipients of the identities in `identity` (keygen `-y`, fed on stdin). Plugin
    /// identities make age-keygen fail; rage-keygen's `-y` is unverified.
    pub fn recipients_of(&self, identity: &[u8]) -> Result<Vec<String>> {
        let keygen = self.keygen()?;
        let mut cmd = proc::command(&keygen);
        cmd.arg("-y");
        let out = run(cmd, Some(identity))?;
        if !out.status.success() {
            return Err(failure(&keygen, "-y", &out));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

fn select(command: Option<&str>) -> Result<Cli, String> {
    let program = match command {
        Some(command) => proc::find_program(command).ok_or_else(|| {
            format!("`[age] command` is `{command}`, which is not an executable on PATH")
        })?,
        None => ["rage", "age"]
            .iter()
            .find_map(|name| proc::find_program(name))
            .ok_or_else(|| {
                format!("encrypted files need rage or age, and neither is installed; install one: {INSTALL}")
            })?,
    };
    let rage = program
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("rage"));
    Ok(Cli { program, rage })
}

// `-i -` was verified with age 1.3.2 only. rage was not installed when this was written, so its
// `-i -` is unverified and an `age` on PATH reads the key instead.
fn stdin_reader(cli: &Cli) -> Result<(PathBuf, String), String> {
    let age = if cli.rage {
        proc::find_program("age").ok_or_else(|| {
            "rage is selected and its `-i -` is unverified, and no age is on PATH".to_owned()
        })?
    } else {
        cli.program.clone()
    };
    let (major, minor, patch) =
        version_of(&age).ok_or_else(|| format!("cannot read the version of {}", age.display()))?;
    let version = format!("{major}.{minor}.{patch}");
    if (major, minor, patch) < STDIN_IDENTITIES {
        return Err(format!(
            "{} is {version}, and reading the key from stdin needs 1.1.0 or newer",
            age.display()
        ));
    }
    Ok((age, version))
}

fn version_of(program: &Path) -> Option<(u32, u32, u32)> {
    let out = proc::command(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push(' ');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text.split_whitespace().find_map(parse_version)
}

/// `1.3.2`, `v1.2.1` or `0.10` as numbers; anything else is `None`.
fn parse_version(token: &str) -> Option<(u32, u32, u32)> {
    let token = token.strip_prefix('v').unwrap_or(token);
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(part) => {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()?
        }
        None => 0,
    };
    Some((major, minor, patch))
}

fn failure(program: &Path, mode: &str, out: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or_default().trim();
    anyhow!(
        "{} {mode} failed ({}): {first}",
        program.display(),
        out.status
    )
}

// The input is written from another thread while the output is read, because age streams: a big
// plaintext would otherwise fill both pipes and deadlock.
fn run(mut cmd: Command, input: Option<&[u8]>) -> Result<Output> {
    let program = PathBuf::from(cmd.get_program());
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let _sigpipe = sys::ignore_sigpipe();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run {}", program.display()))?;
    let stdin = child.stdin.take();
    let (output, written) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || match (stdin, input) {
            (Some(mut pipe), Some(bytes)) => pipe.write_all(bytes),
            _ => Ok(()),
        });
        (child.wait_with_output(), writer.join())
    });
    let output = output.with_context(|| format!("cannot run {}", program.display()))?;
    match written {
        // A child that exits early closes its stdin; its own error says why.
        Ok(Err(e)) if output.status.success() => {
            Err(e).with_context(|| format!("cannot write to {}", program.display()))
        }
        Err(_) => bail!("the thread writing to {} panicked", program.display()),
        Ok(_) => Ok(output),
    }
}

/// Trimmed, sorted, without duplicates or empty entries: the form compared with the record.
#[must_use]
pub fn recipient_set(list: &[String]) -> Vec<String> {
    let mut set: Vec<String> = list
        .iter()
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .collect();
    set.sort();
    set.dedup();
    set
}

/// The recorded recipient set; `None` when there is no record, or it is not a regular file.
pub(crate) fn read_record(repo: &Path) -> Result<Option<Vec<String>>> {
    if !fs::symlink_metadata(repo.join(".dotman")).is_ok_and(|meta| meta.is_dir()) {
        return Ok(None);
    }
    let path = repo.join(RECORD);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path);
    let mut file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ELOOP) => {
            return Ok(None);
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    Ok(Some(recipient_set(&lines)))
}

/// Writes the record, never through a symlinked `.dotman` in the worktree.
pub(crate) fn write_record(repo: &Path, recipients: &[String]) -> Result<()> {
    crate::commands::add::ensure_worktree_parents(repo, RECORD)?;
    let mut text = recipient_set(recipients).join("\n");
    text.push('\n');
    write_atomic(&repo.join(RECORD), text.as_bytes(), 0o644)
}

/// Replaces the ciphertext at `source` in the repo through the safe write path.
pub(crate) fn write_ciphertext(repo: &Path, source: &str, ciphertext: &[u8]) -> Result<()> {
    let expected = observe(repo, source)?;
    let modes = BTreeMap::new();
    let opts = WriteOptions {
        dry_run: false,
        in_place: false,
        backup: None,
        expected: &expected,
        modes: &modes,
    };
    match safe_write(repo, source, ciphertext, 0o644, &opts)? {
        WriteOutcome::Written(_) => Ok(()),
        WriteOutcome::Skipped(skip) => bail!("{source} in the repo {skip}"),
    }
}

/// Every `{age}` regular file in the worktree, whichever machine it is for.
pub(crate) fn encrypted_sources(ctx: &Context) -> Result<Vec<String>> {
    let managed = ctx.managed()?;
    let sources = ctx.sources()?;
    let mut found = Vec::new();
    for key in sources.keys() {
        found.extend(
            sources
                .sources_of(key)
                .filter(|source| {
                    sources.flag(key, source) == Flag::Age
                        && managed.get(*source) == Some(&Kind::File)
                })
                .map(str::to_owned),
        );
    }
    Ok(found)
}

/// When the record differs from `[age] recipients`, re-encrypts every `{age}` source to the
/// current set and rewrites the record. All or nothing: if one source cannot be decrypted here,
/// nothing changes. Returns how many sources were rewritten.
pub(crate) fn reencrypt(ctx: &Context, dry_run: bool) -> Result<usize> {
    let repo = &ctx.paths.repo;
    let recipients = ctx.repo_config()?.age.recipients;
    let current = recipient_set(&recipients);
    if read_record(repo)?.is_none_or(|record| record == current) {
        return Ok(0);
    }
    let sources = encrypted_sources(ctx)?;
    let count = sources.len();
    if count == 0 {
        return Ok(0);
    }
    if dry_run {
        output::info(&format!(
            "would re-encrypt {count} {} to the recipients in .dotman.toml",
            files(count)
        ));
        return Ok(0);
    }
    let age = ctx.age();
    let mut plain = Vec::new();
    let mut unreadable = Vec::new();
    for source in &sources {
        match age.decrypt(&repo.join(source)) {
            Ok(bytes) => plain.push((source, bytes)),
            Err(e) => unreadable.push(format!("{source} ({e:#})")),
        }
    }
    if !unreadable.is_empty() {
        output::warning(&format!(
            "the recipients in .dotman.toml changed, but this machine cannot decrypt {} of the {count} encrypted {}, so none was re-encrypted. The new recipients cannot read them until a machine whose key can runs `{} sync`:",
            unreadable.len(),
            files(count),
            name::bin()
        ));
        for line in unreadable {
            output::warning(&format!("  {line}"));
        }
        return Ok(0);
    }
    for (source, bytes) in &plain {
        let ciphertext = age
            .encrypt(bytes, &recipients)
            .with_context(|| format!("cannot re-encrypt {source}"))?;
        write_ciphertext(repo, source, &ciphertext)?;
        output::action("re-encrypt", source);
    }
    write_record(repo, &current)?;
    Ok(plain.len())
}

const fn files(n: usize) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_marker_is_found_anywhere_in_the_bytes() {
        assert!(holds_secret_key(b"x\nAGE-SECRET-KEY-1ABC\n"));
        assert!(holds_secret_key(b"AGE-SECRET-KEY-"));
        assert!(!holds_secret_key(b"AGE-SECRET-KEY"));
        assert!(!holds_secret_key(b"age-secret-key-1abc"));
        assert!(!holds_secret_key(b""));
    }

    #[test]
    fn versions_parse_from_age_and_rage_output() {
        assert_eq!(parse_version("1.3.2"), Some((1, 3, 2)));
        assert_eq!(parse_version("v1.1.0"), Some((1, 1, 0)));
        assert_eq!(parse_version("0.10"), Some((0, 10, 0)));
        assert_eq!(parse_version("1.2.0-rc.1"), Some((1, 2, 0)));
        for bad in ["rage", "(devel)", "1", ""] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!((1, 0, 9) < STDIN_IDENTITIES && (1, 1, 0) >= STDIN_IDENTITIES);
    }

    #[test]
    fn recipient_sets_ignore_order_space_and_duplicates() {
        let list = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            recipient_set(&list(&[" b", "a", "", "b"])),
            list(&["a", "b"])
        );
    }

    #[test]
    fn the_record_round_trips_and_a_symlink_is_no_record() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        assert_eq!(read_record(repo).unwrap(), None);
        write_record(repo, &["b".to_owned(), "a".to_owned()]).unwrap();
        assert_eq!(fs::read_to_string(repo.join(RECORD)).unwrap(), "a\nb\n");
        assert_eq!(
            read_record(repo).unwrap(),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        fs::remove_file(repo.join(RECORD)).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", repo.join(RECORD)).unwrap();
        assert_eq!(read_record(repo).unwrap(), None);
    }

    #[test]
    fn a_missing_cli_or_key_locks_without_running_anything() {
        let tmp = TempDir::new().unwrap();
        let missing = Age::new(
            Some("/nonexistent/age".to_owned()),
            tmp.path().join("id.txt"),
            None,
            None,
        );
        let err = missing.decrypt(&tmp.path().join("x")).unwrap_err();
        assert!(err.is::<Locked>(), "{err:#}");
        assert!(format!("{err:#}").contains("[age] command"), "{err:#}");
        assert!(missing.encrypt(b"x", &["age1x".to_owned()]).is_err());

        let keyless = Age::new(
            Some("/bin/sh".to_owned()),
            tmp.path().join("id.txt"),
            None,
            None,
        );
        let err = keyless.decrypt(&tmp.path().join("x")).unwrap_err();
        assert!(
            err.is::<Locked>() && format!("{err:#}").contains("no age key"),
            "{err:#}"
        );
        assert!(keyless.key_reader().is_none());
    }

    #[test]
    fn zero_recipients_name_key_init() {
        let age = Age::new(None, PathBuf::from("/nonexistent"), None, None);
        let err = format!("{:#}", age.encrypt(b"x", &[]).unwrap_err());
        assert!(err.contains("dotman key init"), "{err}");
    }
}
