//! `dotman system-write --plan <file>`, the only code that runs as root.
//!
//! The plan and every file of the invoking user are treated as hostile. The helper takes nothing
//! from the caller's environment: the roots come from the user's own machine config, every entry
//! is checked again before anything is written, and each write goes through directory
//! descriptors that never follow a symlink.
//!
//! A plan is not transactional across files. Entries are written in order, each one atomically,
//! and the first failure stops the rest, which are reported as not written.

use super::{Scope, Standing, canonical_form, rel};
use crate::config::Config;
use crate::context::TEMP_PREFIX;
use crate::engine::{hash_hex, parse_mode};
use crate::output;
use crate::proc;
use crate::sys::{self, EntryKind};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use libc::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_64;

const PLAN_CAP: u64 = 256 << 20;
const CONFIG_CAP: u64 = 1 << 20;
const SELINUX_ENFORCE: &str = "/sys/fs/selinux/enforce";
// As root the caller's PATH could name any program, so restorecon is looked for here only.
const RELABEL_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

// The first word of each result line on stdout. `skipped-owner` is a write that left the owner
// and group alone, because the helper was not root; `not-written` follows another entry's failure.
pub(crate) const WRITTEN: &str = "written";
pub(crate) const SKIPPED_OWNER: &str = "skipped-owner";
pub(crate) const FAILED: &str = "failed";
pub(crate) const NOT_WRITTEN: &str = "not-written";

/// What `apply --system` hands to `system-write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    /// Absolute path of the invoking user's machine config, whose `[system] roots` the helper reads.
    pub config: String,
    /// The usable roots in canonical form, as the user resolved them.
    pub roots: Vec<String>,
    /// `DOTMAN_SYSTEM_ROOT`. Only an unprivileged helper under `escalate = "none"` accepts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Written in this order.
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanEntry {
    /// Absolute, in canonical form.
    pub target: String,
    /// Lowercase hex, since JSON cannot carry raw bytes.
    pub bytes: String,
    /// xxh3-128 of the content, as 32 hex chars.
    pub xxh3: String,
    pub owner: String,
    pub group: String,
    /// Octal, such as `0644`. Setuid, setgid and sticky are dropped.
    pub mode: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reported {
    Written,
    OwnerSkipped,
    Failed,
    NotWritten,
}

/// A target as result lines show it. Escaped, so a newline in a path cannot forge another line.
pub(crate) fn shown(target: &str) -> String {
    target.escape_debug().to_string()
}

/// The line `stdout` holds for `target`, with the reason when it failed.
pub(crate) fn reported<'a>(stdout: &'a str, target: &str) -> Option<(Reported, &'a str)> {
    let shown = shown(target);
    stdout.lines().find_map(|line| {
        let (word, rest) = line.split_once(' ')?;
        let status = match word {
            WRITTEN => Reported::Written,
            SKIPPED_OWNER => Reported::OwnerSkipped,
            NOT_WRITTEN => Reported::NotWritten,
            FAILED => {
                let why = rest.strip_prefix(shown.as_str())?.strip_prefix(": ")?;
                return Some((Reported::Failed, why));
            }
            _ => return None,
        };
        (rest == shown).then_some((status, ""))
    })
}

/// The roots that count, in canonical form.
pub(crate) fn usable_roots(scope: &Scope) -> Vec<String> {
    scope
        .roots()
        .iter()
        .filter(|root| root.problem.is_none())
        .map(|root| root.canon.clone())
        .collect()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        for nibble in [byte / 16, byte % 16] {
            out.extend(char::from_digit(u32::from(nibble), 16));
        }
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = <[u8; 2]>::try_from(pair).ok()?;
            if !digits.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            u8::from_str_radix(std::str::from_utf8(&digits).ok()?, 16).ok()
        })
        .collect()
}

