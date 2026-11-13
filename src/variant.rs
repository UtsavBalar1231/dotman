//! Brace-block variants. `monitors{host=laptop}.conf` is a variant of `monitors.conf`, and each
//! machine applies the most specific source whose conditions all hold there.
//!
//! Brace text that does not parse as a block stays part of the file name, because `$HOME` holds
//! names such as `{ca546566-e370-4cde-98d3-acc110c5967d}.xpi`.

use crate::paths::validate_key;
use crate::state::IdentitySnapshot;
use anyhow::{Result, bail};
use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;

const ONE_FLAG: &str = "a block takes at most one of `tmpl` and `age`";
const ONE_BLOCK: &str = "a name takes at most one block";
const BAD_TARGET: &str = "the name without its block is not a path dotman can manage";

// Declared most specific first; the derived order is the selection rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Host,
    User,
    Profile,
    Distro,
    Like,
    Os,
    Arch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Condition {
    key: Key,
    negated: bool,
    /// Lowercased, as identity values are.
    value: String,
}

impl Condition {
    fn holds(&self, identity: &IdentitySnapshot) -> bool {
        let value = self.value.as_str();
        let matches = match self.key {
            Key::Host => identity.host == value,
            Key::User => identity.user == value,
            Key::Profile => identity.profiles.iter().any(|p| p == value),
            Key::Distro => identity.distro == value,
            Key::Like => identity.like.split_whitespace().any(|t| t == value),
            // WSL counts as Linux, so `os=linux` holds there as well as `os=wsl`.
            Key::Os => identity.os == value || (identity.os == "wsl" && value == "linux"),
            Key::Arch => identity.arch == value,
        };
        matches != self.negated
    }
}

// Positive conditions, most specific first, then whether the candidate has any condition at all.
// The tuple's ordering is the selection rule: the lists are compared element by element, a list
// that extends another wins, and on equal lists a candidate with conditions beats one without, so
// a negated-only variant beats the plain fallback. The element flag makes `os=wsl` outrank
// `os=linux`; off WSL `os=wsl` never holds, so it never competes.
type Rank = (Vec<(Reverse<Key>, bool)>, bool);

fn rank(conditions: &[Condition], identity: &IdentitySnapshot) -> Option<Rank> {
    if !conditions.iter().all(|c| c.holds(identity)) {
        return None;
    }
    let mut positive: Vec<(Reverse<Key>, bool)> = conditions
        .iter()
        .filter(|c| !c.negated)
        .map(|c| (Reverse(c.key), c.key == Key::Os && c.value == "wsl"))
        .collect();
    positive.sort_unstable_by(|a, b| b.cmp(a));
    Some((positive, !conditions.is_empty()))
}

/// What a source's block says to do with its bytes before they are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// Applied as they are.
    Plain,
    /// Rendered as a template.
    Tmpl,
    /// Decrypted.
    Age,
}

#[derive(Debug, PartialEq, Eq)]
enum Name {
    Literal,
    Variant {
        target: String,
        conditions: Vec<Condition>,
        flag: Flag,
    },
    Invalid {
        target: String,
        reason: &'static str,
    },
}

fn parse_name(name: &str) -> Name {
    let mut blocks = Vec::new();
    for (open, _) in name.match_indices('{') {
        let (Some(head), Some(after)) = (
            name.get(..open),
            name.get(open..).and_then(|s| s.strip_prefix('{')),
        ) else {
            continue;
        };
        // No `}` after this `{` means none after any later one either.
        let Some((inner, tail)) = after.split_once('}') else {
            break;
        };
        if let Some(parsed) = parse_block(inner) {
            blocks.push((format!("{head}{tail}"), parsed));
        }
    }
    if blocks.len() > 1 {
        return Name::Invalid {
            target: name.to_owned(),
            reason: ONE_BLOCK,
        };
    }
    match blocks.pop() {
        None => Name::Literal,
        Some((target, Ok((conditions, flag)))) => Name::Variant {
            target,
            conditions,
            flag,
        },
        Some((target, Err(reason))) => Name::Invalid { target, reason },
    }
}

