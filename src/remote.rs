//! Whether the remote is public.
//!
//! Its https form is asked anonymously, and the answer is kept in `state.toml` beside the URL it
//! was asked about. The check fails open, because a failure must never block a command or switch
//! the secret-name guard off.

use crate::context::Context;
use crate::git::redact_credentials;
use crate::output;
use crate::state::{RemotePublic, State};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the anonymous `ls-remote` may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

const RECHECK_AFTER_SECS: u64 = 86_400;

/// The https URL to ask about a remote; `None` for `file://`, local paths and anything else that
/// is not a network remote.
#[must_use]
pub fn https_form(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with('-') || url.chars().any(char::is_whitespace) {
        return None;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return scp_form(url);
    };
    let (authority, path) = rest.split_once('/')?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match scheme {
        "https" | "http" if !host.is_empty() && !path.is_empty() => {
            Some(format!("{scheme}://{host}/{path}"))
        }
        "ssh" => {
            let host = host_without_port(host);
            git_suffixed(path).map(|path| format!("https://{host}/{path}"))
        }
        _ => None,
    }
}

/// `[user@]host:path`, git's scp-like form. A `/` before the first `:` makes it a local path.
fn scp_form(url: &str) -> Option<String> {
    let (host, path) = url.split_once(':')?;
    if host.contains('/') {
        return None;
    }
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    if host.is_empty() || host.contains(['[', '@']) {
        return None;
    }
    git_suffixed(path).map(|path| format!("https://{host}/{path}"))
}

fn host_without_port(host: &str) -> &str {
    host.split_once(':').map_or(host, |(host, _)| host)
}

/// `path` without leading and trailing slashes, ending in `.git`; `None` when nothing is left.
fn git_suffixed(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(path.strip_suffix(".git").unwrap_or(path).to_owned() + ".git")
}

/// The form of `origin` that `state.toml` keeps: without credentials.
fn stored_form(origin: &str) -> String {
    redact_credentials(origin)
}

fn decide(prior: Option<RemotePublic>, same_url: bool, answered: bool) -> RemotePublic {
    if answered || (same_url && prior == Some(RemotePublic::True)) {
        RemotePublic::True
    } else {
        RemotePublic::Unknown
    }
}

/// Asks `probe` about the https form of `origin` and records the outcome in `state`.
///
/// Returns whether the remote answered anonymously. A failure keeps an earlier `true` for the same
/// URL; otherwise the result is `unknown`.
pub fn check(state: &mut State, origin: &str, now: u64, probe: impl FnOnce(&str) -> bool) -> bool {
    let key = stored_form(origin);
    let same_url = state.remote_url.as_deref() == Some(key.as_str());
    let answered = https_form(origin).is_some_and(|url| probe(&url));
    state.remote_public = Some(decide(state.remote_public, same_url, answered));
    state.remote_url = Some(key);
    state.remote_checked_at = Some(now);
    answered
}

/// Whether `state` holds no result for `origin`.
#[must_use]
pub fn needs_check(state: &State, origin: &str) -> bool {
    state.remote_url.as_deref() != Some(stored_form(origin).as_str())
}

/// Whether the stored result for `origin` is a day old or missing.
#[must_use]
pub fn is_due(state: &State, origin: &str, now: u64) -> bool {
    needs_check(state, origin)
        || state
            .remote_checked_at
            .is_none_or(|at| now.saturating_sub(at) >= RECHECK_AFTER_SECS)
}

/// Whether `state` says `origin` is public.
#[must_use]
pub fn is_public(state: &State, origin: &str) -> bool {
    state.remote_public == Some(RemotePublic::True) && !needs_check(state, origin)
}

/// The current time in unix seconds.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Checks `origin` over the network, warning when it is public.
pub fn refresh(ctx: &Context, state: &mut State, origin: &str) {
    let probe = |url: &str| ctx.git.ls_remote_anonymous(url, TIMEOUT).unwrap_or(false);
    if check(state, origin, now(), probe) {
        output::warning(&format!(
            "{} is public: it answers without credentials. `dot add` refuses files that match [guard] secret_names unless you pass --encrypt or --allow-plain",
            redact_credentials(origin)
        ));
    }
}

/// Checks the repo's `origin` and saves the result, when `state.toml` holds none for it, or with
/// `daily` when the result is a day old. Errors only reach `--verbose`.
pub fn note_origin(ctx: &Context, daily: bool) {
    if let Err(e) = note(ctx, daily) {
        output::verbose(&format!("remote visibility not recorded: {e:#}"));
    }
}