/// Runs `dotman system-write --plan <file>`.
///
/// # Errors
///
/// Fails when the caller may not run a plan, the plan or config is unreadable or not owned by the
/// invoking user, its roots differ from `[system] roots`, an entry is rejected (nothing is
/// written), or a write fails (later entries are skipped).
pub fn run(plan_path: &Path) -> Result<()> {
    let euid = sys::geteuid();
    let invoking = invoking_user(
        euid,
        |name| std::env::var_os(name),
        || {
            let dir = plan_path
                .parent()
                .ok_or_else(|| anyhow!("{} has no directory", plan_path.display()))?;
            Ok(fs::symlink_metadata(dir)
                .with_context(|| format!("cannot stat {}", dir.display()))?
                .uid())
        },
    )?;
    let plan = read_plan(plan_path, invoking)?;
    let config = read_config(Path::new(&plan.config), invoking)?;
    let env_prefix = std::env::var_os("DOTMAN_SYSTEM_ROOT").is_some_and(|v| !v.is_empty());
    gate(
        euid,
        config.system.escalate.as_deref(),
        plan.prefix.is_some(),
        env_prefix,
    )?;

    let prefix = plan.prefix.as_deref().map(PathBuf::from);
    if let Some(prefix) = &prefix {
        ensure!(
            prefix.is_absolute(),
            "the plan's prefix {} is not absolute",
            prefix.display()
        );
    }
    let scope = Scope::build(
        prefix.clone(),
        &config.system.roots,
        cfg!(target_os = "macos"),
    );
    let configured: BTreeSet<String> = usable_roots(&scope).into_iter().collect();
    let planned: BTreeSet<String> = plan.roots.iter().cloned().collect();
    ensure!(
        configured == planned,
        "the plan's roots ({}) differ from [system] roots in {} ({}); nothing was written",
        list(&planned),
        plan.config,
        list(&configured)
    );

    let base_path = prefix.unwrap_or_else(|| PathBuf::from("/"));
    let base = OwnedFd::from(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&base_path)
            .with_context(|| format!("cannot open {}", base_path.display()))?,
    );
    let mut dirs = Dirs::default();
    let mut seen = BTreeSet::new();
    let checked: Vec<Result<Ready>> = plan
        .entries
        .iter()
        .map(|entry| check(&scope, &base_path, &base, &mut dirs, &mut seen, entry))
        .collect();

    let mut out = io::stdout().lock();
    if checked.iter().any(Result::is_err) {
        for (entry, result) in plan.entries.iter().zip(&checked) {
            let target = shown(&entry.target);
            match result {
                Ok(_) => writeln!(out, "{NOT_WRITTEN} {target}"),
                Err(e) => writeln!(out, "{FAILED} {target}: {}", one_line(e)),
            }
            .context("cannot write to stdout")?;
        }
        bail!("the plan was rejected; nothing was written");
    }

    // Each entry takes milliseconds, and an interrupt in the middle would leave a temp file in a
    // system directory, so the batch runs to its end. Only SIGKILL can still stop it.
    let _ignored = sys::ignore_signals(&[SIGINT, SIGQUIT, SIGHUP, SIGTERM]);
    let as_root = euid == 0;
    let mut failed = 0_usize;
    for ready in checked.into_iter().flatten() {
        let target = shown(&ready.target);
        if failed > 0 {
            failed = failed.saturating_add(1);
            writeln!(out, "{NOT_WRITTEN} {target}").context("cannot write to stdout")?;
            continue;
        }
        match write(&dirs, &ready, as_root) {
            Ok(()) => {
                let word = if as_root { WRITTEN } else { SKIPPED_OWNER };
                writeln!(out, "{word} {target}").context("cannot write to stdout")?;
                relabel(&ready.disk);
            }
            Err(e) => {
                failed = 1;
                writeln!(out, "{FAILED} {target}: {}", one_line(&e))
                    .context("cannot write to stdout")?;
            }
        }
    }
    if failed > 0 {
        bail!("{}", not_written(failed, plan.entries.len()));
    }
    Ok(())
}

fn not_written(failed: usize, total: usize) -> String {
    format!(
        "{failed} of {total} {} {} not written",
        output::plural(total, "file", "files"),
        output::plural(failed, "was", "were"),
    )
}