// `None` when `inner` is not a block, so its braces are part of the name.
fn parse_block(inner: &str) -> Option<Result<(Vec<Condition>, Flag), &'static str>> {
    let mut conditions = Vec::new();
    let mut flag = Flag::Plain;
    let mut flags = 0usize;
    for term in inner.split('+') {
        match term {
            "tmpl" | "age" => {
                flags = flags.saturating_add(1);
                flag = if term == "tmpl" {
                    Flag::Tmpl
                } else {
                    Flag::Age
                };
            }
            _ => conditions.push(parse_condition(term)?),
        }
    }
    Some(if flags > 1 {
        Err(ONE_FLAG)
    } else {
        Ok((conditions, flag))
    })
}

fn parse_condition(term: &str) -> Option<Condition> {
    let (lhs, value) = term.split_once('=')?;
    let (name, negated) = lhs
        .strip_suffix('!')
        .map_or((lhs, false), |name| (name, true));
    let key = match name {
        "host" => Key::Host,
        "user" => Key::User,
        "profile" => Key::Profile,
        "distro" => Key::Distro,
        "like" => Key::Like,
        "os" => Key::Os,
        "arch" => Key::Arch,
        _ => return None,
    };
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    valid.then(|| Condition {
        key,
        negated,
        value: value.to_ascii_lowercase(),
    })
}

/// Whether a file name carries no block. A name that does would be read back as a variant, so
/// dotman cannot manage a `$HOME` file under it.
#[must_use]
pub fn is_literal(name: &str) -> bool {
    parse_name(name) == Name::Literal
}

/// Whether `source`'s block holds `host=<host>`, so that no other machine applies it.
#[must_use]
pub fn names_host(source: &str, host: &str) -> bool {
    let (_, name) = split_name(source);
    matches!(parse_name(name), Name::Variant { conditions, .. }
        if conditions.iter().any(|c| c.key == Key::Host && !c.negated && c.value == host))
}

/// The source path that makes `key` a variant for `conditions`, such as `host=laptop+os=linux`.
/// The block goes before the last `.` of the name, unless that dot is the name's first character.
pub fn variant_source(key: &str, conditions: &str) -> Result<String> {
    let only_conditions = matches!(parse_block(conditions), Some(Ok((parsed, _)))
        if parsed.len() == conditions.split('+').count());
    if !only_conditions {
        bail!("`{conditions}` is not a list of conditions such as `host=laptop+os=linux`");
    }
    Ok(insert_block(key, conditions))
}

/// `key` with `block`, a `+`-joined list of conditions and at most one flag such as
/// `os=linux+tmpl`, placed where `--for` puts it.
pub fn with_block(key: &str, block: &str) -> Result<String> {
    if !matches!(parse_block(block), Some(Ok(_))) {
        bail!("`{block}` is not a block such as `host=laptop+os=linux+tmpl`");
    }
    Ok(insert_block(key, block))
}

/// `source` with the `age` flag: `a{host=x}.conf` gives `a{host=x+age}.conf`, and a name with no
/// block gets `{age}` where `--for` would put its block.
pub fn with_age(source: &str) -> Result<String> {
    with_flag(source, Flag::Age)
}

/// `source` with the `tmpl` flag, placed as [`with_age`] places `age`.
pub fn with_tmpl(source: &str) -> Result<String> {
    with_flag(source, Flag::Tmpl)
}

fn with_flag(source: &str, wanted: Flag) -> Result<String> {
    let (term, clash) = match wanted {
        Flag::Age => ("age", "a template, which cannot be encrypted"),
        _ => ("tmpl", "encrypted, which cannot be a template"),
    };
    let (dir, name) = split_name(source);
    for (open, _) in name.match_indices('{') {
        let (Some(head), Some(after)) = (
            name.get(..open),
            name.get(open..).and_then(|s| s.strip_prefix('{')),
        ) else {
            continue;
        };
        let Some((inner, tail)) = after.split_once('}') else {
            break;
        };
        match parse_block(inner) {
            None => {}
            Some(Ok((_, flag))) if flag == wanted => return Ok(source.to_owned()),
            Some(Ok((_, Flag::Plain))) => {
                return Ok(join_name(dir, &format!("{head}{{{inner}+{term}}}{tail}")));
            }
            Some(Ok(_)) => bail!("{source} is {clash}"),
            Some(Err(reason)) => bail!("{source}: {reason}"),
        }
    }
    Ok(insert_block(source, term))
}

fn split_name(path: &str) -> (Option<&str>, &str) {
    path.rsplit_once('/')
        .map_or((None, path), |(dir, name)| (Some(dir), name))
}

fn join_name(dir: Option<&str>, name: &str) -> String {
    dir.map_or_else(|| name.to_owned(), |dir| format!("{dir}/{name}"))
}

