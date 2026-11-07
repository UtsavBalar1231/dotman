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

// Positive conditions, most specific first. `Vec`'s ordering is the selection rule: elements are
// compared in turn, and a list that extends another wins. The flag makes `os=wsl` outrank
// `os=linux`; off WSL `os=wsl` never holds, so it never competes.
type Rank = Vec<(Reverse<Key>, bool)>;

fn rank(conditions: &[Condition], identity: &IdentitySnapshot) -> Option<Rank> {
    if !conditions.iter().all(|c| c.holds(identity)) {
        return None;
    }
    let mut rank: Rank = conditions
        .iter()
        .filter(|c| !c.negated)
        .map(|c| (Reverse(c.key), c.key == Key::Os && c.value == "wsl"))
        .collect();
    rank.sort_unstable_by(|a, b| b.cmp(a));
    Some(rank)
}

#[derive(Debug, PartialEq, Eq)]
enum Name {
    Literal,
    Variant {
        target: String,
        conditions: Vec<Condition>,
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
        Some((target, Ok(conditions))) => Name::Variant { target, conditions },
        Some((target, Err(reason))) => Name::Invalid { target, reason },
    }
}

// `None` when `inner` is not a block, so its braces are part of the name.
fn parse_block(inner: &str) -> Option<Result<Vec<Condition>, &'static str>> {
    let mut conditions = Vec::new();
    let mut flags = 0usize;
    for term in inner.split('+') {
        if matches!(term, "tmpl" | "age") {
            flags = flags.saturating_add(1);
        } else {
            conditions.push(parse_condition(term)?);
        }
    }
    Some(if flags > 1 {
        Err(ONE_FLAG)
    } else {
        Ok(conditions)
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

/// The source path that makes `key` a variant for `conditions`, such as `host=laptop+os=linux`.
/// The block goes before the last `.` of the name, unless that dot is the name's first character.
pub fn variant_source(key: &str, conditions: &str) -> Result<String> {
    let only_conditions = matches!(parse_block(conditions), Some(Ok(parsed))
        if parsed.len() == conditions.split('+').count());
    if !only_conditions {
        bail!("`{conditions}` is not a list of conditions such as `host=laptop+os=linux`");
    }
    let (dir, name) = key
        .rsplit_once('/')
        .map_or((None, key), |(dir, name)| (Some(dir), name));
    let named = match name
        .rfind('.')
        .filter(|dot| *dot > 0)
        .and_then(|dot| name.split_at_checked(dot))
    {
        Some((stem, ext)) => format!("{stem}{{{conditions}}}{ext}"),
        None => format!("{name}{{{conditions}}}"),
    };
    Ok(dir.map_or_else(|| named.clone(), |dir| format!("{dir}/{named}")))
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
    Variant(Vec<Condition>),
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
                Name::Variant { target, conditions } => join(&target).map_or_else(
                    || (path.to_owned(), Form::Invalid(BAD_TARGET)),
                    |key| (key, Form::Variant(conditions)),
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

    /// Picks `key`'s source for `identity`. The plain file is a candidate with no conditions,
    /// and negated conditions filter candidates without ranking them.
    #[must_use]
    pub fn select(&self, key: &str, identity: &IdentitySnapshot) -> Selection<&str> {
        let Some(candidates) = self.by_key.get(key) else {
            return Selection::None;
        };
        let invalid: Vec<(String, &'static str)> = candidates
            .iter()
            .filter_map(|(source, form)| match form {
                Form::Invalid(reason) => Some((source.clone(), *reason)),
                Form::Plain | Form::Variant(_) => None,
            })
            .collect();
        if !invalid.is_empty() {
            return Selection::Invalid(invalid);
        }
        let mut best: Option<Rank> = None;
        let mut top: Vec<&str> = Vec::new();
        for (source, form) in candidates {
            let rank = match form {
                Form::Plain => Some(Rank::new()),
                Form::Variant(conditions) => rank(conditions, identity),
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
    fn negated_conditions_filter_without_ranking() {
        let mac = machine("x", "macos");
        assert_eq!(
            pick(&["a.conf", "a{os!=macos}.conf"], &mac),
            winner("a.conf")
        );
        assert_eq!(pick(&["a{os!=macos}.conf"], &mac), Selection::None);
        // On Linux both candidates rank empty, which is a tie.
        let linux = machine("x", "linux");
        assert!(matches!(
            pick(&["a.conf", "a{os!=macos}.conf"], &linux),
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

    proptest! {
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