fn list(roots: &BTreeSet<String>) -> String {
    if roots.is_empty() {
        return "none".to_owned();
    }
    roots.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn one_line(error: &anyhow::Error) -> String {
    format!("{error:#}").replace('\n', " ")
}

/// Root may run any plan without a prefix. Anyone else may run one only under
/// `escalate = "none"`, which writes with the caller's own rights.
fn gate(euid: u32, escalate: Option<&str>, plan_prefix: bool, env_prefix: bool) -> Result<()> {
    if euid == 0 {
        // Every check applies below the prefix, so a prefix chosen by the user would let root
        // write outside [system] roots, into directories the user controls.
        ensure!(
            !env_prefix,
            "DOTMAN_SYSTEM_ROOT is set; system-write never honors it as root"
        );
        ensure!(
            !plan_prefix,
            "the plan carries a DOTMAN_SYSTEM_ROOT prefix; system-write never honors one as root"
        );
        return Ok(());
    }
    ensure!(
        escalate == Some("none"),
        "system-write runs as root; as uid {euid} it runs only under `escalate = \"none\"`"
    );
    Ok(())
}

/// Who asked for the write: the caller itself when not root, else what sudo, run0 or doas
/// recorded, else the owner of the plan's directory.
fn invoking_user(
    euid: u32,
    env: impl Fn(&str) -> Option<OsString>,
    plan_dir_owner: impl FnOnce() -> Result<u32>,
) -> Result<u32> {
    if euid != 0 {
        return Ok(euid);
    }
    let set = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(uid) = set("SUDO_UID") {
        return uid
            .to_str()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| anyhow!("SUDO_UID ({}) is not a uid", uid.display()));
    }
    if let Some(user) = set("DOAS_USER") {
        let name = user
            .to_str()
            .ok_or_else(|| anyhow!("DOAS_USER is not valid UTF-8"))?;
        return sys::passwd_by_name(name)
            .map(|entry| entry.uid)
            .ok_or_else(|| anyhow!("DOAS_USER ({name}) has no passwd entry"));
    }
    // No other user can create a root-owned directory, so a root-owned plan directory means root
    // wrote the plan and started system-write itself.
    plan_dir_owner()
}

fn read_plan(path: &Path, uid: u32) -> Result<Plan> {
    ensure!(
        path.is_absolute(),
        "the plan path {} is not absolute",
        path.display()
    );
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory", path.display()))?;
    for dir in [Some(dir), dir.parent()].into_iter().flatten() {
        let meta =
            fs::symlink_metadata(dir).with_context(|| format!("cannot stat {}", dir.display()))?;
        ensure!(meta.is_dir(), "{} is not a directory", dir.display());
        owned_by(dir, meta.uid(), uid)?;
        ensure!(
            meta.mode() & 0o022 == 0,
            "{} is writable by group or others",
            dir.display()
        );
    }
    let (mode, bytes) = read_owned(path, uid, PLAN_CAP)?;
    ensure!(
        mode == 0o600,
        "the plan {} has mode {mode:04o}, not 0600",
        path.display()
    );
    serde_json::from_slice(&bytes).with_context(|| format!("cannot parse {}", path.display()))
}

fn read_config(path: &Path, uid: u32) -> Result<Config> {
    ensure!(
        path.is_absolute(),
        "the machine config path {} is not absolute",
        path.display()
    );
    let (mode, bytes) = read_owned(path, uid, CONFIG_CAP)?;
    ensure!(
        mode & 0o022 == 0,
        "{} is writable by group or others; run `chmod go-w {}`",
        path.display(),
        shell_words::quote(&path.to_string_lossy())
    );
    let text =
        String::from_utf8(bytes).map_err(|_| anyhow!("{} is not valid UTF-8", path.display()))?;
    let (config, _unknown) =
        Config::parse(&text).with_context(|| format!("cannot parse {}", path.display()))?;
    Ok(config)
}

fn owned_by(path: &Path, owner: u32, uid: u32) -> Result<()> {
    ensure!(
        owner == uid,
        "{} is owned by uid {owner}, not by uid {uid}, who started system-write",
        path.display()
    );
    Ok(())
}

/// Mode and bytes of the regular file at `path`, which `uid` owns. The checks run on the open
/// descriptor, so the file cannot be swapped between the check and the read.
fn read_owned(path: &Path, uid: u32, cap: u64) -> Result<(u32, Vec<u8>)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                anyhow!("{} is a symlink", path.display())
            } else {
                anyhow!(e).context(format!("cannot open {}", path.display()))
            }
        })?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "{} is not a regular file", path.display());
    owned_by(path, meta.uid(), uid)?;
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        u64::try_from(bytes.len()).is_ok_and(|len| len <= cap),
        "{} is larger than {cap} bytes",
        path.display()
    );
    Ok((meta.mode() & 0o7777, bytes))
}

/// One entry that passed every check.
struct Ready {
    target: String,
    /// For restorecon: the target below the prefix.
    disk: PathBuf,
    dir: usize,
    name: CString,
    bytes: Vec<u8>,
    uid: u32,
    gid: u32,
    mode: u32,
}

/// The entries' parent directories, each opened once by a walk that follows no symlink.
#[derive(Default)]
struct Dirs {
    open: Vec<OwnedFd>,
    by_path: BTreeMap<String, usize>,
}