// Before the last `.` of the name, unless that dot is its first character.
fn insert_block(key: &str, block: &str) -> String {
    let (dir, name) = split_name(key);
    let named = match name
        .rfind('.')
        .filter(|dot| *dot > 0)
        .and_then(|dot| name.split_at_checked(dot))
    {
        Some((stem, ext)) => format!("{stem}{{{block}}}{ext}"),
        None => format!("{name}{{{block}}}"),
    };
    join_name(dir, &named)
}

/// What a key resolves to on one machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection<T> {
    /// No source applies here.
    None,
    /// The most specific source whose conditions all hold.
    Winner(T),
    /// The equally specific sources at the top; none is applied.
    Ambiguous(Vec<String>),
    /// Sources whose block parses but cannot be used, with the reason. None of the key's sources
    /// is applied, since the broken one may have been meant to win.
    Invalid(Vec<(String, &'static str)>),
}

impl<'a> Selection<&'a str> {
    /// The winner, or an error saying why `key` has none.
    pub fn require(self, key: &str) -> Result<&'a str> {
        match self {
            Self::Winner(source) => Ok(source),
            Self::None => bail!("no source of {key} applies on this machine"),
            Self::Ambiguous(tied) => bail!(
                "{} are equally specific here; remove one first",
                tied.join(", ")
            ),
            Self::Invalid(problems) => bail!(
                "{}",
                problems
                    .iter()
                    .map(|(source, reason)| format!("{source}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

impl<T> Selection<T> {
    /// `None` for an ambiguous or invalid key as well as for one that no source applies to.
    pub const fn winner(&self) -> Option<&T> {
        match self {
            Self::Winner(winner) => Some(winner),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Form {
    Plain,
    Variant(Vec<Condition>, Flag),
    Invalid(&'static str),
}

/// Repo paths grouped by the key each one targets.
#[derive(Debug, Default)]
pub struct Sources {
    by_key: BTreeMap<String, Vec<(String, Form)>>,
}

impl Sources {
    /// Groups repo-relative paths. Only the final component may carry a block; a block whose
    /// target is not a usable key, such as `.git{os=linux}`, is invalid and keyed by its own path.
    pub fn group<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut by_key: BTreeMap<String, Vec<(String, Form)>> = BTreeMap::new();
        for path in paths {
            let (dir, name) = path
                .rsplit_once('/')
                .map_or((None, path), |(dir, name)| (Some(dir), name));
            let join = |target: &str| {
                let key = dir.map_or_else(|| target.to_owned(), |dir| format!("{dir}/{target}"));
                validate_key(&key).is_ok().then_some(key)
            };
            let (key, form) = match parse_name(name) {
                Name::Literal => (path.to_owned(), Form::Plain),
                Name::Variant {
                    target,
                    conditions,
                    flag,
                } => join(&target).map_or_else(
                    || (path.to_owned(), Form::Invalid(BAD_TARGET)),
                    |key| (key, Form::Variant(conditions, flag)),
                ),
                Name::Invalid { target, reason } => (
                    join(&target).unwrap_or_else(|| path.to_owned()),
                    Form::Invalid(reason),
                ),
            };
            by_key.entry(key).or_default().push((path.to_owned(), form));
        }
        Self { by_key }
    }

    /// Sorted. A key is listed even when none of its sources applies to this machine.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.by_key.keys().map(String::as_str)
    }

    /// True even when none of `key`'s sources applies to this machine.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }

    /// The plain file and every variant of `key`, invalid ones included.
    pub fn sources_of(&self, key: &str) -> impl Iterator<Item = &str> {
        self.by_key
            .get(key)
            .into_iter()
            .flatten()
            .map(|(source, _)| source.as_str())
    }

    /// What `source`, one of `key`'s paths, asks to be done with its bytes.
    #[must_use]
    pub fn flag(&self, key: &str, source: &str) -> Flag {
        self.by_key
            .get(key)
            .into_iter()
            .flatten()
            .find_map(|(path, form)| match form {
                Form::Variant(_, flag) if path == source => Some(*flag),
                _ => None,
            })
            .unwrap_or(Flag::Plain)
    }

    /// Picks `key`'s source for `identity`. The plain file is a candidate with no conditions,
    /// and negated conditions rank only against candidates whose positive conditions are the same.
    #[must_use]
    pub fn select(&self, key: &str, identity: &IdentitySnapshot) -> Selection<&str> {
        let Some(candidates) = self.by_key.get(key) else {
            return Selection::None;
        };
        let invalid: Vec<(String, &'static str)> = candidates
            .iter()
            .filter_map(|(source, form)| match form {
                Form::Invalid(reason) => Some((source.clone(), *reason)),
                Form::Plain | Form::Variant(..) => None,
            })
            .collect();
        if !invalid.is_empty() {
            return Selection::Invalid(invalid);
        }
        let mut best: Option<Rank> = None;
        let mut top: Vec<&str> = Vec::new();
        for (source, form) in candidates {
            let rank = match form {
                Form::Plain => Some(Rank::default()),
                Form::Variant(conditions, _) => rank(conditions, identity),
                Form::Invalid(_) => None,
            };
            let Some(rank) = rank else { continue };
            match best.as_ref().map(|b| rank.cmp(b)) {
                Some(Ordering::Less) => {}
                Some(Ordering::Equal) => top.push(source),
                None | Some(Ordering::Greater) => {
                    best = Some(rank);
                    top = vec![source];
                }
            }
        }
        match top.as_slice() {
            [] => Selection::None,
            [one] => Selection::Winner(*one),
            _ => Selection::Ambiguous(top.iter().map(|s| (*s).to_owned()).collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn machine(host: &str, os: &str) -> IdentitySnapshot {
        IdentitySnapshot {
            host: host.to_owned(),
            user: "u".to_owned(),
            os: os.to_owned(),
            distro: "arch".to_owned(),
            like: "archlinux manjaro".to_owned(),
            arch: "x86_64".to_owned(),
            profiles: vec!["work".to_owned(), "home".to_owned()],
        }
    }

    fn pick(paths: &[&str], identity: &IdentitySnapshot) -> Selection<String> {
        let sources = Sources::group(paths.iter().copied());
        let keys: Vec<&str> = sources.keys().collect();
        let [key] = keys.as_slice() else {
            unreachable!("{paths:?} target more than one key: {keys:?}");
        };
        match sources.select(key, identity) {
            Selection::None => Selection::None,
            Selection::Winner(w) => Selection::Winner(w.to_owned()),
            Selection::Ambiguous(a) => Selection::Ambiguous(a),
            Selection::Invalid(i) => Selection::Invalid(i),
        }
    }

    fn winner(source: &str) -> Selection<String> {
        Selection::Winner(source.to_owned())
    }

    fn target(name: &str) -> Option<String> {
        match parse_name(name) {
            Name::Variant { target, .. } => Some(target),
            Name::Literal | Name::Invalid { .. } => None,
        }
    }

    #[test]
    fn brace_names_that_are_not_blocks_stay_literal() {
        for name in [
            "{ca546566-e370-4cde-98d3-acc110c5967d}.xpi",
            "a{b}.conf",
            "a{}.conf",
            "a{+}.conf",
            "a{host}.conf",
            "a{host=}.conf",
            "a{HOST=x}.conf",
            "a{host=x.y}.conf",
            "a{host=x,os=linux}.conf",
            "a{tmpl+bogus}.conf",
            "a{os=linux.conf",
            "a}os=linux{.conf",
        ] {
            assert_eq!(parse_name(name), Name::Literal, "{name}");
            assert!(is_literal(name), "{name}");
        }
        let sources = Sources::group([".mozilla/{ca546566-e370-4cde-98d3-acc110c5967d}.xpi"]);
        assert_eq!(
            sources.keys().collect::<Vec<_>>(),
            [".mozilla/{ca546566-e370-4cde-98d3-acc110c5967d}.xpi"]
        );
    }

    #[test]
    fn the_target_is_the_name_without_its_block() {
        assert_eq!(
            target("monitors{host=laptop}.conf").as_deref(),
            Some("monitors.conf")
        );
        assert_eq!(target(".zshrc{os=macos}").as_deref(), Some(".zshrc"));
        assert_eq!(target("kitty{tmpl}.conf").as_deref(), Some("kitty.conf"));
        assert_eq!(
            target("hosts{host=x+age}.yml").as_deref(),
            Some("hosts.yml")
        );
        assert_eq!(target("a{os!=macos+profile=Work}").as_deref(), Some("a"));
        assert_eq!(target("a{b}{os=linux}.conf").as_deref(), Some("a{b}.conf"));
        assert!(!is_literal("x{os=linux}"));
    }

    #[test]
    fn only_the_final_component_carries_a_block() {
        let sources = Sources::group([".config/{os=linux}/x", ".config/a{os=linux}/b"]);
        assert_eq!(
            sources.keys().collect::<Vec<_>>(),
            [".config/a{os=linux}/b", ".config/{os=linux}/x"]
        );
        let sources = Sources::group([".config/hypr/monitors{host=a}.conf"]);
        assert!(sources.contains(".config/hypr/monitors.conf"));
        assert_eq!(
            sources
                .sources_of(".config/hypr/monitors.conf")
                .collect::<Vec<_>>(),
            [".config/hypr/monitors{host=a}.conf"]
        );
    }

    #[test]
    fn tmpl_with_age_and_double_blocks_are_invalid() {
        let id = machine("x", "linux");
        for (paths, reason) in [
            (&["kitty{tmpl+age}.conf"][..], ONE_FLAG),
            (&["kitty.conf", "kitty{host=x+tmpl+age}.conf"], ONE_FLAG),
            (&["kitty{age+age}.conf"], ONE_FLAG),
            (&["a{os=linux}{host=x}"], ONE_BLOCK),
        ] {
            let selection = pick(paths, &id);
            let Selection::Invalid(problems) = &selection else {
                unreachable!("{paths:?} should be invalid: {selection:?}");
            };
            assert_eq!(problems.len(), 1, "{paths:?}");
            assert!(problems.iter().all(|(_, r)| *r == reason), "{paths:?}");
        }
        assert!(matches!(
            parse_name("kitty{tmpl+age}.conf"),
            Name::Invalid { target, .. } if target == "kitty.conf"
        ));
    }

    #[test]
    fn a_target_that_is_not_a_usable_key_is_invalid() {
        let id = machine("x", "linux");
        for path in [
            ".git{os=linux}",
            "install{os=linux}.sh",
            ".dotman{os=linux}.toml",
            "{os=linux}",
            "dir/{os=linux}",
            "..{os=linux}",
            "e{os=linux}\u{301}",
        ] {
            let sources = Sources::group([path]);
            assert_eq!(sources.keys().collect::<Vec<_>>(), [path], "{path}");
            assert_eq!(
                sources.select(path, &id),
                Selection::Invalid(vec![(path.to_owned(), BAD_TARGET)]),
                "{path}"
            );
        }
        assert_eq!(
            pick(&["sub/install{os=linux}.sh"], &id),
            winner("sub/install{os=linux}.sh")
        );
    }

    #[test]
    fn the_block_flag_is_kept_per_source() {
        let sources = Sources::group(["a.conf", "a{tmpl}.conf", "a{host=x+age}.conf"]);
        assert_eq!(sources.flag("a.conf", "a.conf"), Flag::Plain);
        assert_eq!(sources.flag("a.conf", "a{tmpl}.conf"), Flag::Tmpl);
        assert_eq!(sources.flag("a.conf", "a{host=x+age}.conf"), Flag::Age);
        assert_eq!(sources.flag("a.conf", "missing"), Flag::Plain);
    }

    #[test]
    fn host_beats_os() {
        let id = machine("x", "linux");
        let paths = ["a.conf", "a{os=linux}.conf", "a{host=x}.conf"];
        assert_eq!(pick(&paths, &id), winner("a{host=x}.conf"));
    }

    #[test]
    fn more_conditions_beat_a_prefix_of_them() {
        let id = machine("x", "linux");
        assert_eq!(
            pick(&["a{host=x}.conf", "a{host=x+os=linux}.conf"], &id),
            winner("a{host=x+os=linux}.conf")
        );
        let paths = [
            "a{host=x}.conf",
            "a{host=x+os=linux}.conf",
            "a{os=linux+host=x+user=u}.conf",
        ];
        assert_eq!(pick(&paths, &id), winner("a{os=linux+host=x+user=u}.conf"));
        assert_eq!(
            pick(&["a{host=x+arch=x86_64}", "a{host=x+os=linux}"], &id),
            winner("a{host=x+os=linux}")
        );
    }

    #[test]
    fn every_key_ranks_in_order() {
        let id = machine("x", "linux");
        let ladder = [
            "a{arch=x86_64}",
            "a{os=linux}",
            "a{like=manjaro}",
            "a{distro=arch}",
            "a{profile=home}",
            "a{user=u}",
            "a{host=x}",
        ];
        for (i, top) in ladder.iter().enumerate() {
            let below = ladder.get(..i).unwrap_or_default();
            let mut paths = vec!["a", *top];
            paths.extend_from_slice(below);
            assert_eq!(pick(&paths, &id), winner(top), "{top}");
        }
    }

    #[test]
    fn a_condition_that_fails_removes_the_candidate() {
        let id = machine("x", "linux");
        assert_eq!(pick(&["a", "a{host=y}"], &id), winner("a"));
        assert_eq!(pick(&["a{host=y}"], &id), Selection::None);
        assert_eq!(
            pick(&["a", "a{host=X+os=LINUX}"], &id),
            winner("a{host=X+os=LINUX}")
        );
        assert_eq!(pick(&["a", "a{host=x+os=macos}"], &id), winner("a"));
    }

    #[test]
    fn negated_conditions_beat_only_the_plain_fallback() {
        let mac = machine("x", "macos");
        assert_eq!(
            pick(&["a.conf", "a{os!=macos}.conf"], &mac),
            winner("a.conf")
        );
        assert_eq!(pick(&["a{os!=macos}.conf"], &mac), Selection::None);
        let linux = machine("x", "linux");
        assert_eq!(
            pick(&["a.conf", "a{os!=macos}.conf"], &linux),
            winner("a{os!=macos}.conf")
        );
        assert_eq!(
            pick(
                &["a.conf", "a{os!=macos}.conf", "a{arch=x86_64}.conf"],
                &linux
            ),
            winner("a{arch=x86_64}.conf")
        );
        assert!(matches!(
            pick(&["a.conf", "a{os!=macos}.conf", "a{host!=y}.conf"], &linux),
            Selection::Ambiguous(tied) if tied == ["a{os!=macos}.conf", "a{host!=y}.conf"]
        ));
        assert!(matches!(
            pick(&["a{host=x}.conf", "a{host=x+os!=macos}.conf"], &linux),
            Selection::Ambiguous(_)
        ));
        let paths = [
            "a{os=linux}.conf",
            "a{os=linux+host!=y}.conf",
            "a{os!=macos+distro=arch}.conf",
        ];
        assert!(matches!(pick(&paths[..2], &linux), Selection::Ambiguous(_)));
        assert_eq!(
            pick(&paths, &linux),
            winner("a{os!=macos+distro=arch}.conf")
        );
    }

    #[test]
    fn on_wsl_os_wsl_beats_os_linux() {
        let wsl = machine("x", "wsl");
        let paths = ["a", "a{os=linux}", "a{os=wsl}"];
        assert_eq!(pick(&paths, &wsl), winner("a{os=wsl}"));
        assert_eq!(pick(&["a", "a{os=linux}"], &wsl), winner("a{os=linux}"));
        assert_eq!(pick(&["a", "a{os!=linux}"], &wsl), winner("a"));
        assert_eq!(pick(&paths, &machine("x", "linux")), winner("a{os=linux}"));
        assert_eq!(
            pick(&["a{os=wsl}", "a{distro=arch}"], &wsl),
            winner("a{distro=arch}")
        );
    }

    #[test]
    fn equal_ranks_are_ambiguous() {
        let id = machine("x", "linux");
        assert_eq!(
            pick(&["x", "x{tmpl}"], &id),
            Selection::Ambiguous(vec!["x".to_owned(), "x{tmpl}".to_owned()])
        );
        assert!(matches!(
            pick(&["a{profile=work}", "a{profile=home}"], &id),
            Selection::Ambiguous(_)
        ));
        assert!(matches!(
            pick(&["a{like=archlinux}", "a{like=manjaro}"], &id),
            Selection::Ambiguous(_)
        ));
        assert_eq!(
            pick(&["x", "x{tmpl}", "x{host=x}"], &id),
            winner("x{host=x}")
        );
    }

    #[test]
    fn profiles_and_like_match_any_listed_value() {
        let id = machine("x", "linux");
        assert_eq!(
            pick(&["a", "a{profile=home}"], &id),
            winner("a{profile=home}")
        );
        assert_eq!(pick(&["a", "a{profile=other}"], &id), winner("a"));
        assert_eq!(
            pick(&["a", "a{like=manjaro}"], &id),
            winner("a{like=manjaro}")
        );
        assert_eq!(pick(&["a", "a{profile!=work}"], &id), winner("a"));
    }

    #[test]
    fn add_for_places_the_block_before_the_last_dot() {
        let place = |key: &str, conds: &str| variant_source(key, conds).unwrap();
        assert_eq!(place(".zshrc", "os=macos"), ".zshrc{os=macos}");
        assert_eq!(
            place(".config/hypr/monitors.conf", "host=laptop"),
            ".config/hypr/monitors{host=laptop}.conf"
        );
        assert_eq!(place("a.tar.gz", "os=linux"), "a.tar{os=linux}.gz");
        assert_eq!(place(".config/x/.env", "host=a"), ".config/x/.env{host=a}");
        assert_eq!(
            place("bin/run", "arch=aarch64+os!=macos"),
            "bin/run{arch=aarch64+os!=macos}"
        );
        for bad in [
            "",
            "tmpl",
            "host=a+age",
            "host",
            "host=a,os=b",
            "x=1",
            "{host=a}",
        ] {
            assert!(variant_source(".zshrc", bad).is_err(), "{bad}");
        }
        let source = place(".config/a{b}.conf", "host=a");
        let sources = Sources::group([source.as_str()]);
        assert_eq!(sources.keys().collect::<Vec<_>>(), [".config/a{b}.conf"]);
    }

    #[test]
    fn the_age_flag_joins_an_existing_block_or_gets_its_own() {
        let encrypt = |source: &str| with_age(source).unwrap();
        assert_eq!(encrypt(".config/gh/hosts.yml"), ".config/gh/hosts{age}.yml");
        assert_eq!(encrypt(".netrc"), ".netrc{age}");
        assert_eq!(encrypt("a{host=x}.conf"), "a{host=x+age}.conf");
        assert_eq!(encrypt("a{host=x+age}.conf"), "a{host=x+age}.conf");
        assert_eq!(encrypt("d/{ca54-e3}.xpi"), "d/{ca54-e3}{age}.xpi");
        assert!(with_age("kitty{tmpl}.conf").is_err());
        assert!(with_age("a{age+tmpl}.conf").is_err());
        let sources = Sources::group(["a{host=x+age}.conf"]);
        assert_eq!(sources.flag("a.conf", "a{host=x+age}.conf"), Flag::Age);
    }

    #[test]
    fn the_tmpl_flag_joins_an_existing_block_or_gets_its_own() {
        let template = |source: &str| with_tmpl(source).unwrap();
        assert_eq!(template(".gitconfig"), ".gitconfig{tmpl}");
        assert_eq!(template("a/kitty.conf"), "a/kitty{tmpl}.conf");
        assert_eq!(template("a{host=x}.conf"), "a{host=x+tmpl}.conf");
        assert_eq!(template("a{host=x+tmpl}.conf"), "a{host=x+tmpl}.conf");
        assert!(with_tmpl("a{age}.conf").is_err());
        assert!(with_tmpl("a{host=x+age}.conf").is_err());
    }

    proptest! {
        #[test]
        fn with_tmpl_names_the_same_key_as_a_template(
            key in "(\\.?[a-z]{1,6}/)?\\.?[a-z]{1,6}(\\.[a-z]{1,3})?",
            host in proptest::option::of("[a-z]{1,4}"),
        ) {
            let source = host.as_ref().map_or_else(
                || key.clone(),
                |host| variant_source(&key, &format!("host={host}")).unwrap(),
            );
            let template = with_tmpl(&source).unwrap();
            let sources = Sources::group([template.as_str()]);
            prop_assert_eq!(sources.keys().collect::<Vec<_>>(), [key.as_str()]);
            prop_assert_eq!(sources.flag(&key, &template), Flag::Tmpl);
            prop_assert_eq!(with_tmpl(&template).unwrap(), template);
        }

        #[test]
        fn parsing_never_panics_and_brace_free_names_stay_literal(
            name in prop_oneof![
                "[a-z{}=!+._-]{0,24}",
                "(host|os|tmpl|age|\\{|\\}|=|!=|\\+|x|\\.|é){0,12}",
                "\\PC{0,24}",
            ],
        ) {
            let parsed = parse_name(&name);
            if !name.contains('{') {
                prop_assert_eq!(&parsed, &Name::Literal);
            }
            if let Name::Variant { target, .. } = &parsed {
                prop_assert!(target.len() < name.len());
            }
            let path = format!("dir/{name}");
            let sources = Sources::group([name.as_str(), path.as_str()]);
            for key in sources.keys() {
                let _ = sources.select(key, &machine("x", "wsl"));
            }
        }
    }
}
