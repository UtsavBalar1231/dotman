//! `dotman restore`: write an earlier version of a managed file into `$HOME`.
//!
//! Only `$HOME` changes. The repo and the state stay as they were, so the next `status` shows the
//! file as changed locally and `sync` records it as a new commit. A template's source is restored
//! in the repo as well, and what `$HOME` receives is that source rendered. An encrypted source
//! stays as it is in the repo, and `$HOME` receives the old ciphertext decrypted.

use crate::cli::RestoreArgs;
use crate::context::Context;
use crate::engine::write::{WriteOptions, WriteOutcome, backup_run_dir, safe_symlink, safe_write};
use crate::engine::{Home, Tuple, hash_hex, observe, observe_portable, recorded_mode};
use crate::name;
use crate::output;
use crate::paths::{create_private_dir, derive_key, private_temp_dir};
use crate::secrets::{Locked, key_hint};
use crate::state::Kind;
use crate::sys;
use crate::template;
use crate::variant::Flag;
use anyhow::{Context as _, Result, anyhow, bail};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::ops::RangeInclusive;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs `dotman restore`.
///
/// # Errors
///
/// Fails when the path is not managed, `--from` is neither a commit nor a date in an accepted form,
/// every commit is newer than that date, the repo cannot read the commit, the file is absent from
/// that commit, the lock is held, or the file cannot be written.
pub fn run(args: &RestoreArgs) -> Result<()> {
    let ctx = Context::resolve("restore")?;
    let dry_run = args.dry_run;
    let _lock = ctx.lock(dry_run)?;
    ctx.repo_id()?;
    ctx.git.check_version()?;
    let repo_config = ctx.repo_config()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let home = &ctx.paths.home;
    let key = derive_key(&args.path, &cwd, home, ctx.paths.home_real.as_deref())?;
    let sources = ctx.sources()?;
    if !sources.contains(&key) {
        bail!("{key} is not managed");
    }
    let source = sources.select(&key, ctx.identity()).require(&key)?;
    let flag = sources.flag(&key, source);
    let template = flag == Flag::Tmpl;

    let rev = resolve(&ctx, args.from.as_deref().unwrap_or("HEAD"))?;
    let short = rev.get(..8).unwrap_or(&rev);
    let git_mode = ctx
        .git
        .tree_mode(&rev, source)?
        .ok_or_else(|| anyhow!("{source} does not exist in {short}"))?;
    let blob = ctx.git.show_blob(&rev, source)?;

    let default_mode = match flag {
        Flag::Age => 0o600,
        _ if git_mode == "100755" => 0o755,
        _ => 0o644,
    };
    let mode = recorded_mode(&repo_config.modes, source)?.unwrap_or(default_mode);
    let link = (git_mode == "120000").then(|| Path::new(OsStr::from_bytes(&blob)));
    if link.is_some() && flag != Flag::Plain {
        bail!("{source} in {short} is a symlink, which cannot be rendered or decrypted");
    }
    // Rendering and decrypting can fail, and that must happen before anything is touched.
    let converted = convert(&ctx, flag, source, short, &blob, dry_run)?;
    let bytes = converted.as_deref().unwrap_or(&blob);
    let wanted = link.map_or_else(|| Tuple::of_file(bytes, mode), Tuple::of_link);

    let source_now = observe(&ctx.paths.repo, source)?;
    let restore_source = template
        && !matches!(&source_now, Home::Present(t) if t.kind == Kind::File && t.hash == hash_hex(&blob));
    let expected = observe_portable(home, ctx.paths.home_real.as_deref(), &key)?;
    if !restore_source && matches!(&expected, Home::Present(h) if *h == wanted) {
        output::info(&format!("{key}: already matches {short}"));
        return Ok(());
    }
    let backups = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
    if restore_source {
        let opts = WriteOptions {
            dry_run,
            in_place: false,
            backup: Some(&backups),
            expected: &source_now,
            modes: &repo_config.modes,
        };
        let repo_mode = if git_mode == "100755" { 0o755 } else { 0o644 };
        match safe_write(&ctx.paths.repo, source, &blob, repo_mode, &opts)? {
            WriteOutcome::Written(_) => {
                let verb = if dry_run { "would restore" } else { "restore" };
                output::action(verb, &format!("{source} from {short}"));
            }
            WriteOutcome::Skipped(skip) => bail!("{source}: {skip}"),
        }
    }
    let opts = WriteOptions {
        dry_run,
        in_place: false,
        backup: Some(&backups),
        expected: &expected,
        modes: &repo_config.modes,
    };
    let outcome = match link {
        Some(target) => safe_symlink(home, &key, target, &opts)?,
        None => safe_write(home, &key, bytes, mode, &opts)?,
    };
    match outcome {
        WriteOutcome::Written(written) => {
            let verb = if dry_run { "would restore" } else { "restore" };
            output::action(verb, &format!("{key} from {short}"));
            if let Some(backup) = &written.backup {
                let verb = if dry_run {
                    "would back up to"
                } else {
                    "backed up to"
                };
                output::info(&format!("  {verb} {}", backup.display()));
            }
            if let Some(warning) = written.warning() {
                output::warning(&format!("{key}: {warning}"));
            }
            Ok(())
        }
        WriteOutcome::Skipped(skip) => bail!("{key}: {skip}"),
    }
}

