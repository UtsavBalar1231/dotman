//! `{age}` sources, through the `age` or `rage` CLI. Plaintext only travels through pipes: it goes
//! to the CLI on stdin and comes back on stdout, so the repo only ever holds ciphertext.

use crate::config::{Allow, Config};
use crate::context::Context;
use crate::engine::write::{WriteOptions, WriteOutcome, safe_write};
use crate::engine::{Node, observe};
use crate::name;
use crate::output;
use crate::paths::{Paths, write_atomic};
use crate::proc::{self, AGE_KEY_VAR};
use crate::state::Kind;
use crate::sys;
use crate::variant::Flag;
use anyhow::{Context as _, Result, anyhow, bail};
use regex::bytes::{Regex, RegexBuilder};
use std::cell::{Cell, OnceCell};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const ARMOR: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";
// Every age checked reads `-i -` with the ciphertext named as a file: 1.0.0~rc1 (Debian 11), 1.0.0
// (Ubuntu 22.04) and 1.3.2. A CLI that does not may still read `-i /dev/stdin`, which opens the
// same pipe, so the key never reaches a file either way.
const STDIN_ARGS: [&str; 2] = ["-", "/dev/stdin"];
// Bounds the probes and `--version` only, as long as the bound on `git --version`: a real
// decryption can wait on a plugin that asks the user to touch a token, so it has no time limit.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL: Duration = Duration::from_millis(10);
const PROBE_TEXT: &[u8] = b"dotman probe\n";
// The recipient set the `{age}` sources were last all encrypted to. `.dotman/` is reserved, so
// this is never a managed key.
const RECORD: &str = ".dotman/age-recipients";
const RELEASES: &str = "https://github.com/FiloSottile/age/releases";
/// Where a key can come from, for messages about a missing one.
#[must_use]
pub fn key_hint() -> String {
    format!(
        "set DOTMAN_AGE_KEY or DOTMAN_AGE_KEY_FILE, or run `{} key import`",
        name::bin()
    )
}

pub fn no_recipients() -> String {
    format!(
        "[age] recipients in .dotman.toml is empty; `{} key init` creates a key and adds its recipient",
        name::bin()
    )
}

/// The rule that finds an age identity. No `[guard] allow` entry covers it.
pub const AGE_RULE: &str = "age-secret-key";

/// Git's rule for a binary file: a NUL in the first 8000 bytes. The content guard skips such a
/// file, or a tool that embeds a key shape, such as age-keygen, could never be stored.
pub const SNIFF_BYTES: usize = 8000;

// The age identity is `AGE-SECRET-KEY-1` and 58 characters of uppercase bech32 (32 bytes of key
// and a 6-character checksum), so a bare `AGE-SECRET-KEY-` in a script is not one.
const AGE_PATTERN: &str = r"AGE-SECRET-KEY-1[QPZRY9X8GF2TVDW0S3JN54KHCE6MUA7L]{58}";

