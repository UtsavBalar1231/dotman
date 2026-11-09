//! yadm's `##` alternates and default-processor templates, translated to dotman's names and to
//! minijinja. The weights, the winner rule and the template grammar follow `score_file`,
//! `record_score` and `template_default` in yadm at bbb58e6.

use crate::state::IdentitySnapshot;
use crate::variant::{self, Flag, Selection, Sources};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// A reason for leaving something out, and what is wrong with it.
pub(super) type Refusal = (&'static str, String);

pub(super) const DIRECTORY: &str = "alternates on a directory, which dotman cannot express";
pub(super) const INEXPRESSIBLE: &str =
    "alternates dotman cannot express (an unknown condition, or a value outside A-Z a-z 0-9 _ -)";
pub(super) const OTHER_PROCESSOR: &str =
    "templates for esh, envtpl or j2, which dotman does not render";
pub(super) const TEMPLATE: &str = "yadm templates that cannot be converted exactly";

const BLANK: [char; 2] = [' ', '\t'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cond {
    Arch,
    Os,
    Distro,
    Family,
    Class,
    Host,
    User,
}

impl Cond {
    const fn weight(self) -> u32 {
        match self {
            Self::Arch => 1,
            Self::Os => 2,
            Self::Distro => 4,
            Self::Family => 8,
            Self::Class => 16,
            Self::Host => 32,
            Self::User => 64,
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Arch => "arch",
            Self::Os => "os",
            Self::Distro => "distro",
            Self::Family => "like",
            Self::Class => "profile",
            Self::Host => "host",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Term {
    Default,
    Extension,
    Template,
    Cond {
        cond: Cond,
        negated: bool,
        value: String,
    },
}

/// The values yadm compares conditions with on this machine.
#[derive(Debug, Clone)]
pub(super) struct Machine {
    arch: String,
    /// `Linux`, `Darwin` or `WSL`, as `uname -s` and yadm spell them.
    os: String,
    distro: String,
    family: String,
    classes: Vec<String>,
    host: String,
    user: String,
}

impl Machine {
    pub(super) fn from_identity(id: &IdentitySnapshot) -> Self {
        let os = match id.os.as_str() {
            "linux" => "Linux",
            "macos" => "Darwin",
            "wsl" => "WSL",
            other => other,
        };
        Self {
            arch: id.arch.clone(),
            os: os.to_owned(),
            distro: id.distro.clone(),
            family: id.like.replace(' ', "_"),
            classes: id.profiles.clone(),
            host: id.host.clone(),
            user: id.user.clone(),
        }
    }

    fn holds(&self, cond: Cond, value: &str) -> bool {
        let same = |have: &str| have.eq_ignore_ascii_case(value);
        match cond {
            Cond::Arch => same(&self.arch),
            Cond::Os => same(&self.os),
            Cond::Distro => same(&self.distro.replace(' ', "_")),
            Cond::Family => same(&self.family),
            Cond::Class => self.classes.iter().any(|class| same(class)),
            Cond::Host => same(&self.host),
            Cond::User => same(&self.user),
        }
    }
}

/// A repo path with a `##` suffix, read the way yadm reads it.
#[derive(Debug, Clone)]
pub(super) struct Alternate {
    /// The path without the suffix: where yadm links or renders it.
    pub(super) target: String,
    terms: Vec<Term>,
}

/// `None` for a path with no `##`. Otherwise the alternate, or why dotman cannot express it.
pub(super) fn parse(path: &str) -> Option<Result<Alternate, Refusal>> {
    let (target, tail) = path.split_once("##")?;
    if tail.contains('/') {
        return Some(Err((
            DIRECTORY,
            format!("{path}: dotman blocks go on the final component only"),
        )));
    }
    Some(
        terms(tail)
            .map(|terms| Alternate {
                target: target.to_owned(),
                terms,
            })
            .map_err(|(reason, why)| (reason, format!("{path}: {why}"))),
    )
}

fn terms(tail: &str) -> Result<Vec<Term>, Refusal> {
    let mut out = Vec::new();
    for field in tail.split(',') {
        let (label, value) = field.split_once('.').unwrap_or((field, ""));
        let (negated, label) = label
            .strip_prefix('~')
            .map_or((false, label), |label| (true, label));
        let label = label.to_ascii_lowercase();
        let cond = match label.as_str() {
            "default" | "t" | "template" | "yadm" if negated => {
                return Err((INEXPRESSIBLE, format!("`~{label}` is invalid in yadm too")));
            }
            "default" => {
                out.push(Term::Default);
                continue;
            }
            "e" | "extension" => {
                out.push(Term::Extension);
                continue;
            }
            "t" | "template" | "yadm" => {
                match value.to_ascii_lowercase().as_str() {
                    "" | "default" => out.push(Term::Template),
                    "esh" | "j2" | "j2cli" | "envtpl" => {
                        return Err((
                            OTHER_PROCESSOR,
                            format!("uses the {value} template processor"),
                        ));
                    }
                    _ => {
                        return Err((
                            INEXPRESSIBLE,
                            format!("`{value}` is not a template processor yadm knows"),
                        ));
                    }
                }
                continue;
            }
            "s" | "seed" => {
                return Err((
                    INEXPRESSIBLE,
                    "seed alternates are not supported".to_owned(),
                ));
            }
            "a" | "arch" => Cond::Arch,
            "o" | "os" => Cond::Os,
            "d" | "distro" => Cond::Distro,
            "f" | "distro_family" => Cond::Family,
            "c" | "class" => Cond::Class,
            "h" | "hostname" => Cond::Host,
            "u" | "user" => Cond::User,
            _ => return Err((INEXPRESSIBLE, format!("`{label}` is not a yadm condition"))),
        };
        let plain = !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !plain {
            return Err((
                INEXPRESSIBLE,
                format!("the value `{value}` of `{label}` is outside A-Z a-z 0-9 _ -"),
            ));
        }
        if cond == Cond::Os && dotman_os(value).is_none() {
            return Err((
                INEXPRESSIBLE,
                format!("`{value}` is not Linux, Darwin or WSL, the systems dotman runs on"),
            ));
        }
        out.push(Term::Cond {
            cond,
            negated,
            value: value.to_owned(),
        });
    }
    Ok(out)
}

pub(super) fn dotman_os(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "linux" => Some("linux"),
        "darwin" => Some("macos"),
        "wsl" => Some("wsl"),
        _ => None,
    }
}

impl Alternate {
    pub(super) fn is_template(&self) -> bool {
        self.terms.contains(&Term::Template)
    }

    fn block(&self) -> String {
        let mut parts: Vec<String> = self
            .terms
            .iter()
            .filter_map(|term| match term {
                Term::Cond {
                    cond,
                    negated,
                    value,
                } => {
                    let value = if *cond == Cond::Os {
                        dotman_os(value).map(str::to_owned)?
                    } else {
                        value.to_ascii_lowercase()
                    };
                    let op = if *negated { "!=" } else { "=" };
                    Some(format!("{}{op}{value}", cond.key()))
                }
                _ => None,
            })
            .collect();
        if self.is_template() {
            parts.push("tmpl".to_owned());
        }
        parts.join("+")
    }

    /// The dotman path of this alternate. Fails when the result is not a name dotman reads back
    /// as the same target and flag, which also covers a target dotman refuses.
    pub(super) fn dotman_path(&self, original: &str) -> Result<String, Refusal> {
        if !self
            .terms
            .iter()
            .any(|term| !matches!(term, Term::Extension))
        {
            return Err((
                INEXPRESSIBLE,
                format!("{original}: only an extension condition, so yadm never deploys it"),
            ));
        }
        let block = self.block();
        let path = if block.is_empty() {
            self.target.clone()
        } else {
            variant::with_block(&self.target, &block)
                .map_err(|e| (INEXPRESSIBLE, format!("{original}: {e}")))?
        };
        let group = Sources::group([path.as_str()]);
        let flag = if self.is_template() {
            Flag::Tmpl
        } else {
            Flag::Plain
        };
        let sound = group.contains(&self.target)
            && group.flag(&self.target, &path) == flag
            && !matches!(
                group.select(&self.target, &IdentitySnapshot::default()),
                Selection::Invalid(_)
            );
        if sound {
            Ok(path)
        } else {
            Err((
                INEXPRESSIBLE,
                format!(
                    "{original}: {path} is not a name dotman reads as a variant of {}",
                    self.target
                ),
            ))
        }
    }

    /// yadm's score on `machine`; `None` when a condition fails and yadm skips the file.
    fn score(&self, machine: &Machine) -> Option<u32> {
        let mut score: u32 = 0;
        for term in &self.terms {
            match term {
                Term::Default | Term::Template => score = score.saturating_add(1000),
                Term::Extension => {}
                Term::Cond {
                    cond,
                    negated,
                    value,
                } => {
                    if machine.holds(*cond, value) == *negated {
                        return None;
                    }
                    let bonus = if *negated { 0 } else { 1000 };
                    score = score.saturating_add(cond.weight()).saturating_add(bonus);
                }
            }
        }
        Some(score)
    }
}

/// The index in `alternates`, sorted by path as `git ls-files` lists them, of the file yadm
/// links or renders for their shared target. A tie keeps the earlier file, and a template beats
/// a plain file whatever the scores.
pub(super) fn winner(alternates: &[&Alternate], machine: &Machine) -> Option<usize> {
    let mut best: Option<(usize, u32, bool)> = None;
    for (at, alternate) in alternates.iter().enumerate() {
        let template = alternate.is_template();
        let Some(score) = alternate.score(machine) else {
            continue;
        };
        if score == 0 && !template {
            continue;
        }
        let replaces = match best {
            None => true,
            Some((_, current, true)) => template && score >= current,
            Some((_, current, false)) => template || score > current,
        };
        if replaces {
            best = Some((at, score, template));
        }
    }
    best.map(|(at, _, _)| at)
}

/// A converted template body and the environment variables it reads.
#[derive(Debug)]
pub(super) struct Rewritten {
    pub(super) text: String,
    pub(super) env: BTreeSet<String>,
    /// The printed yadm variables whose dotman value is lowercase where yadm's is not, such as
    /// `os`, which yadm prints as `Linux` and dotman as `linux`.
    pub(super) cased: BTreeSet<&'static str>,
}

enum Line<'a> {
    If { test: String, env: Option<&'a str> },
    Else,
    Endif,
    Include,
    Text,
}

/// Rewrites the body of a default-processor template into minijinja. yadm prints no line for a
/// tag, so each converted tag ends in a `{#`/`#}` comment that swallows its newline.
pub(super) fn rewrite_template(text: &str) -> Result<Rewritten, String> {
    let mut out = String::with_capacity(text.len());
    let mut env = BTreeSet::new();
    let mut cased = BTreeSet::new();
    let mut open: Vec<bool> = Vec::new();
    for raw in text.split_inclusive('\n') {
        let (line, newline) = raw
            .strip_suffix('\n')
            .map_or((raw, ""), |line| (line, "\n"));
        let tag = |tag: &str| {
            if newline.is_empty() {
                tag.to_owned()
            } else {
                format!("{tag}{{#\n#}}")
            }
        };
        match classify(line)? {
            Line::If { test, env: name } => {
                env.extend(name.map(str::to_owned));
                open.push(false);
                out.push_str(&tag(&format!("{{% if {test} %}}")));
            }
            Line::Else => {
                let Some(seen_else) = open.last_mut() else {
                    return Err("has an {% else %} with no {% if %}".to_owned());
                };
                if std::mem::replace(seen_else, true) {
                    return Err("has two {% else %} in one {% if %}".to_owned());
                }
                out.push_str(&tag("{% else %}"));
            }
            Line::Endif => {
                if open.pop().is_none() {
                    return Err("has an {% endif %} with no {% if %}".to_owned());
                }
                out.push_str(&tag("{% endif %}"));
            }
            Line::Include => return Err("uses {% include %}".to_owned()),
            Line::Text => {
                out.push_str(&convert_text(line, &mut env, &mut cased)?);
                out.push_str(newline);
            }
        }
    }
    if !open.is_empty() {
        return Err("has an {% if %} with no {% endif %}".to_owned());
    }
    Ok(Rewritten {
        text: out,
        env,
        cased,
    })
}

/// The text between `{%` and `%}` of a line that is one tag.
fn tag_body(line: &str) -> Option<&str> {
    line.trim_start_matches(BLANK)
        .strip_prefix("{%")?
        .strip_suffix("%}")
}

fn classify(line: &str) -> Result<Line<'_>, String> {
    let Some(inner) = tag_body(line) else {
        return Ok(Line::Text);
    };
    let word = inner.trim_start_matches(BLANK);
    if inner.trim_matches(BLANK) == "else" {
        return Ok(Line::Else);
    }
    if inner.trim_matches(BLANK) == "endif" {
        return Ok(Line::Endif);
    }
    if word
        .strip_prefix("include")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(BLANK))
    {
        return Ok(Line::Include);
    }
    let Some(rest) = word.strip_prefix("if") else {
        return Ok(Line::Text);
    };
    let condition = rest.trim_start_matches(BLANK);
    if condition.len() == rest.len() {
        return Ok(Line::Text);
    }
    parse_if(condition).map_or(Ok(Line::Text), |(variable, negated, rhs)| {
        convert_if(variable, negated, rhs)
    })
}

/// `yadm.os == "Linux"` gives `(("yadm", "os"), false, "Linux")`; `None` when yadm would print
/// the line as text.
fn parse_if(condition: &str) -> Option<((&str, &str), bool, &str)> {
    let (namespace, rest) = if let Some(rest) = condition.strip_prefix("env.") {
        ("env", rest)
    } else {
        ("yadm", condition.strip_prefix("yadm.")?)
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let (name, rest) = rest.split_at_checked(end)?;
    if name.is_empty() {
        return None;
    }
    let rest = rest.trim_start_matches(BLANK);
    let (negated, rest) = if let Some(rest) = rest.strip_prefix("==") {
        (false, rest)
    } else {
        (true, rest.strip_prefix("!=")?)
    };
    let quoted = rest.trim_matches(BLANK);
    let rhs = quoted.strip_prefix('"')?.strip_suffix('"')?;
    Some(((namespace, name), negated, rhs))
}

fn convert_if<'a>(
    (namespace, name): (&'a str, &'a str),
    negated: bool,
    rhs: &str,
) -> Result<Line<'a>, String> {
    if rhs.contains("{{") || rhs.contains('"') || rhs.contains('\\') {
        return Err(format!(
            "compares {namespace}.{name} with a value that has a variable, a quote or a backslash"
        ));
    }
    let want = rhs.to_lowercase();
    let eq = if negated { "!=" } else { "==" };
    let membership = if negated { "not in" } else { "in" };
    if namespace == "env" {
        return Ok(Line::If {
            test: format!("env.{name} | lower {eq} \"{want}\""),
            env: Some(name),
        });
    }
    let test = match name {
        "os" => {
            let want = dotman_os(&want).map_or(want, str::to_owned);
            format!("os {eq} \"{want}\"")
        }
        "hostname" => format!("host {eq} \"{want}\""),
        "user" | "distro" | "arch" => format!("{name} {eq} \"{want}\""),
        "distro_family" => format!("\"{want}\" {membership} like"),
        "class" => format!("\"{want}\" {membership} profiles"),
        other => {
            return Err(format!(
                "tests yadm.{other}, which has no dotman equivalent"
            ));
        }
    };
    Ok(Line::If { test, env: None })
}

/// Replaces yadm variables, and writes every other `{{`, `{%` and `{#` as a string so that
/// minijinja prints it as yadm does.
fn convert_text(
    line: &str,
    env: &mut BTreeSet<String>,
    cased: &mut BTreeSet<&'static str>,
) -> Result<String, String> {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('{') {
        let Some((head, tail)) = rest.split_at_checked(at) else {
            break;
        };
        out.push_str(head);
        let Some(delimiter) = tail.get(..2).filter(|d| matches!(*d, "{{" | "{%" | "{#")) else {
            out.push('{');
            rest = tail.get(1..).unwrap_or_default();
            continue;
        };
        if delimiter == "{{"
            && let Some((expression, after)) = variable(tail, env, cased)?
        {
            let _ = write!(out, "{{{{ {expression} }}}}");
            rest = after;
            continue;
        }
        let _ = write!(out, "{{{{ \"{delimiter}\" }}}}");
        rest = tail.get(2..).unwrap_or_default();
    }
    out.push_str(rest);
    Ok(out)
}

/// The minijinja expression for the `{{ yadm.X }}` or `{{ env.X }}` that starts `text`, and the
/// text after it. `None` when `text` does not start with one.
fn variable<'a>(
    text: &'a str,
    env: &mut BTreeSet<String>,
    cased: &mut BTreeSet<&'static str>,
) -> Result<Option<(String, &'a str)>, String> {
    let Some(inner) = text.strip_prefix("{{") else {
        return Ok(None);
    };
    let inner = inner.trim_start_matches(BLANK);
    let (namespace, inner) = if let Some(rest) = inner.strip_prefix("env.") {
        ("env", rest)
    } else if let Some(rest) = inner.strip_prefix("yadm.") {
        ("yadm", rest)
    } else {
        return Ok(None);
    };
    let end = inner
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(inner.len());
    let Some((name, after)) = inner.split_at_checked(end) else {
        return Ok(None);
    };
    let Some(after) = after.trim_start_matches(BLANK).strip_prefix("}}") else {
        return Ok(None);
    };
    if name.is_empty() {
        return Ok(None);
    }
    let expression = if namespace == "env" {
        env.insert(name.to_owned());
        format!("env.{name}")
    } else {
        match name {
            "os" => {
                cased.insert("os");
                "os".to_owned()
            }
            "distro" => {
                cased.insert("distro");
                "distro".to_owned()
            }
            "hostname" => "host".to_owned(),
            "user" | "arch" => name.to_owned(),
            "distro_family" => "like | join(\" \")".to_owned(),
            "classes" => "profiles | join(\"\\n\")".to_owned(),
            other => {
                return Err(format!(
                    "prints yadm.{other}, which has no dotman equivalent"
                ));
            }
        }
    };
    Ok(Some((expression, after)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::template::{self, Vars};
    use std::path::Path;

    fn identity(host: &str, os: &str) -> IdentitySnapshot {
        IdentitySnapshot {
            host: host.to_owned(),
            user: "me".to_owned(),
            os: os.to_owned(),
            distro: "arch".to_owned(),
            like: "archlinux".to_owned(),
            arch: "x86_64".to_owned(),
            profiles: vec!["work".to_owned(), "home".to_owned()],
        }
    }

    fn named(path: &str) -> Result<String, Refusal> {
        parse(path)
            .expect("has ##")
            .and_then(|alternate| alternate.dotman_path(path))
    }

    fn render(text: &str, id: &IdentitySnapshot, env: &[&str]) -> String {
        let machine = Config {
            template: crate::config::TemplateConfig {
                env: env.iter().map(|n| (*n).to_owned()).collect(),
            },
            ..Config::default()
        };
        let vars = Vars::new(id, &machine, Path::new("/home/me"), |name| {
            Some(format!("<{name}>"))
        });
        template::render("t", text, &vars).expect("renders")
    }

    #[test]
    fn names_follow_the_plan_mapping() {
        for (yadm, dotman) in [
            (".zshrc##o.Darwin", ".zshrc{os=macos}"),
            (".zshrc##os.Linux", ".zshrc{os=linux}"),
            (".zshrc##o.WSL", ".zshrc{os=wsl}"),
            ("x##h.box,c.work", "x{host=box+profile=work}"),
            ("x##hostname.Box,user.Me", "x{host=box+user=me}"),
            (
                "a##d.arch,f.arch,a.x86_64",
                "a{distro=arch+like=arch+arch=x86_64}",
            ),
            ("x##~o.Darwin", "x{os!=macos}"),
            ("x##~h.box,default", "x{host!=box}"),
            ("x##default", "x"),
            ("x##default,e.txt", "x"),
            ("x##o.Linux,e.txt", "x{os=linux}"),
            (
                "config.toml##os.Linux,template",
                "config{os=linux+tmpl}.toml",
            ),
            (".zshrc##template", ".zshrc{tmpl}"),
            (
                ".config/kitty/kitty.conf##t",
                ".config/kitty/kitty{tmpl}.conf",
            ),
        ] {
            assert_eq!(named(yadm).as_deref(), Ok(dotman), "{yadm}");
        }
    }

    #[test]
    fn what_dotman_cannot_express_is_refused_with_a_reason() {
        for (path, reason) in [
            ("dir##os.Linux/file", DIRECTORY),
            ("x##h.my.host", INEXPRESSIBLE),
            ("x##h.", INEXPRESSIBLE),
            ("x##h", INEXPRESSIBLE),
            ("x##bogus.1", INEXPRESSIBLE),
            ("x##o.FreeBSD", INEXPRESSIBLE),
            ("x##~default", INEXPRESSIBLE),
            ("x##~template", INEXPRESSIBLE),
            ("x##seed", INEXPRESSIBLE),
            ("x##e.txt", INEXPRESSIBLE),
            ("x##template.foo", INEXPRESSIBLE),
            ("x##t.esh", OTHER_PROCESSOR),
            ("x##template.j2", OTHER_PROCESSOR),
            ("x##template.envtpl,o.Linux", OTHER_PROCESSOR),
            ("README##o.Linux", INEXPRESSIBLE),
            ("##o.Linux", INEXPRESSIBLE),
        ] {
            let refusal = named(path).expect_err(path);
            assert_eq!(refusal.0, reason, "{path}: {}", refusal.1);
            assert!(refusal.1.contains(path), "{}", refusal.1);
        }
        assert!(parse("plain").is_none());
    }

    fn pick(files: &[&str], machine: &Machine) -> Option<String> {
        let alternates: Vec<Alternate> = files
            .iter()
            .map(|f| parse(f).expect("has ##").expect("parses"))
            .collect();
        let refs: Vec<&Alternate> = alternates.iter().collect();
        winner(&refs, machine).and_then(|at| files.get(at).map(|f| (*f).to_owned()))
    }

    #[test]
    fn the_winner_follows_yadms_weights_and_tie_rule() {
        let m = Machine::from_identity(&identity("box", "linux"));
        assert_eq!(
            pick(&["x##h.box", "x##o.Linux"], &m).as_deref(),
            Some("x##h.box")
        );
        assert_eq!(
            pick(&["x##o.Linux", "x##o.Darwin"], &m).as_deref(),
            Some("x##o.Linux")
        );
        assert_eq!(
            pick(&["x##default", "x##o.Linux"], &m).as_deref(),
            Some("x##o.Linux")
        );
        assert_eq!(pick(&["x##o.Darwin"], &m), None);
        // Sums, not ranks: two classes outscore one host.
        assert_eq!(
            pick(&["x##c.home,c.work", "x##h.box"], &m).as_deref(),
            Some("x##c.home,c.work")
        );
        // A tie keeps the file git lists first.
        assert_eq!(
            pick(&["x##o.Linux", "x##os.linux"], &m).as_deref(),
            Some("x##o.Linux")
        );
        // A negated condition adds its weight but no 1000.
        assert_eq!(
            pick(&["x##~o.Darwin", "x##default"], &m).as_deref(),
            Some("x##default")
        );
        // A template beats a plain file with a higher score, and a later template of at least
        // the same score replaces an earlier one.
        assert_eq!(
            pick(&["x##h.box,u.me", "x##template"], &m).as_deref(),
            Some("x##template")
        );
        assert_eq!(
            pick(&["x##template", "x##template,o.Linux"], &m).as_deref(),
            Some("x##template,o.Linux")
        );
    }

    #[test]
    fn family_is_the_whole_string_to_yadm() {
        let mut id = identity("box", "linux");
        id.like = "ubuntu debian".to_owned();
        let m = Machine::from_identity(&id);
        assert_eq!(pick(&["x##f.debian"], &m), None);
        assert_eq!(
            pick(&["x##f.ubuntu_debian"], &m).as_deref(),
            Some("x##f.ubuntu_debian")
        );
    }

    #[test]
    fn a_default_template_converts_and_renders() {
        let yadm = "\
# {{ yadm.hostname }} {{yadm.user}} {{ yadm.arch }} on {{ yadm.os }}
  {% if yadm.os == \"Linux\" %}
linux only
  {% else %}
not linux
{% endif %}
{% if yadm.class == \"Work\" %}
work class
{% endif %}
{% if yadm.class != \"nope\" %}
no nope class
{% endif %}
{% if yadm.distro_family == \"archlinux\" %}
arch family
{% endif %}
{% if yadm.distro == \"Arch\" %}
arch
{% endif %}
{% if yadm.hostname != \"Other\" %}
not other
{% endif %}
{% if env.EDITOR == \"Vim\" %}
vim
{% endif %}
classes: {{ yadm.classes }}
home: {{ env.HOME }}
literal {{ not_yadm }} and {% raw %} and ${#array[@]}
";
        let done = rewrite_template(yadm).expect("converts");
        assert!(
            done.text.contains("{% if os == \"linux\" %}"),
            "{}",
            done.text
        );
        assert!(done.text.contains("{% if \"work\" in profiles %}"));
        assert_eq!(
            done.env.iter().map(String::as_str).collect::<Vec<_>>(),
            ["EDITOR", "HOME"]
        );
        let id = identity("box", "linux");
        let out = render(&done.text, &id, &["EDITOR", "HOME"]);
        assert_eq!(
            out,
            "\
# box me x86_64 on linux
linux only
work class
no nope class
arch family
arch
not other
classes: work
home
home: <HOME>
literal {{ not_yadm }} and {% raw %} and ${#array[@]}
"
        );
    }

    #[test]
    fn tag_lines_leave_no_blank_line_in_either_branch() {
        let done =
            rewrite_template("a\n{% if yadm.os == \"Linux\" %}\nb\n{% else %}\nc\n{% endif %}\nd")
                .expect("converts");
        assert_eq!(render(&done.text, &identity("h", "linux"), &[]), "a\nb\nd");
        assert_eq!(render(&done.text, &identity("h", "macos"), &[]), "a\nc\nd");
        let first = rewrite_template("{% if yadm.os == \"Darwin\" %}\nmac\n{% endif %}\nrest\n")
            .expect("converts");
        assert!(first.text.contains("{% if os == \"macos\" %}"));
        assert_eq!(
            render(&first.text, &identity("h", "macos"), &[]),
            "mac\nrest\n"
        );
        assert_eq!(render(&first.text, &identity("h", "linux"), &[]), "rest\n");
    }

    #[test]
    fn templates_that_cannot_convert_exactly_are_refused() {
        for (text, why) in [
            ("{% include \"other\" %}\n", "include"),
            ("{{ yadm.source }}\n", "yadm.source"),
            ("{{ yadm.class }}\n", "yadm.class"),
            (
                "{% if yadm.source == \"x\" %}\n{% endif %}\n",
                "yadm.source",
            ),
            (
                "{% if yadm.os == \"{{ yadm.user }}\" %}\n{% endif %}\n",
                "variable",
            ),
            ("{% else %}\n", "else"),
            ("{% endif %}\n", "endif"),
            ("{% if yadm.os == \"Linux\" %}\nx\n", "no {% endif %}"),
            (
                "{% if yadm.os == \"Linux\" %}\n{% else %}\n{% else %}\n{% endif %}\n",
                "two",
            ),
        ] {
            let err = rewrite_template(text).expect_err(text);
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }
}