fn note(ctx: &Context, daily: bool) -> anyhow::Result<()> {
    let Some(origin) = ctx.git.remote_url("origin")? else {
        return Ok(());
    };
    let mut state = ctx.state(&ctx.repo_id()?)?;
    let due = if daily {
        is_due(&state, &origin, now())
    } else {
        needs_check(&state, &origin)
    };
    if due {
        refresh(ctx, &mut state, &origin);
        state.save(&ctx.paths.state_file(), false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn asked(url: &str) -> Option<String> {
        https_form(url)
    }

    #[test]
    fn scp_and_ssh_urls_map_to_https() {
        for url in [
            "git@github.com:me/dots.git",
            "git@github.com:me/dots",
            "github.com:me/dots.git",
            "ssh://git@github.com/me/dots.git",
            "ssh://git@github.com:2222/me/dots",
            "ssh://github.com/me/dots/",
        ] {
            assert_eq!(
                asked(url).as_deref(),
                Some("https://github.com/me/dots.git"),
                "{url}"
            );
        }
    }

    #[test]
    fn https_keeps_its_form_minus_the_userinfo() {
        assert_eq!(
            asked("https://github.com/me/dots").as_deref(),
            Some("https://github.com/me/dots")
        );
        assert_eq!(
            asked("https://me:s3cret@example.org:8443/a/b.git").as_deref(),
            Some("https://example.org:8443/a/b.git")
        );
        assert_eq!(
            asked("http://example.org/a.git").as_deref(),
            Some("http://example.org/a.git")
        );
    }

    #[test]
    fn what_is_not_a_network_remote_has_no_form() {
        for url in [
            "file:///srv/dots.git",
            "/srv/dots.git",
            "./dots.git",
            "../a:b/dots.git",
            "dots",
            "",
            "-oProxyCommand=x:y",
            "git://example.org/dots.git",
            "https://example.org",
            "ssh://example.org/",
            "git@:me/dots.git",
            "me:",
        ] {
            assert_eq!(asked(url), None, "{url}");
        }
    }

    fn fresh() -> State {
        State::default()
    }

    const URL: &str = "git@example.org:me/dots.git";

    #[test]
    fn an_answer_means_public_and_is_stored_with_the_url() {
        let mut state = fresh();
        let seen = Cell::new(None);
        let public = check(&mut state, URL, 100, |url| {
            seen.set(Some(url.to_owned()));
            true
        });
        assert!(public);
        assert_eq!(
            seen.take().as_deref(),
            Some("https://example.org/me/dots.git")
        );
        assert_eq!(state.remote_public, Some(RemotePublic::True));
        assert_eq!(state.remote_url.as_deref(), Some(URL));
        assert_eq!(state.remote_checked_at, Some(100));
        assert!(is_public(&state, URL));
    }

    #[test]
    fn a_failure_is_unknown_when_nothing_was_known() {
        let mut state = fresh();
        assert!(!check(&mut state, URL, 5, |_| false));
        assert_eq!(state.remote_public, Some(RemotePublic::Unknown));
        assert_eq!(state.remote_url.as_deref(), Some(URL));
        assert!(!is_public(&state, URL));
    }

    #[test]
    fn a_failure_after_true_keeps_true() {
        let mut state = fresh();
        check(&mut state, URL, 1, |_| true);
        assert!(!check(&mut state, URL, 2, |_| false));
        assert_eq!(state.remote_public, Some(RemotePublic::True));
        assert_eq!(state.remote_checked_at, Some(2));
        assert!(is_public(&state, URL));
    }

    #[test]
    fn a_new_url_resets_true_when_the_check_fails() {
        let mut state = fresh();
        check(&mut state, URL, 1, |_| true);
        let other = "git@example.org:me/other.git";
        assert!(!is_public(&state, other));
        check(&mut state, other, 2, |_| false);
        assert_eq!(state.remote_public, Some(RemotePublic::Unknown));
        assert_eq!(state.remote_url.as_deref(), Some(other));
    }

    #[test]
    fn a_local_remote_is_unknown_without_asking() {
        let mut state = fresh();
        let public = check(&mut state, "file:///srv/dots.git", 9, |_| {
            unreachable!("a file:// remote is never asked")
        });
        assert!(!public);
        assert_eq!(state.remote_public, Some(RemotePublic::Unknown));
        assert_eq!(state.remote_checked_at, Some(9));
    }

    #[test]
    fn credentials_never_reach_the_state() {
        let mut state = fresh();
        check(&mut state, "https://me:s3cret@example.org/a.git", 1, |_| {
            true
        });
        let stored = state.remote_url.unwrap_or_default();
        assert!(!stored.contains("s3cret"), "{stored}");
    }

    #[test]
    fn a_result_is_due_after_a_day_or_for_a_new_url() {
        let mut state = fresh();
        assert!(needs_check(&state, URL));
        check(&mut state, URL, 1_000, |_| false);
        assert!(!needs_check(&state, URL));
        assert!(!is_due(&state, URL, 1_000 + RECHECK_AFTER_SECS - 1));
        assert!(is_due(&state, URL, 1_000 + RECHECK_AFTER_SECS));
        assert!(is_due(&state, "git@example.org:me/else.git", 1_001));
    }

    #[test]
    fn the_probe_gets_ten_seconds() {
        assert_eq!(TIMEOUT, Duration::from_secs(10));
    }
}
