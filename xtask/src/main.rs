//! Build tasks for dotman.

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use clap_mangen::Man;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const BIN: &str = "dotman";

/// `after_long_help` blocks that start with one of these become man sections of their own.
const SECTIONS: [(&str, &str); 2] = [
    ("Exit status:", "\"EXIT STATUS\""),
    ("Environment:", "ENVIRONMENT"),
];

#[derive(Parser)]
#[command(name = "xtask", about = "Build tasks for dotman")]
enum Task {
    /// Write the man pages and shell completions from the clap tree
    Generate {
        /// Directory that holds `man/` and `completions/`
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
}

fn main() -> Result<()> {
    match Task::parse() {
        Task::Generate { root } => generate(&root),
    }
}

/// Replaces the generated files under `root`; `completions/README.md` is kept.
fn generate(root: &Path) -> Result<()> {
    let mut cmd = dotman::cli::Cli::command().disable_help_subcommand(true);
    cmd.build();

    let man_dir = root.join("man");
    let completions_dir = root.join("completions");
    for dir in [&man_dir, &completions_dir] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    clear(&man_dir, |name| name.ends_with(".1"))?;
    clear(&completions_dir, |name| name != "README.md")?;

    write_page(&man_dir, cmd.clone())?;
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        write_page(&man_dir, sub.clone())?;
    }
    for shell in Shell::value_variants() {
        clap_complete::generate_to(*shell, &mut cmd.clone(), BIN, &completions_dir)?;
    }
    Ok(())
}