// Every rule starts with a fixed prefix: without one, the combined scan is 100 times slower.
// Provider rules come from the research for the content guard; none of them is a keyword or
// entropy rule.
const RULES: [(&str, &str); 21] = [
    (AGE_RULE, AGE_PATTERN),
    ("github-token", r"\bgh[pousr]_[0-9A-Za-z]{36,251}\b"),
    (
        "github-fine-grained-pat",
        r"\bgithub_pat_[0-9A-Za-z_]{82,251}\b",
    ),
    ("gitlab-pat", r"\bglpat-[0-9A-Za-z_-]{20,}"),
    (
        "slack-token",
        r"\bxox(?:b-[0-9]{8,14}-[0-9A-Za-z-]{10,}|[pe](?:-[0-9]{10,13}){3}-[0-9A-Za-z-]{28,34}|[os]-[0-9]+-[0-9]+-[0-9]+-[0-9A-Fa-f]+|[ar]-(?:[0-9]-)?[0-9A-Za-z]{8,48})",
    ),
    (
        "slack-webhook",
        r"hooks\.slack\.com/(?:services|workflows|triggers)/[A-Za-z0-9+/]{43,56}",
    ),
    (
        "aws-access-key-id",
        r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b",
    ),
    ("stripe-live-key", r"\b[sr]k_live_[0-9A-Za-z]{10,99}\b"),
    (
        "openai-api-key",
        r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}T3BlbkFJ[A-Za-z0-9_-]{20,}",
    ),
    (
        "anthropic-api-key",
        r"\bsk-ant-[a-z]{3,6}[0-9]{2}-[A-Za-z0-9_-]{40,}",
    ),
    ("npm-token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    ("pypi-token", r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{50,}"),
    ("digitalocean-token", r"\bdo[opr]_v1_[a-f0-9]{64}\b"),
    (
        "sendgrid-key",
        r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b",
    ),
    ("huggingface-token", r"\b(?:hf_|api_org_)[A-Za-z]{34}\b"),
    (
        "private-key",
        r"-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----(?:\\[nr]|\s)*(?:[A-Za-z-]+:[^\n]*?(?:\\n|\n)(?:\\[nr]|\s)*)*[A-Za-z0-9+/=]{40,}",
    ),
    ("openrouter-key", r"\bsk-or-v1-[0-9a-f]{64}\b"),
    ("groq-key", r"\bgsk_[A-Za-z0-9]{52}\b"),
    ("xai-key", r"\bxai-[A-Za-z0-9]{80}\b"),
    ("perplexity-key", r"\bpplx-[A-Za-z0-9]{48}\b"),
    ("docker-pat", r"\bdckr_(?:pat|oat)_[A-Za-z0-9_-]{20,}"),
];

// One capture group per rule, in `RULES` order; the rules themselves only use `(?:...)`.
static SCANNER: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| {
    let combined = RULES
        .iter()
        .map(|(_, rule)| format!("({rule})"))
        .collect::<Vec<_>>()
        .join("|");
    RegexBuilder::new(&combined).unicode(false).build()
});

// The combined scan matches leftmost-first, so a longer match of another rule, such as a `glpat-`
// run or a PEM header line, swallows an age key that starts inside it. The age rule, which no
// allow entry covers, also runs alone.
static AGE_SCANNER: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| RegexBuilder::new(AGE_PATTERN).unicode(false).build());

/// What the content guard found: the rule, and the 1-based line where its match starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finding {
    /// A name from the rule list, such as `github-token`.
    pub rule: &'static str,
    /// 1-based.
    pub line: usize,
}

impl Finding {
    /// False for an age identity, which is never stored in plain text.
    #[must_use]
    pub fn overridable(self) -> bool {
        self.rule != AGE_RULE
    }

    /// Whether a `[guard] allow` entry for `key` covers this finding.
    #[must_use]
    pub fn allowed(self, key: &str, allow: &[Allow]) -> bool {
        self.overridable()
            && allow
                .iter()
                .any(|entry| entry.path == key && entry.rule == self.rule)
    }

    /// What follows the finding in a message: how to resolve the hold, with the commands for
    /// `path`, or with only the `add` flags.
    #[must_use]
    pub fn remedy(self, path: Option<&str>) -> String {
        let bin = name::bin();
        let never = ", which dotman never stores in plain text";
        match (path, self.overridable()) {
            (None, true) => {
                "; --encrypt stores it encrypted, --allow-plain stores it as is".to_owned()
            }
            (None, false) => format!("{never}; --encrypt stores it encrypted"),
            (Some(path), true) => format!(
                "; `{bin} add --encrypt {path}` stores it encrypted, `{bin} add --allow-plain {path}` stores it as is"
            ),
            (Some(path), false) => {
                format!("{never}; `{bin} add --encrypt {path}` stores it encrypted")
            }
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.overridable() {
            write!(f, "line {} holds a {}", self.line, self.rule)
        } else {
            write!(
                f,
                "line {} holds an age secret key (AGE-SECRET-KEY-)",
                self.line
            )
        }
    }
}

/// Every finding in `bytes`, in order; none in a file that [`looks_binary`]. The whole file is
/// scanned at once, so a match may span lines, as a PEM block does. Should the rules ever fail to
/// compile, every file is held.
pub fn findings(bytes: &[u8]) -> Box<dyn Iterator<Item = Finding> + '_> {
    if looks_binary(bytes) {
        return Box::new(std::iter::empty());
    }
    let (Ok(scanner), Ok(age)) = (SCANNER.as_ref(), AGE_SCANNER.as_ref()) else {
        return Box::new(std::iter::once(Finding {
            rule: "unscanned",
            line: 1,
        }));
    };
    let mut found: Vec<(usize, &'static str)> = scanner
        .captures_iter(bytes)
        .filter_map(|caps| {
            let (at, (rule, _)) = RULES
                .iter()
                .enumerate()
                .find(|(at, _)| caps.get(at.saturating_add(1)).is_some())?;
            Some((caps.get(at.saturating_add(1))?.start(), *rule))
        })
        .collect();
    found.extend(age.find_iter(bytes).map(|key| (key.start(), AGE_RULE)));
    found.sort_unstable();
    found.dedup();
    Box::new(found.into_iter().map(move |(start, rule)| Finding {
        rule,
        line: line_at(bytes, start),
    }))
}

/// A NUL in the first [`SNIFF_BYTES`].
#[must_use]
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(SNIFF_BYTES).any(|b| *b == 0)
}