impl Dirs {
    fn open(&mut self, base: &OwnedFd, parent: &str) -> Result<usize> {
        if let Some(&index) = self.by_path.get(parent) {
            return Ok(index);
        }
        let mut current: Option<OwnedFd> = None;
        let mut walked = String::new();
        for part in parent.split('/').filter(|part| !part.is_empty()) {
            walked.push('/');
            walked.push_str(part);
            let name = CString::new(part)?;
            let from = current.as_ref().map_or_else(|| base.as_fd(), AsFd::as_fd);
            current = Some(sys::openat_dir(from, &name).map_err(|e| walk_error(&walked, &e))?);
        }
        let fd = match current {
            Some(fd) => fd,
            None => base.try_clone()?,
        };
        let index = self.open.len();
        self.open.push(fd);
        self.by_path.insert(parent.to_owned(), index);
        Ok(index)
    }

    fn get(&self, index: usize) -> Result<BorrowedFd<'_>> {
        self.open
            .get(index)
            .map(AsFd::as_fd)
            .ok_or_else(|| anyhow!("no open directory {index}"))
    }
}

fn walk_error(walked: &str, error: &io::Error) -> anyhow::Error {
    match error.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => {
            anyhow!("{walked} is a symlink or not a directory, and is never followed")
        }
        Some(libc::ENOENT) => {
            anyhow!("{walked} does not exist; system-write creates no directories")
        }
        _ => anyhow!("cannot open {walked}: {error}"),
    }
}

fn check(
    scope: &Scope,
    base_path: &Path,
    base: &OwnedFd,
    dirs: &mut Dirs,
    seen: &mut BTreeSet<String>,
    entry: &PlanEntry,
) -> Result<Ready> {
    let target = entry.target.as_str();
    let rel = rel(target)?;
    let expected = scope.target(target);
    ensure!(
        expected == target,
        "{target} is not in canonical form, which is {expected}"
    );
    let canonical = canonical_form(target, scope.macos());
    match scope.standing(target) {
        Standing::Active => {}
        Standing::Off => bail!("{target} is outside [system] roots"),
        Standing::Refused(why) => bail!("{why}"),
    }
    ensure!(
        !usable_roots(scope).contains(&canonical),
        "{target} is a root, not a file inside one"
    );
    ensure!(seen.insert(target.to_owned()), "{target} is listed twice");
    let bytes = unhex(&entry.bytes).ok_or_else(|| anyhow!("the bytes of {target} are not hex"))?;
    ensure!(
        hash_hex(&bytes) == entry.xxh3,
        "the xxh3 of {target} does not match its bytes"
    );
    let uid = sys::passwd_by_name(&entry.owner)
        .ok_or_else(|| anyhow!("{target}: no user is named {}", entry.owner))?
        .uid;
    let gid = sys::group_by_name(&entry.group)
        .ok_or_else(|| anyhow!("{target}: no group is named {}", entry.group))?
        .gid;
    let mode = parse_mode(&entry.mode).with_context(|| format!("the mode of {target}"))? & 0o777;

    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let dir = dirs.open(base, parent)?;
    let name = CString::new(name)?;
    match sys::kind_at(dirs.get(dir)?, &name).with_context(|| format!("cannot stat {target}"))? {
        None | Some(EntryKind::File) => {}
        Some(EntryKind::Symlink) => bail!("{target} is a symlink, which is never replaced"),
        Some(EntryKind::Other) => bail!("{target} is not a regular file"),
    }
    Ok(Ready {
        target: target.to_owned(),
        disk: base_path.join(rel),
        dir,
        name,
        bytes,
        uid,
        gid,
        mode,
    })
}

/// Temp file, owner, mode, fsync, rename, then fsync of the directory.
fn write(dirs: &Dirs, ready: &Ready, as_root: bool) -> Result<()> {
    let dir = dirs.get(ready.dir)?;
    let (temp, mut file) = create_temp(dir)?;
    let filled = fill(dir, &temp, &mut file, ready, as_root);
    if filled.is_err() {
        let _ = sys::unlinkat(dir, &temp);
    }
    filled.with_context(|| format!("cannot write {}", ready.target))
}

fn fill(
    dir: BorrowedFd<'_>,
    temp: &CString,
    file: &mut File,
    ready: &Ready,
    as_root: bool,
) -> io::Result<()> {
    file.write_all(&ready.bytes)?;
    if as_root {
        sys::fchown(file.as_fd(), ready.uid, ready.gid)?;
    }
    file.set_permissions(Permissions::from_mode(ready.mode))?;
    file.sync_all()?;
    sys::renameat(dir, temp, &ready.name)?;
    sys::fsync_dir(dir)
}

