//! The README names only commands that exist, and AGENTS.md names only paths that exist.

use clap::CommandFactory;
use dotman::cli::Cli;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::Path;

type R = Result<(), Box<dyn Error>>;

fn read(name: &str) -> Result<String, Box<dyn Error>> {
    Ok(fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(name),
    )?)
}

/// The word after each `dotman ` or `dot ` in the code spans and fenced blocks of `text`.
fn commands_in(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let code: Vec<&str> = if fenced {
            vec![line]
        } else {
            line.split('`').skip(1).step_by(2).collect()
        };
        for span in code {
            let words: Vec<&str> = span.split_whitespace().collect();
            for pair in words.windows(2) {
                let [first, second] = pair else { continue };
                if matches!(*first, "dotman" | "dot")
                    && second.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                {
                    found.insert((*second).to_owned());
                }
            }
        }
    }
    found
}

#[test]
fn every_readme_command_exists() -> R {
    let known: BTreeSet<String> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect();
    let used = commands_in(&read("README.md")?);
    assert!(used.len() > 10, "the extractor found only {used:?}");
    for cmd in &used {
        assert!(
            known.contains(cmd),
            "README names `dotman {cmd}`, which is not a subcommand"
        );
    }
    Ok(())
}

#[test]
fn every_structure_path_in_agents_exists() -> R {
    let text = read("AGENTS.md")?;
    let section = text
        .split("## Structure")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .ok_or("AGENTS.md has no Structure section")?;
    let mut checked = 0_usize;
    for line in section.lines() {
        let Some(path) = line.split_whitespace().next().filter(|w| w.contains('/')) else {
            continue;
        };
        let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        assert!(
            full.exists(),
            "AGENTS.md structure lists {path}, which does not exist"
        );
        checked = checked.saturating_add(1);
    }
    assert!(checked > 10, "only {checked} paths were checked");
    Ok(())
}

#[test]
fn agents_has_no_private_item_doc_rule() -> R {
    assert!(!read("AGENTS.md")?.contains("including private items"));
    Ok(())
}

/// Every `uses:` in the workflows names a full commit SHA, so a moved tag cannot change what CI
/// and the release run, and a comment says which version that commit is.
#[test]
fn every_workflow_action_is_pinned_to_a_commit() -> R {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut checked = 0_usize;
    for file in fs::read_dir(&dir)? {
        let path = file?.path();
        let text = fs::read_to_string(&path)?;
        let name = path.display();
        for line in text.lines() {
            let Some(used) = line
                .trim_start()
                .trim_start_matches("- ")
                .strip_prefix("uses:")
            else {
                continue;
            };
            let (action, comment) = used.split_once('#').unwrap_or((used, ""));
            let action = action.trim().trim_matches(['"', '\'']);
            if action.starts_with("./") {
                continue;
            }
            let sha = action.rsplit_once('@').map_or("", |(_, sha)| sha);
            assert!(
                sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "{name}: `{action}` is not pinned to a commit SHA"
            );
            assert!(
                !comment.trim().is_empty(),
                "{name}: `{action}` has no version comment"
            );
            checked = checked.saturating_add(1);
        }
    }
    assert!(checked > 20, "only {checked} uses: lines were checked");
    Ok(())
}

#[test]
fn readme_installs_only_this_project() -> R {
    let readme = read("README.md")?;
    for line in readme.lines() {
        if line.contains("cargo install dotman ") || line.trim_end() == "cargo install dotman" {
            assert!(
                line.contains("--git"),
                "plain `cargo install dotman` installs another project: {line}"
            );
        }
    }
    assert!(!readme.contains("yay -S dotman"));
    assert!(!readme.contains("SECURITY_MIGRATION"));
    Ok(())
}