// The text before the match splits at each newline into one piece more than it has newlines,
// which is the 1-based line the match starts on.
fn line_at(bytes: &[u8], offset: usize) -> usize {
    bytes
        .get(..offset)
        .unwrap_or(bytes)
        .split(|b| *b == b'\n')
        .count()
}

/// The first finding in `bytes` that no `[guard] allow` entry for `key` covers, an age key before
/// any credential, since no flag stores one in plain text.
#[must_use]
pub fn held(bytes: &[u8], key: &str, allow: &[Allow]) -> Option<Finding> {
    findings(bytes)
        .filter(|found| !found.allowed(key, allow))
        .min_by_key(|found| found.overridable())
}

/// Whether `bytes` hold an age identity or a credential a rule finds, whatever `[guard] allow`
/// says: for captures that no allow entry reaches, such as system files.
#[must_use]
pub fn holds_secret_key(bytes: &[u8]) -> bool {
    findings(bytes).next().is_some()
}

/// What [`holds_secret_key`] found in the system file `key`, for the message about a [`HoldsKey`]
/// capture, which carries no finding.
#[must_use]
pub fn system_finding(ctx: &Context, key: &str) -> String {
    match ctx.system_scope().node(key) {
        Ok(Node::File { bytes, .. }) => findings(&bytes).next().map(|found| found.to_string()),
        _ => None,
    }
    .unwrap_or_else(|| "it holds an age secret key or a credential".to_owned())
}

/// A plain capture would store what the content guard found.
#[derive(Debug)]
pub struct Held(pub Finding);

impl fmt::Display for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.0, self.0.remedy(None))
    }
}

impl std::error::Error for Held {}

/// No key here decrypts a source. Report-only: the path is left alone.
#[derive(Debug)]
pub struct Locked(pub String);

impl fmt::Display for Locked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "locked: {}", self.0)
    }
}

impl std::error::Error for Locked {}

/// A capture would store what [`holds_secret_key`] finds where no override applies, as in system
/// scope.
#[derive(Debug)]
pub struct HoldsKey;

impl fmt::Display for HoldsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "holds an age secret key or a credential, which system scope never stores because it does not encrypt",
        )
    }
}

impl std::error::Error for HoldsKey {}

#[derive(Debug, Clone)]
struct Cli {
    program: PathBuf,
    rage: bool,
}

/// An age CLI that reads `DOTMAN_AGE_KEY` from its stdin.
#[derive(Debug)]
struct StdinReader {
    program: PathBuf,
    /// One of [`STDIN_ARGS`].
    arg: &'static str,
}