/// The commit `from` names, or the newest one before the date it names.
fn resolve(ctx: &Context, from: &str) -> Result<String> {
    if let Some(rev) = ctx.git.resolve_commit(from)? {
        return Ok(rev);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let secs = match parse_date(from, now) {
        Some(When::Local([year, month, day, hour, minute, second])) => {
            sys::local_epoch(year, month, day, hour, minute, second)
        }
        Some(When::At(secs)) => Some(secs),
        Some(When::BeforeEpoch) => None,
        None => {
            let base = from.split(['~', '^']).next().unwrap_or(from);
            if base != from && !base.is_empty() && ctx.git.resolve_commit(base)?.is_some() {
                bail!("the revision `{from}` does not exist: {base} has no such ancestor");
            }
            if let Some(damage) = unreadable(ctx, base) {
                return Err(damage);
            }
            bail!(
                "`{from}` is neither a commit nor a date. Give a revision such as HEAD~2 or a commit hash, or a date such as 2024-05-01, \"2024-05-01 13:30\", 2.days.ago or yesterday"
            )
        }
    };
    let rev = match secs {
        Some(secs) => ctx.git.rev_before(&format!("@{secs} +0000"))?,
        None => None,
    };
    rev.ok_or_else(|| anyhow!("every commit is newer than `{from}`"))
}

/// When `rev` resolves through the refs but git cannot read the object it names, as after a pack
/// is truncated or deleted, an error that says the repo is damaged and keeps git's own line. A
/// full object id is left out: one that was never in this repo resolves the same way.
fn unreadable(ctx: &Context, rev: &str) -> Option<anyhow::Error> {
    if rev.is_empty() || rev.starts_with('-') {
        return None;
    }
    let git = |args: &[&str]| {
        let mut cmd = ctx.git.command();
        cmd.args(args).stdin(Stdio::null());
        cmd.output().ok()
    };
    let named = git(&["rev-parse", "--verify", "--quiet", rev])?;
    let oid = String::from_utf8_lossy(&named.stdout).trim().to_owned();
    if !named.status.success() || oid.is_empty() || oid.eq_ignore_ascii_case(rev) {
        return None;
    }
    let read = git(&["cat-file", "-t", &oid])?;
    if read.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&read.stderr);
    let mut lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let line = lines
        .clone()
        .find(|line| line.starts_with("error:"))
        .or_else(|| lines.next())
        .unwrap_or("no message");
    let short = oid.get(..8).unwrap_or(&oid);
    Some(
        anyhow!("git cat-file -t {oid} failed: {line}").context(format!(
            "the repo is damaged: {rev} names object {short}, which git cannot read; `{} doctor` checks the repo",
            name::bin()
        )),
    )
}

/// The time a `--from` date names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    /// A wall-clock time in the local time zone: year, month, day, hour, minute and second.
    Local([u32; 6]),
    /// Unix seconds.
    At(u64),
    /// A span that reaches back past 1970, before any commit.
    BeforeEpoch,
}

/// `text` as a date: `YYYY-MM-DD` in local time, optionally followed by a space or `T` and
/// `HH:MM` or `HH:MM:SS`; `N.units.ago` or `N units ago`, where a month is 30 days and a year 365;
/// or `yesterday`. `None` for anything else. git gets the result as seconds, never the text: its
/// approxidate reads nearly any text as some date, and misreads some of these forms.
fn parse_date(text: &str, now: u64) -> Option<When> {
    calendar(text).map(When::Local).or_else(|| ago(text, now))
}

fn number(part: &str, len: usize, range: RangeInclusive<u32>) -> Option<u32> {
    if part.len() != len || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok().filter(|n| range.contains(n))
}

const fn leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn calendar(text: &str) -> Option<[u32; 6]> {
    let (date, time) = text
        .split_once([' ', 'T'])
        .map_or((text, None), |(date, time)| (date, Some(time)));
    let date: Vec<&str> = date.split('-').collect();
    let [year, month, day] = date.as_slice() else {
        return None;
    };
    let year = number(year, 4, 1970..=9999)?;
    let month = number(month, 2, 1..=12)?;
    let day = number(day, 2, 1..=days_in_month(year, month))?;
    let (hour, minute, second) = match time {
        None => (0, 0, 0),
        Some(time) => {
            let parts: Vec<&str> = time.split(':').collect();
            let (hour, minute, second) = match parts.as_slice() {
                [hour, minute] => (hour, minute, &"00"),
                [hour, minute, second] => (hour, minute, second),
                _ => return None,
            };
            (
                number(hour, 2, 0..=23)?,
                number(minute, 2, 0..=59)?,
                number(second, 2, 0..=59)?,
            )
        }
    };
    Some([year, month, day, hour, minute, second])
}