// No `rand` dependency: the name only has to be unused, and O_EXCL refuses one that is not.
fn create_temp(dir: BorrowedFd<'_>) -> Result<(CString, File)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    for attempt in 0_u32..16 {
        let seed = format!("{}-{nanos}-{attempt}", std::process::id());
        let name = CString::new(format!("{TEMP_PREFIX}{:016x}", xxh3_64(seed.as_bytes())))?;
        match sys::create_excl(dir, &name, 0o600) {
            Ok(file) => return Ok((name, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context("cannot create a temp file"),
        }
    }
    bail!("cannot find an unused temp file name")
}

fn relabel(disk: &Path) {
    if !Path::new(SELINUX_ENFORCE).exists() {
        return;
    }
    let Some(restorecon) = proc::find_program_in("restorecon", OsStr::new(RELABEL_PATH)) else {
        output::warning(&format!(
            "{}: SELinux is on, but restorecon is not in {RELABEL_PATH}; the file keeps the label it was created with",
            disk.display()
        ));
        return;
    };
    // stdout carries the result lines.
    let status = proc::command(&restorecon)
        .arg("-F")
        .arg(disk)
        .stdin(Stdio::null())
        .stdout(Stdio::from(io::stderr()))
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => output::warning(&format!(
            "restorecon -F {} failed ({status}); check its SELinux label",
            disk.display()
        )),
        Err(e) => output::warning(&format!("cannot run restorecon: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_failure_count_agrees_with_its_nouns_and_verb() {
        assert_eq!(not_written(1, 1), "1 of 1 file was not written");
        assert_eq!(not_written(1, 3), "1 of 3 files was not written");
        assert_eq!(not_written(2, 3), "2 of 3 files were not written");
    }

    #[test]
    fn a_prefix_never_reaches_root() {
        assert!(gate(0, Some("none"), true, false).is_err());
        assert!(gate(0, None, false, true).is_err());
        assert!(gate(0, Some("sudo"), false, false).is_ok());
        assert!(gate(0, Some("none"), false, false).is_ok());
    }

    #[test]
    fn a_user_runs_the_helper_only_under_escalate_none() {
        assert!(gate(1000, Some("sudo"), false, false).is_err());
        assert!(gate(1000, None, false, false).is_err());
        assert!(gate(1000, Some("none"), true, true).is_ok());
    }

    #[test]
    fn the_invoking_user_comes_from_the_escalator_then_the_plan_directory() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let unasked = || -> Result<u32> { bail!("the plan directory is not asked") };
        assert_eq!(
            invoking_user(1000, env(&[("SUDO_UID", "5")]), unasked).unwrap(),
            1000
        );
        assert_eq!(
            invoking_user(0, env(&[("SUDO_UID", "1001")]), unasked).unwrap(),
            1001
        );
        assert!(invoking_user(0, env(&[("SUDO_UID", "x")]), unasked).is_err());
        assert!(invoking_user(0, env(&[("DOAS_USER", "no-such-user-dotman")]), unasked).is_err());
        assert_eq!(
            invoking_user(0, env(&[("SUDO_UID", "")]), || Ok(1002)).unwrap(),
            1002
        );
        assert_eq!(invoking_user(0, env(&[]), || Ok(0)).unwrap(), 0);
    }

    #[test]
    fn hex_round_trips_and_rejects_anything_else() {
        let bytes = [0_u8, 1, 0x7f, 0xab, 0xff];
        assert_eq!(hex(&bytes), "00017fabff");
        assert_eq!(unhex("00017fabff").unwrap(), bytes);
        assert_eq!(unhex("").unwrap(), Vec::<u8>::new());
        for bad in ["0", "0g", "+f", " f", "abc"] {
            assert_eq!(unhex(bad), None, "{bad}");
        }
    }

    #[test]
    fn result_lines_parse_and_cannot_be_forged_by_a_newline() {
        let stdout =
            "written /etc/a\nskipped-owner /etc/b\nfailed /etc/c: no: really\nnot-written /etc/d\n";
        assert_eq!(reported(stdout, "/etc/a"), Some((Reported::Written, "")));
        assert_eq!(
            reported(stdout, "/etc/b"),
            Some((Reported::OwnerSkipped, ""))
        );
        assert_eq!(
            reported(stdout, "/etc/c"),
            Some((Reported::Failed, "no: really"))
        );
        assert_eq!(reported(stdout, "/etc/d"), Some((Reported::NotWritten, "")));
        assert_eq!(reported(stdout, "/etc"), None);
        let forged = format!("{FAILED} {}: x\n", shown("/etc/x\nwritten /etc/y"));
        assert_eq!(reported(&forged, "/etc/y"), None);
    }
}