#[derive(Debug)]
struct Reader {
    program: PathBuf,
    /// The `-i` argument that reads `DOTMAN_AGE_KEY`, when a CLI here can.
    stdin_arg: Option<&'static str>,
    files: Vec<PathBuf>,
    locked: Option<String>,
    /// Printed once, by the first decryption.
    notes: Vec<String>,
    /// With `DOTMAN_AGE_KEY` set: the age that reads it, or why none can.
    key_cli: Option<Result<PathBuf, String>>,
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
                    stdin_arg: None,
                    files: Vec::new(),
                    locked: Some(format!("{e:#}")),
                    notes: Vec::new(),
                    key_cli: self.key.as_ref().map(|_| Err(format!("{e:#}"))),
                };
            }
        };
        let mut notes = Vec::new();
        let mut program = cli.program.clone();
        let key_reader = self.key.as_ref().map(|_| Self::stdin_reader(&cli));
        let stdin_arg = match &key_reader {
            Some(Ok(found)) => {
                program.clone_from(&found.program);
                Some(found.arg)
            }
            Some(Err(why)) => {
                notes.push(format!(
                    "{AGE_KEY_VAR} is set but cannot be used: {why}. dotman never writes the key to a file, so put it in one yourself and point DOTMAN_AGE_KEY_FILE at it, or run `{} key import`",
                    name::bin()
                ));
                None
            }
            None => None,
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
        let locked = (stdin_arg.is_none() && files.is_empty()).then(|| {
            if self.key.is_some() {
                format!("{AGE_KEY_VAR} cannot be used here and there is no identity file")
            } else {
                format!("no age key; {}", key_hint())
            }
        });
        Reader {
            program,
            stdin_arg,
            files,
            locked,
            notes,
            key_cli: key_reader.map(|found| found.map(|found| found.program)),
        }
    }

    // What a CLI can do is probed, not read from `--version`, which a distro build can print as
    // `(devel)`. With rage selected, the `age` on PATH is the fallback.
    fn stdin_reader(cli: &Cli) -> Result<StdinReader, String> {
        let probe = Probe::new()?;
        let mut candidates = vec![cli.program.clone()];
        candidates.extend(proc::find_program("age").filter(|_| cli.rage));
        for program in &candidates {
            if let Some(arg) = STDIN_ARGS.into_iter().find(|arg| probe.reads(program, arg)) {
                return Ok(StdinReader {
                    program: program.clone(),
                    arg,
                });
            }
        }
        let tried: Vec<String> = candidates
            .iter()
            .map(|program| format!("{} ({})", program.display(), version_of(program)))
            .collect();
        Err(format!(
            "{} cannot read an identity from stdin",
            tried.join(" or ")
        ))
    }

    /// `None` when `DOTMAN_AGE_KEY` is unset; else the version of the age that reads it from
    /// stdin, or why no installed CLI can.
    #[must_use]
    pub fn key_reader(&self) -> Option<Result<String, String>> {
        let found = self.reader().key_cli.as_ref()?;
        Some(
            found
                .as_ref()
                .map(|program| version_of(program))
                .map_err(Clone::clone),
        )
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
        if let Some(arg) = reader.stdin_arg {
            cmd.args(["-i", arg]);
        }
        for file in &reader.files {
            cmd.arg("-i").arg(file);
        }
        cmd.arg("--").arg(ciphertext);
        let key = self
            .key
            .as_ref()
            .filter(|_| reader.stdin_arg.is_some())
            .map(|key| format!("{key}\n"));
        let out = run(cmd, key.as_deref().map(str::as_bytes))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        // Seen with age 1.3.2 ("no identity matched any of the recipients") and rage 0.12.1
        // ("No matching keys found").
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
    /// identities make age-keygen fail. `-y` on stdin was checked with age-keygen 1.3.2 and
    /// rage-keygen 0.12.1.
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
                format!(
                    "encrypted files need rage or age, and neither is installed; {}",
                    install_hint(cfg!(target_os = "macos"), read_os_release().as_deref())
                )
            })?,
    };
    let rage = program
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("rage"));
    Ok(Cli { program, rage })
}

fn read_os_release() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| fs::read_to_string(path).ok())
}

