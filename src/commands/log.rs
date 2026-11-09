//! `dotman log`: one line per commit.

use crate::cli::LogArgs;
use crate::context::Context;
use crate::output;
use crate::paths::derive_key;
use anyhow::{Context as _, Result};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs `dotman log`. Takes no lock and writes nothing.
///
/// # Errors
///
/// Fails when the repo is not initialised or has a rebase or merge in progress, git is too old or
/// exits non-zero, or stdout cannot be written.
pub fn run(args: &LogArgs) -> Result<()> {
    let ctx = Context::resolve("log")?;
    ctx.guard()?;
    ctx.repo_id()?;
    ctx.git.check_version()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let mut specs = Vec::new();
    for input in &args.paths {
        specs.extend(pathspecs(input, &cwd, &ctx)?);
    }
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
    let entries = ctx.git.log(&specs, args.max_count)?;
    if entries.is_empty() && args.max_count != Some(0) {
        output::info(&if args.paths.is_empty() {
            "the repo has no commits yet".to_owned()
        } else {
            format!("no commit touches {}", args.paths.join(" "))
        });
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let mut out = std::io::stdout().lock();
    for entry in &entries {
        let short = entry.oid.get(..8).unwrap_or(&entry.oid);
        // A subject comes from whoever pushed the commit, so it could hold escape sequences.
        writeln!(
            out,
            "{short}  {:<14} {}",
            relative_age(now, entry.time),
            output::escape_line(&entry.subject)
        )
        .context("cannot write to stdout")?;
    }
    if ctx.git.is_shallow()? {
        output::info(&format!(
            "history is shallow; run `{} git fetch --unshallow` for the full log",
            crate::name::bin()
        ));
    }
    Ok(())
}

/// A path under `$HOME` becomes its key; anything else is taken as a pathspec glob already
/// relative to the repo. A key matches the path itself and, as a directory, everything below it.
fn pathspecs(input: &str, cwd: &Path, ctx: &Context) -> Result<Vec<String>> {
    let home = &ctx.paths.home;
    let home_real = ctx.paths.home_real.as_deref();
    let is_fs_path = input.starts_with('~') || Path::new(input).is_absolute() || {
        // A relative input is a filesystem path only when cwd is inside $HOME.
        cwd.starts_with(home) || home_real.is_some_and(|real| cwd.starts_with(real))
    };
    let key = if is_fs_path {
        derive_key(Path::new(input), cwd, home, home_real)?
    } else {
        input.trim_end_matches('/').to_owned()
    };
    Ok(if input.ends_with('/') {
        vec![format!(":(glob){key}/**")]
    } else {
        vec![format!(":(glob){key}"), format!(":(glob){key}/**")]
    })
}

fn relative_age(now: i64, then: i64) -> String {
    const UNITS: [(i64, &str); 6] = [
        (31_536_000, "year"),
        (2_592_000, "month"),
        (604_800, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
    ];
    let secs = now.saturating_sub(then).max(0);
    let (count, unit) = UNITS
        .iter()
        .find(|(size, _)| secs >= *size)
        .map_or((secs, "second"), |(size, name)| {
            (secs.checked_div(*size).unwrap_or(0), name)
        });
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

#[cfg(test)]
mod tests {
    use super::relative_age;

    #[test]
    fn ages_use_the_largest_whole_unit() {
        assert_eq!(relative_age(100, 100), "0 seconds ago");
        assert_eq!(relative_age(10_000, 10_000 - 3 * 3_600), "3 hours ago");
        assert_eq!(relative_age(200_000, 200_000 - 86_400), "1 day ago");
        assert_eq!(relative_age(5, 10), "0 seconds ago");
    }
}
