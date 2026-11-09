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
use crate::lock::Lock;
use crate::output;
use crate::paths::{create_private_dir, derive_key, private_temp_dir};
use crate::secrets::{Locked, key_hint};
use crate::state::Kind;
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
use std::time::SystemTime;

/// Runs `dotman restore`.
///
/// # Errors
///
/// Fails when the path is not managed, `--from` is neither a commit nor a date in an accepted form,
/// every commit is newer than that date, the file is absent from that commit, the lock is held, or
/// the file cannot be written.
pub fn run(args: &RestoreArgs) -> Result<()> {
    let ctx = Context::resolve("restore")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
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
    if !is_date(from) {
        bail!(
            "`{from}` is neither a commit nor a date. Give a revision such as HEAD~2 or a commit hash, or a date such as 2024-05-01, \"2024-05-01 13:30\", 2.days.ago or yesterday"
        );
    }
    ctx.git
        .rev_before(from)?
        .ok_or_else(|| anyhow!("every commit is newer than `{from}`"))
}

/// Whether `--from` names a date: `YYYY-MM-DD`, optionally followed by a space or `T` and `HH:MM`
/// or `HH:MM:SS`; `N.units.ago` or `N units ago`; or `yesterday`. git reads nearly any text as a
/// date, so a mistyped revision such as `mian` would otherwise pick some commit.
fn is_date(text: &str) -> bool {
    text == "yesterday" || is_calendar_date(text) || is_relative_date(text)
}

fn digits(part: &str, len: usize, range: RangeInclusive<u32>) -> bool {
    part.len() == len
        && part.bytes().all(|b| b.is_ascii_digit())
        && part.parse().is_ok_and(|n| range.contains(&n))
}

fn is_calendar_date(text: &str) -> bool {
    let (day, time) = text
        .split_once([' ', 'T'])
        .map_or((text, None), |(day, time)| (day, Some(time)));
    let day: Vec<&str> = day.split('-').collect();
    let day_ok = matches!(day.as_slice(), [y, m, d]
        if digits(y, 4, 1970..=9999) && digits(m, 2, 1..=12) && digits(d, 2, 1..=31));
    let time_ok = time.is_none_or(|time| {
        let parts: Vec<&str> = time.split(':').collect();
        match parts.as_slice() {
            [h, m] => digits(h, 2, 0..=23) && digits(m, 2, 0..=59),
            [h, m, s] => digits(h, 2, 0..=23) && digits(m, 2, 0..=59) && digits(s, 2, 0..=59),
            _ => false,
        }
    });
    day_ok && time_ok
}

fn is_relative_date(text: &str) -> bool {
    let words: Vec<&str> = text.split(['.', ' ']).collect();
    let [count, unit, "ago"] = words.as_slice() else {
        return false;
    };
    let unit = unit.strip_suffix('s').unwrap_or(unit);
    (1..=6).contains(&count.len())
        && count.bytes().all(|b| b.is_ascii_digit())
        && matches!(
            unit,
            "second" | "minute" | "hour" | "day" | "week" | "month" | "year"
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

    #[test]
    fn only_strict_date_forms_are_dates() {
        for date in [
            "2024-05-01",
            "2024-05-01 13:30",
            "2024-05-01T13:30:59",
            "2.days.ago",
            "1 day ago",
            "10.minutes.ago",
            "3 weeks ago",
            "1.year.ago",
            "yesterday",
        ] {
            assert!(is_date(date), "{date}");
        }
        for typo in [
            "heaad~2",
            "mian",
            "yesterdy",
            "nonsense-rev",
            "HEAD~x",
            "2024-5-1",
            "2024-13-01",
            "2024-05-32",
            "2024-05-01 24:00",
            "2024-05-01 13:60",
            "2024-05-01 13",
            "2024-05-01  13:30",
            "2.days",
            "2.dais.ago",
            "two.days.ago",
            "2 days  ago",
            "1234567.days.ago",
            "last week",
            "",
        ] {
            assert!(!is_date(typo), "{typo}");
        }
    }
}