/// The way to install age here, from `ID`, `ID_LIKE` and `VERSION_ID`, parsed as the git hint
/// parses them and never sourced. Where the distro has no package, the release binaries or rage.
fn install_hint(macos: bool, os_release: Option<&str>) -> String {
    let fallback = format!("download a release binary from {RELEASES}, or `cargo install rage`");
    if macos {
        return "install one with `brew install age`".to_owned();
    }
    let (mut ids, mut version) = (Vec::new(), String::new());
    for line in os_release.unwrap_or_default().lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        match key.trim() {
            "ID" => ids.insert(0, value.to_owned()),
            "ID_LIKE" => ids.extend(value.split_whitespace().map(str::to_owned)),
            "VERSION_ID" => value.clone_into(&mut version),
            _ => {}
        }
    }
    let major = version
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok());
    // Ubuntu has age from 22.04 and Debian from 11; a derivative's own version says nothing.
    let too_old = match ids.first().map(String::as_str) {
        Some("ubuntu") => major.is_some_and(|major| major < 22),
        Some("debian") => major.is_some_and(|major| major < 11),
        _ => false,
    };
    // `Some(None)`: this distro is known to have no package. RHEL and its rebuilds carry age only
    // in EPEL, and Amazon Linux has neither, though both list fedora in `ID_LIKE`.
    let package = ids.iter().find_map(|id| match id.as_str() {
        "arch" => Some(Some("pacman -S age")),
        "alpine" => Some(Some("apk add age")),
        "opensuse" | "suse" => Some(Some("zypper install age")),
        "rhel" | "centos" | "rocky" | "almalinux" | "ol" => {
            Some(Some("dnf install epel-release && dnf install age"))
        }
        "amzn" => Some(None),
        "fedora" => Some(Some("dnf install age")),
        "debian" | "ubuntu" if too_old => Some(None),
        "debian" | "ubuntu" => Some(Some("apt install age")),
        _ => None,
    });
    match package.flatten() {
        Some(package) if package.contains("epel") => {
            format!("install one with `{package}`, or {fallback}")
        }
        Some(package) => format!("install one with `{package}`"),
        None => fallback,
    }
}

/// The version an age CLI prints, for messages only: what it can do is probed. `--version` is
/// bounded, so a hung CLI cannot hold the lock.
fn version_of(program: &Path) -> String {
    let mut cmd = proc::command(program);
    cmd.arg("--version");
    let Ok(Some(out)) = proc::output_within(&mut cmd, PROBE_TIMEOUT) else {
        return "version unknown".to_owned();
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push(' ');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let parsed = text
        .split_whitespace()
        .find_map(parse_version)
        .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"));
    parsed
        .or_else(|| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "version unknown".to_owned())
}

// A throwaway identity and `PROBE_TEXT` encrypted to it. They protect nothing: decrypting the file
// with the identity on stdin shows whether a CLI reads one there, with no keygen installed. The
// identity is stored in lowercase and in pieces, so the binary holds no key shape for the content
// guard, or another scanner, to find in it.
const PROBE_IDENTITY: [&str; 3] = [
    "age-secret-key-1",
    "e6yrnetl4eaaft35vteh5delq64wd7gg",
    "suz8aku0sd08vnhfrafsg60lpx",
];
const PROBE_FILE: &str = "-----BEGIN AGE ENCRYPTED FILE-----
YWdlLWVuY3J5cHRpb24ub3JnL3YxCi0+IFgyNTUxOSBIdElwYURDMXBSczV5UGNj
QSswWGhNTVdrL2d4dlRwMjRhSFZ2OTMzZGtjCmx5TVI3enM2cStJMFFiMUN4cU4z
SHZDdUlmZVlVNHJab0gvQ3VnNldOZEEKLS0tIG5iRk1hNjN4dDUvd1RjbnJ3dHlC
TUlENkROd1JPWmIwc0hOUHhRbldQM2sKV10cXQZDhqzdlXZRzCPZtZDiYt3AeHxW
QOe2NBxAoJC7VlGzEsT1MFAFXNCa
-----END AGE ENCRYPTED FILE-----
";

/// [`PROBE_FILE`] in a temp file, which a CLI decrypts with the probe identity on stdin.
struct Probe {
    // Removed on drop.
    dir: tempfile::TempDir,
}

impl Probe {
    fn new() -> Result<Self, String> {
        let dir = tempfile::Builder::new()
            .prefix("dotman-age-probe-")
            .tempdir()
            .map_err(|e| format!("cannot create a temp dir: {e}"))?;
        fs::write(dir.path().join("probe.age"), PROBE_FILE)
            .map_err(|e| format!("cannot write the probe: {e}"))?;
        Ok(Self { dir })
    }

    fn reads(&self, program: &Path, arg: &str) -> bool {
        let mut identity = PROBE_IDENTITY.concat().to_ascii_uppercase();
        identity.push('\n');
        let mut cmd = proc::command(program);
        cmd.args(["-d", "-i", arg, "--"])
            .arg(self.dir.path().join("probe.age"));
        run_within(cmd, identity.as_bytes())
            .is_some_and(|out| out.status.success() && out.stdout == PROBE_TEXT)
    }
}

/// [`run`] for the probes: `None` when the child cannot start or is still running after
/// [`PROBE_TIMEOUT`], when it is killed. Every pipe is read or written by a thread of its own, so
/// the poll sees the deadline whatever the child does with them.
fn run_within(mut cmd: Command, input: &[u8]) -> Option<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _sigpipe = sys::ignore_sigpipe();
    let mut child = cmd.spawn().ok()?;
    let stdin = child.stdin.take();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.map(|mut pipe| pipe.write_all(input)));
        let out = scope.spawn(move || read_all(stdout));
        let err = scope.spawn(move || read_all(stderr));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() < PROBE_TIMEOUT => std::thread::sleep(PROBE_POLL),
                _ => {
                    // The kill closes the child's ends of the pipes, so the threads finish.
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let _ = writer.join();
        let (stdout, stderr) = (out.join().ok()?, err.join().ok()?);
        Some(Output {
            status: status?,
            stdout,
            stderr,
        })
    })
}

fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes);
    }
    bytes
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
pub fn read_record(repo: &Path) -> Result<Option<Vec<String>>> {
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
pub fn write_record(repo: &Path, recipients: &[String]) -> Result<()> {
    crate::commands::add::ensure_worktree_parents(repo, RECORD)?;
    let mut text = recipient_set(recipients).join("\n");
    text.push('\n');
    write_atomic(&repo.join(RECORD), text.as_bytes(), 0o644)
}

/// Replaces the ciphertext at `source` in the repo through the safe write path.
pub fn write_ciphertext(repo: &Path, source: &str, ciphertext: &[u8]) -> Result<()> {
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
pub fn encrypted_sources(ctx: &Context) -> Result<Vec<String>> {
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
pub fn reencrypt(ctx: &Context, dry_run: bool) -> Result<usize> {
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
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn bech32(n: usize) -> String {
        "QPZRY9X8GF2TVDW0S3JN54KHCE6MUA7L"
            .chars()
            .cycle()
            .take(n)
            .collect()
    }

    fn age_key(data: &str) -> String {
        format!("AGE-SECRET-KEY-1{data}")
    }

    fn found(text: &str) -> Vec<(&'static str, usize)> {
        findings(text.as_bytes())
            .map(|found| (found.rule, found.line))
            .collect()
    }

    #[test]
    fn the_rules_compile_into_one_scanner() {
        assert!(SCANNER.is_ok(), "{:?}", SCANNER.as_ref().err());
        let names: BTreeSet<&str> = RULES.iter().map(|(name, _)| *name).collect();
        assert_eq!(names.len(), RULES.len(), "rule names are unique");
    }

    #[test]
    fn only_the_full_age_key_shape_is_an_age_key() {
        let full = age_key(&bech32(58));
        assert_eq!(found(&full), [(AGE_RULE, 1)]);
        assert_eq!(found(&format!("# created\n{full}\n")), [(AGE_RULE, 2)]);
        for text in [
            "alias agekeys='grep AGE-SECRET-KEY- ~/.config/age/keys.txt'".to_owned(),
            "AGE-SECRET-KEY-".to_owned(),
            age_key(&bech32(57)),
            age_key(&"B".repeat(58)),
            age_key(&bech32(58).to_lowercase()),
            format!("AGE-SECRET-KEY-2{}", bech32(58)),
        ] {
            assert_eq!(found(&text), [], "{text}");
        }
        assert!(holds_secret_key(full.as_bytes()));
        assert!(!holds_secret_key(b"AGE-SECRET-KEY-1QQQQ"));
    }

    #[test]
    fn each_rule_names_itself_and_near_misses_are_not_held() {
        let alnum = |n: usize| "aB3".repeat(n).chars().take(n).collect::<String>();
        let upper = |n: usize| "AB2C".repeat(n).chars().take(n).collect::<String>();
        let hex = |n: usize| "a1f9".repeat(n).chars().take(n).collect::<String>();
        let alpha = |n: usize| "aBc".repeat(n).chars().take(n).collect::<String>();
        let pem_body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQ";
        let samples = [
            ("github-token", format!("export GH=ghp_{}", alnum(36))),
            (
                "github-fine-grained-pat",
                format!("github_pat_{}", alnum(82)),
            ),
            ("gitlab-pat", format!("glpat-{}", alnum(20))),
            ("slack-token", format!("xoxb-12345678-{}", alnum(10))),
            (
                "slack-webhook",
                format!("https://hooks.slack.com/services/{}", alnum(43)),
            ),
            ("aws-access-key-id", format!("AKIA{}", upper(16))),
            ("stripe-live-key", format!("sk_live_{}", alnum(24))),
            (
                "openai-api-key",
                format!("sk-proj-{}T3BlbkFJ{}", alnum(20), alnum(20)),
            ),
            ("anthropic-api-key", format!("sk-ant-api03-{}", alnum(40))),
            ("npm-token", format!("npm_{}", alnum(36))),
            ("pypi-token", format!("pypi-AgEIcHlwaS5vcmc{}", alnum(50))),
            ("digitalocean-token", format!("dop_v1_{}", hex(64))),
            ("sendgrid-key", format!("SG.{}.{}", alnum(22), alnum(43))),
            ("huggingface-token", format!("hf_{}", alpha(34))),
            (
                "private-key",
                format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{pem_body}\n"),
            ),
            ("openrouter-key", format!("sk-or-v1-{}", hex(64))),
            ("groq-key", format!("gsk_{}", alnum(52))),
            ("xai-key", format!("xai-{}", alnum(80))),
            ("perplexity-key", format!("pplx-{}", alnum(48))),
            ("docker-pat", format!("dckr_pat_{}", alnum(20))),
        ];
        let named: BTreeSet<&str> = samples.iter().map(|(rule, _)| *rule).collect();
        assert_eq!(
            named.len(),
            RULES.len().saturating_sub(1),
            "a sample per rule"
        );
        for (rule, text) in &samples {
            assert_eq!(found(text), [(*rule, 1)], "{rule}");
        }
        for text in [
            format!("ghp_{}", alnum(35)),
            format!("xghp_{}", alnum(36)),
            "token = \"sk-not-a-key\"".to_owned(),
            format!("AKIA{}", upper(15)),
            "-----BEGIN OPENSSH PRIVATE KEY-----\n...\n".to_owned(),
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA\n".to_owned(),
            "password = hunter2".to_owned(),
        ] {
            assert_eq!(found(&text), [], "{text}");
        }
    }

    #[test]
    fn the_line_is_where_the_match_starts_counted_from_one() {
        let token = format!("ghp_{}", "x".repeat(36));
        assert_eq!(found(&token), [("github-token", 1)]);
        assert_eq!(found(&format!("\n{token}")), [("github-token", 2)]);
        assert_eq!(
            found(&format!("a\r\nb\r\n  {token}\r\n")),
            [("github-token", 3)]
        );
        assert_eq!(
            found(&format!("{token}\n\n{token} {token}\n")),
            [
                ("github-token", 1),
                ("github-token", 3),
                ("github-token", 3)
            ]
        );
        let pem = format!(
            "# key\n-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n\n{}\n",
            "A".repeat(64)
        );
        assert_eq!(found(&pem), [("private-key", 2)]);
    }

    #[test]
    fn an_age_key_inside_another_rules_match_is_still_found() {
        let key = age_key(&bech32(58));
        let in_glpat = format!("export GL=glpat-{key}\n");
        let in_pem = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nComment: {key}\n{}\n",
            "A".repeat(64)
        );
        assert_eq!(found(&in_glpat), [("gitlab-pat", 1), (AGE_RULE, 1)]);
        assert_eq!(found(&in_pem), [("private-key", 1), (AGE_RULE, 2)]);
        for (text, rule) in [(&in_glpat, "gitlab-pat"), (&in_pem, "private-key")] {
            let allow = [Allow {
                path: "k".to_owned(),
                rule: rule.to_owned(),
            }];
            assert_eq!(
                held(text.as_bytes(), "k", &allow).map(|f| f.rule),
                Some(AGE_RULE),
                "{rule}"
            );
        }
        assert_eq!(found(&format!("{key}\n")), [(AGE_RULE, 1)], "found once");
    }

    #[test]
    fn a_file_with_a_nul_in_its_first_8000_bytes_is_not_scanned() {
        let key = age_key(&bech32(58));
        let mut binary = b"\x7fELF\0".to_vec();
        binary.extend_from_slice(key.as_bytes());
        assert!(looks_binary(&binary));
        assert_eq!(findings(&binary).count(), 0);
        let mut late = vec![b'#'; SNIFF_BYTES];
        late.push(0);
        late.extend_from_slice(key.as_bytes());
        assert!(!looks_binary(&late));
        assert_eq!(
            findings(&late).map(|f| f.rule).collect::<Vec<_>>(),
            [AGE_RULE]
        );
    }

    #[test]
    fn the_probe_identity_is_a_key_only_once_assembled() {
        let identity = PROBE_IDENTITY.concat().to_ascii_uppercase();
        assert_eq!(found(&identity), [(AGE_RULE, 1)]);
        for piece in PROBE_IDENTITY {
            assert_eq!(found(piece), [], "{piece}");
            assert_eq!(found(&piece.to_ascii_uppercase()), [], "{piece}");
        }
        assert_eq!(found(PROBE_FILE), []);
        assert!(PROBE_FILE.as_bytes().starts_with(ARMOR));
    }

    #[test]
    fn allow_entries_match_by_key_and_rule_and_never_cover_an_age_key() {
        let allow = [Allow {
            path: ".config/zsh/local.zsh".to_owned(),
            rule: "github-token".to_owned(),
        }];
        let text = format!(
            "a\nexport GH=ghp_{}\nexport NPM=npm_{}\n",
            "x".repeat(36),
            "y".repeat(36)
        );
        let held = |key: &str| held(text.as_bytes(), key, &allow);
        assert_eq!(
            held(".config/zsh/local.zsh"),
            Some(Finding {
                rule: "npm-token",
                line: 3
            })
        );
        assert_eq!(held(".zshrc").map(|f| f.rule), Some("github-token"));
        let age = Finding {
            rule: AGE_RULE,
            line: 1,
        };
        let allow_age = [Allow {
            path: "k".to_owned(),
            rule: AGE_RULE.to_owned(),
        }];
        assert!(!age.allowed("k", &allow_age));
        assert!(!age.overridable());
        assert_eq!(
            Held(age).to_string(),
            "line 1 holds an age secret key (AGE-SECRET-KEY-), which dotman never stores in plain text; --encrypt stores it encrypted"
        );
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
    }

    #[test]
    fn the_install_hint_follows_os_release_and_falls_back_to_the_releases() {
        let hint = |text: &str| install_hint(false, Some(text));
        assert!(hint("ID=arch\n").contains("`pacman -S age`"));
        assert!(hint("ID=manjaro\nID_LIKE=arch\n").contains("`pacman -S age`"));
        assert!(hint("ID=ubuntu\nVERSION_ID=\"22.04\"\n").contains("`apt install age`"));
        assert!(hint("ID=debian\nVERSION_ID=\"13\"\n").contains("`apt install age`"));
        assert!(
            hint("ID=debian\n").contains("`apt install age`"),
            "testing has no VERSION_ID"
        );
        assert!(
            hint("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"21.3\"\n")
                .contains("`apt install age`")
        );
        assert!(hint("ID=fedora\nVERSION_ID=45\n").contains("`dnf install age`"));
        assert!(hint("ID=alpine\nVERSION_ID=3.24.0\n").contains("`apk add age`"));
        assert!(
            hint("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n")
                .contains("`zypper install age`")
        );
        for old in [
            "ID=ubuntu\nVERSION_ID=\"20.04\"\n",
            "ID=debian\nVERSION_ID=\"10\"\n",
        ] {
            let text = hint(old);
            assert!(!text.contains("apt install"), "{text}");
            assert!(text.contains(RELEASES) && text.contains("rage"), "{text}");
        }
        let rocky = hint("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"9.6\"\n");
        assert!(
            rocky.contains("epel-release") && rocky.contains(RELEASES),
            "{rocky}"
        );
        assert!(!rocky.contains("`dnf install age`"), "{rocky}");
        let amazon = hint("ID=\"amzn\"\nID_LIKE=\"fedora\"\nVERSION_ID=\"2023\"\n");
        assert!(amazon.contains(RELEASES), "{amazon}");
        assert_eq!(hint("ID=plan9\n"), amazon);
        assert!(install_hint(true, None).contains("`brew install age`"));
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