/// Trailing whitespace is stripped because the pre-commit hook would do it and the next run would differ.
fn write_page(dir: &Path, cmd: clap::Command) -> Result<()> {
    let man = Man::new(cmd.clone());
    let mut text = String::new();
    for line in page(&man, cmd)?.lines() {
        text.push_str(line.trim_end());
        text.push('\n');
    }
    let path = dir.join(man.get_filename());
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// The page `Man::render` writes, in its order, except for `after_long_help`, which it would put
/// under one EXTRA heading: a block of it that starts with a [`SECTIONS`] heading becomes that
/// section, after the subcommands, and any other block goes on the description.
fn page(man: &Man, mut cmd: clap::Command) -> Result<String> {
    cmd.build();
    let help = cmd
        .get_after_long_help()
        .or_else(|| cmd.get_after_help())
        .map(ToString::to_string)
        .unwrap_or_default();
    let mut prose = String::new();
    let mut sections = String::new();
    for block in help.split("\n\n").filter(|block| !block.trim().is_empty()) {
        let section = SECTIONS
            .iter()
            .find_map(|(heading, name)| block.strip_prefix(heading).map(|body| (name, body)));
        if let Some((name, body)) = section {
            sections.push_str(".SH ");
            sections.push_str(name);
            sections.push('\n');
            sections.push_str(&body_roff(body.trim_start_matches(' ')));
        } else {
            prose.push_str(".PP\n");
            prose.push_str(&body_roff(block));
        }
    }

    // Every renderer starts its output with the same `.ie`/`.el` preamble; the title keeps it.
    let mut text = rendered(|w| man.render_title(w))?;
    let mut add = |part: String| {
        let lines = part
            .lines()
            .skip_while(|l| l.starts_with(".ie ") || l.starts_with(".el "));
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
    };
    add(rendered(|w| man.render_name_section(w))?);
    add(rendered(|w| man.render_synopsis_section(w))?);
    add(rendered(|w| man.render_description_section(w))?);
    add(prose);
    if cmd.get_arguments().any(|arg| !arg.is_hide_set()) {
        add(rendered(|w| man.render_options_section(w))?);
    }
    if cmd.get_subcommands().any(|sub| !sub.is_hide_set()) {
        add(rendered(|w| man.render_subcommands_section(w))?);
    }
    add(sections);
    if cmd
        .get_version()
        .or_else(|| cmd.get_long_version())
        .is_some()
    {
        add(rendered(|w| man.render_version_section(w))?);
    }
    if cmd.get_author().is_some() {
        add(rendered(|w| man.render_authors_section(w))?);
    }
    Ok(text)
}

fn rendered(render: impl FnOnce(&mut dyn io::Write) -> io::Result<()>) -> Result<String> {
    let mut roff = Vec::new();
    render(&mut roff)?;
    Ok(String::from_utf8(roff)?)
}

/// Roff for the help text `body`, laid out as `cli.rs` writes it: a line at the margin is prose;
/// a line indented two spaces starts an entry, whose tag is either its first word, when two
/// spaces follow that word, or the line itself, joined with the lines indented two spaces below
/// it; and a line indented further continues the entry's text. Indented lines with nothing
/// further below them are prose too.
fn body_roff(body: &str) -> String {
    let lines: Vec<(usize, &str)> = body
        .lines()
        .map(|line| {
            let text = line.trim_start_matches(' ');
            (line.len().saturating_sub(text.len()), text.trim_end())
        })
        .filter(|(_, text)| !text.is_empty())
        .collect();
    let mut out = String::new();
    let mut entry = false;
    let mut rest = lines.as_slice();
    while let Some(((indent, text), after)) = rest.split_first() {
        let inline = text.split_once("  ").filter(|(tag, _)| !tag.contains(' '));
        if *indent >= 4 {
            out.push_str(&escape(text));
            rest = after;
        } else if let (2, Some((tag, first))) = (*indent, inline) {
            out.push_str(&tagged(tag));
            out.push('\n');
            out.push_str(&escape(first.trim_start()));
            entry = true;
            rest = after;
        } else if *indent == 2 {
            let run = after
                .iter()
                .take_while(|(indent, text)| *indent == 2 && !text.contains("  "))
                .count();
            let (tags, below) = rest.split_at(run.saturating_add(1));
            if below.first().is_some_and(|(indent, _)| *indent >= 4) {
                let tag: Vec<&str> = tags.iter().map(|(_, text)| *text).collect();
                out.push_str(&tagged(&tag.join(" ")));
                entry = true;
            } else {
                if entry {
                    out.push_str(".PP\n");
                    entry = false;
                }
                let text: Vec<String> = tags.iter().map(|(_, text)| escape(text)).collect();
                out.push_str(&text.join("\n"));
            }
            rest = below;
        } else {
            if entry {
                out.push_str(".PP\n");
                entry = false;
            }
            out.push_str(&escape(text));
            rest = after;
        }
        out.push('\n');
    }
    out
}

/// The `.TP` request and the bold tag line of an entry, without the final newline.
fn tagged(tag: &str) -> String {
    let mut out = String::from(".TP\n\\fB");
    out.push_str(&escape(tag));
    out.push_str("\\fR");
    out
}

/// `text` as one roff text line: a backslash, a hyphen and an apostrophe printed as themselves,
/// as `clap_mangen` prints them, each `` `code` `` span in bold, since groff would print its
/// backticks as opening quotes, and a leading `.` kept from reading as a request.
fn escape(text: &str) -> String {
    let literal = |text: &str| {
        text.replace('\\', "\\e")
            .replace('-', "\\-")
            .replace('\'', "\\*(Aq")
    };
    let paired = text.matches('`').count().is_multiple_of(2);
    let text: String = if paired {
        text.split('`')
            .enumerate()
            .map(|(n, part)| {
                if n.is_multiple_of(2) {
                    literal(part)
                } else {
                    format!("\\fB{}\\fR", literal(part))
                }
            })
            .collect()
    } else {
        literal(text).replace('`', "\\(ga")
    };
    if text.starts_with('.') {
        format!("\\&{text}")
    } else {
        text
    }
}

fn clear(dir: &Path, remove: impl Fn(&str) -> bool) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if remove(&entry.file_name().to_string_lossy()) {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn names(dir: &Path) -> BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn one_page_per_visible_subcommand_and_stale_files_are_removed() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("man")).unwrap();
        fs::write(root.path().join("man/dot-old.1"), "stale").unwrap();
        fs::create_dir_all(root.path().join("completions")).unwrap();
        fs::write(root.path().join("completions/README.md"), "keep").unwrap();
        fs::write(root.path().join("completions/dot-enhanced.bash"), "stale").unwrap();

        generate(root.path()).unwrap();

        let mut expected: BTreeSet<String> = dotman::cli::Cli::command()
            .get_subcommands()
            .filter(|s| !s.is_hide_set() && s.get_name() != "help")
            .map(|s| format!("{BIN}-{}.1", s.get_name()))
            .collect();
        expected.insert(format!("{BIN}.1"));
        assert_eq!(names(&root.path().join("man")), expected);
        for name in ["dotman-edit.1", "dotman-key.1"] {
            assert!(expected.contains(name), "{name}");
        }
        assert!(!expected.contains("dotman-system-write.1"));

        assert_eq!(
            names(&root.path().join("completions")),
            [
                "README.md",
                "dotman.bash",
                "_dotman",
                "dotman.fish",
                "dotman.elv",
                "_dotman.ps1"
            ]
            .map(String::from)
            .into()
        );
        let bash = fs::read_to_string(root.path().join("completions/dotman.bash")).unwrap();
        assert!(bash.contains("dotman__subcmd__edit") && bash.contains("dotman__subcmd__key"));
    }

    /// The exit codes and the environment get the sections man(7) names, after the subcommands,
    /// with one tagged paragraph per code and per variable; no page keeps an EXTRA section, and
    /// each has the preamble once.
    #[test]
    fn exit_status_and_environment_are_sections_of_their_own() {
        let root = tempfile::tempdir().unwrap();
        generate(root.path()).unwrap();
        let read = |name: &str| fs::read_to_string(root.path().join("man").join(name)).unwrap();

        let top = read("dotman.1");
        let at = |needle: &str| top.find(needle).unwrap();
        assert!(at(".SH SUBCOMMANDS") < at(".SH \"EXIT STATUS\""));
        assert!(at(".SH \"EXIT STATUS\"") < at(".SH ENVIRONMENT"));
        assert!(at(".SH ENVIRONMENT") < at(".SH VERSION"));
        assert!(top.contains(".TP\n\\fB4\\fR\n\\fBsync\\fR hit a rebase conflict, aborted the pull and printed the\nsteps to resolve it;"));
        assert!(
            top.contains(".PP\nThe \\fBgit\\fR subcommand exits with git\\*(Aqs own status.\n")
        );
        assert!(top.contains(".TP\n\\fBDOTMAN_REPO_PATH\\fR\nThe repo, as an absolute path."));
        assert!(top.contains(".TP\n\\fBDOTMAN_APPROVE_HOOKS, DOTMAN_BOOTSTRAP_BASE_URL\\fR\n"));

        let sync = read("dotman-sync.1");
        assert!(sync.contains(".SH \"EXIT STATUS\"\n.TP\n\\fB3\\fR\nPaths were left undecided"));
        let status = read("dotman-status.1");
        assert!(status.contains(".SH \"EXIT STATUS\"\n0 whenever the repo can be read"));
        let description = status.split(".SH OPTIONS").next().unwrap_or_default();
        assert!(description.contains(".TP\n\\fBheld\\-in\\-repo\\fR\nan uncommitted repo edit"));
        assert!(
            read("dotman-diff.1").contains("\n.PP\nThe diff runs from repo/<key> to home/<key>")
        );

        for name in names(&root.path().join("man")) {
            let page = read(&name);
            assert!(!page.contains(".SH EXTRA"), "{name}");
            assert_eq!(page.matches(".ie \\n(.g .ds Aq").count(), 1, "{name}");
            assert!(page.starts_with(".ie "), "{name}");
        }
    }

    #[test]
    fn help_text_becomes_tagged_paragraphs_and_prose() {
        assert_eq!(
            body_roff("\n  0  Done.\n     More.\n  The `rest`.\n.dot -x 'q' \\t `"),
            ".TP\n\\fB0\\fR\nDone.\nMore.\n.PP\nThe \\fBrest\\fR.\n\\&.dot \\-x \\*(Aqq\\*(Aq \\et \\(ga\n"
        );
        assert_eq!(
            body_roff("Tokens:\n  a, b,\n  c\n      the first\n  d\n      the second"),
            "Tokens:\n.TP\n\\fBa, b, c\\fR\nthe first\n.TP\n\\fBd\\fR\nthe second\n"
        );
    }
}
