//! Rendering of `{tmpl}` sources. Each render builds its own environment: strict about undefined
//! names, with no loader (so no `include` or `import`) and no functions beyond minijinja's builtins.

use crate::config::Config;
use crate::state::IdentitySnapshot;
use anyhow::{Context, Result, anyhow};
use minijinja::syntax::SyntaxConfig;
use minijinja::{AutoEscape, Environment, Error, UndefinedBehavior, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// The variables a template can read.
#[derive(Debug, Default, Clone)]
pub struct Vars(BTreeMap<String, Value>);

impl Vars {
    /// `env` looks up process environment variables; only the names in the machine config's
    /// `[template] env` are asked for, so any other name is undefined in a template.
    #[must_use]
    pub fn new(
        identity: &IdentitySnapshot,
        machine: &Config,
        home: &Path,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let allowed: BTreeMap<String, String> = machine
            .template
            .env
            .iter()
            .filter_map(|name| env(name).map(|value| (name.clone(), value)))
            .collect();
        let text = |s: &str| Value::from(s);
        Self(BTreeMap::from([
            ("host".to_owned(), text(&identity.host)),
            ("user".to_owned(), text(&identity.user)),
            ("distro".to_owned(), text(&identity.distro)),
            ("os".to_owned(), text(&identity.os)),
            ("arch".to_owned(), text(&identity.arch)),
            ("home".to_owned(), text(&home.to_string_lossy())),
            (
                "profiles".to_owned(),
                identity.profiles.iter().map(String::as_str).collect(),
            ),
            (
                "like".to_owned(),
                identity.like.split_whitespace().collect(),
            ),
            ("vars".to_owned(), Value::from(machine.vars.clone())),
            ("env".to_owned(), Value::from(allowed)),
        ]))
    }
}

/// Renders `text`, the contents of the repo path `source`. A failure names the path, the line
/// and the expression that failed, which for an undefined name is the name.
pub fn render(source: &str, text: &str, vars: &Vars) -> Result<String> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_debug(true);
    // Config files are not HTML, whatever their extension.
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_syntax(
        SyntaxConfig::builder()
            .keep_trailing_newline(true)
            .build()
            .context("cannot configure the template syntax")?,
    );
    env.render_named_str(source, text, Value::from(vars.0.clone()))
        .map_err(|e| explain(source, text, &e))
}

fn explain(source: &str, text: &str, e: &Error) -> anyhow::Error {
    let line = e.line().map(|line| format!(":{line}")).unwrap_or_default();
    let expression = e
        .range()
        .and_then(|range| text.get(range))
        .map(|expression| format!(" in `{expression}`"))
        .unwrap_or_default();
    let detail = e
        .detail()
        .map_or_else(|| e.kind().to_string(), str::to_owned);
    anyhow!("{source}{line}: {detail}{expression}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(env: &[&str]) -> Vars {
        let mut machine = Config::default();
        machine.vars.insert("font".to_owned(), "14".to_owned());
        machine.template.env = env.iter().map(|s| (*s).to_owned()).collect();
        let identity = IdentitySnapshot {
            host: "box".to_owned(),
            like: "archlinux manjaro".to_owned(),
            profiles: vec!["work".to_owned()],
            ..IdentitySnapshot::default()
        };
        Vars::new(&identity, &machine, Path::new("/home/u"), |name| {
            (name == "SECRET" || name == "OPEN").then(|| format!("{name}-value"))
        })
    }

    fn render_str(text: &str, env: &[&str]) -> Result<String> {
        render("dir/a{tmpl}.conf", text, &vars(env))
    }

    #[test]
    fn variables_render_and_the_trailing_newline_survives() {
        let out = render_str(
            "{{ host }} {{ home }} {{ vars.font }}\n{% for l in like %}{{ l }};{% endfor %} {{ profiles | join(',') }} <&>\n",
            &[],
        )
        .unwrap();
        assert_eq!(out, "box /home/u 14\narchlinux;manjaro; work <&>\n");
    }

    #[test]
    fn undefined_names_fail_and_are_named() {
        for (text, name) in [
            ("a\n{{ vars.size }}", "vars.size"),
            ("{{ nothing }}", "nothing"),
        ] {
            let err = format!("{:#}", render_str(text, &[]).unwrap_err());
            assert!(err.contains("dir/a{tmpl}.conf"), "{err}");
            assert!(err.contains(name), "{err}");
        }
    }

    #[test]
    fn only_allowlisted_environment_variables_are_visible() {
        let err = format!("{:#}", render_str("{{ env.SECRET }}", &[]).unwrap_err());
        assert!(err.contains("env.SECRET"), "{err}");
        let err = format!(
            "{:#}",
            render_str("{{ env.SECRET }}", &["OPEN"]).unwrap_err()
        );
        assert!(err.contains("env.SECRET"), "{err}");
        assert_eq!(
            render_str("{{ env.SECRET }}", &["SECRET"]).unwrap(),
            "SECRET-value"
        );
    }

    #[test]
    fn there_is_no_loader() {
        for text in ["{% include 'b' %}", "{% import 'b' as b %}"] {
            assert!(render_str(text, &[]).is_err(), "{text}");
        }
    }
}