fn ago(text: &str, now: u64) -> Option<When> {
    let (count, unit) = if text == "yesterday" {
        ("1", "day")
    } else {
        let words: Vec<&str> = text.split(['.', ' ']).collect();
        let [count, unit, "ago"] = words.as_slice() else {
            return None;
        };
        (*count, unit.strip_suffix('s').unwrap_or(unit))
    };
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let unit: u64 = match unit {
        "second" => 1,
        "minute" => 60,
        "hour" => 3_600,
        "day" => 86_400,
        "week" => 604_800,
        "month" => 2_592_000,
        "year" => 31_536_000,
        _ => return None,
    };
    // A count too large for u64 still names a time before 1970.
    let span = count.parse::<u64>().ok().and_then(|n| n.checked_mul(unit));
    Some(
        span.and_then(|span| now.checked_sub(span))
            .map_or(When::BeforeEpoch, When::At),
    )
}

/// What `$HOME` receives for `blob`, the source in revision `short`; `None` when that is `blob`.
fn convert(
    ctx: &Context,
    flag: Flag,
    source: &str,
    short: &str,
    blob: &[u8],
    dry_run: bool,
) -> Result<Option<Vec<u8>>> {
    Ok(match flag {
        Flag::Plain => None,
        Flag::Tmpl => {
            let text = std::str::from_utf8(blob)
                .with_context(|| format!("{source} in {short} is not valid UTF-8"))?;
            Some(template::render(source, text, ctx.template_vars())?.into_bytes())
        }
        Flag::Age => Some(decrypt(ctx, blob, dry_run).map_err(
            |e| match e.downcast::<Locked>() {
                Ok(Locked(why)) => anyhow!(
                    "{source} in {short} is encrypted and cannot be decrypted here ({why}); {}",
                    key_hint()
                ),
                Err(e) => e.context(format!("cannot decrypt {source} in {short}")),
            },
        )?),
    })
}

// age reads the ciphertext from a file, because stdin may carry the key. The ciphertext is not
// secret; the plaintext comes back on a pipe and is never written here. A dry run creates nothing
// in the state dir, so its temp dir goes in the system one.
fn decrypt(ctx: &Context, ciphertext: &[u8], dry_run: bool) -> Result<Vec<u8>> {
    let base = if dry_run {
        std::env::temp_dir()
    } else {
        let state = &ctx.paths.state_dir;
        create_private_dir(state, false)
            .with_context(|| format!("cannot create {}", state.display()))?;
        state.clone()
    };
    let tmp = private_temp_dir("dotman-restore-", &base)?;
    let path = tmp.path().join("ciphertext.age");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(ciphertext)?;
    drop(file);
    ctx.age().decrypt(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn calendar_dates_are_local_wall_clock_times_with_real_month_lengths() {
        for (date, fields) in [
            ("1970-01-01", [1970, 1, 1, 0, 0, 0]),
            ("2024-05-01 13:30", [2024, 5, 1, 13, 30, 0]),
            ("2024-05-01T13:30:59", [2024, 5, 1, 13, 30, 59]),
            ("2024-02-29", [2024, 2, 29, 0, 0, 0]),
            ("2000-02-29", [2000, 2, 29, 0, 0, 0]),
            ("3000-01-01", [3000, 1, 1, 0, 0, 0]),
            ("9999-12-31T23:59:59", [9999, 12, 31, 23, 59, 59]),
        ] {
            assert_eq!(parse_date(date, NOW), Some(When::Local(fields)), "{date}");
        }
        for bad in [
            "2025-09-31",
            "2023-02-29",
            "2100-02-29",
            "1969-12-31",
            "2024-5-1",
            "2024-13-01",
            "2024-05-32",
            "2024-05-00",
            "2024-05-01 24:00",
            "2024-05-01 13:60",
            "2024-05-01 13",
            "2024-05-01  13:30",
        ] {
            assert_eq!(parse_date(bad, NOW), None, "{bad}");
        }
    }

    #[test]
    fn a_relative_span_counts_back_from_now_and_past_1970_is_before_any_commit() {
        for (date, back) in [
            ("2.days.ago", 172_800),
            ("1 day ago", 86_400),
            ("yesterday", 86_400),
            ("10.minutes.ago", 600),
            ("3 weeks ago", 1_814_400),
            ("1.month.ago", 2_592_000),
            ("1.year.ago", 31_536_000),
        ] {
            let at = NOW.checked_sub(back).map(When::At);
            assert_eq!(parse_date(date, NOW), at, "{date}");
        }
        for far in [
            "58.years.ago",
            "1234567.days.ago",
            "99999999999999999999.days.ago",
        ] {
            assert_eq!(parse_date(far, NOW), Some(When::BeforeEpoch), "{far}");
        }
    }

    #[test]
    fn typos_are_not_dates() {
        for typo in [
            "heaad~2",
            "mian",
            "yesterdy",
            "nonsense-rev",
            "HEAD~x",
            "2.days",
            "2.dais.ago",
            "two.days.ago",
            "2 days  ago",
            "-2.days.ago",
            "last week",
            "",
        ] {
            assert_eq!(parse_date(typo, NOW), None, "{typo}");
        }
    }
}
