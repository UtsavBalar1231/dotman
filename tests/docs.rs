//! The README names only commands and flags that exist, AGENTS.md names only paths that exist, and
//! every never, always or every claim in README.md and SECURITY.md names the tests that prove it.

use clap::{Arg, Command, CommandFactory};
use dotman::cli::Cli;
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

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

/// `--approve-hooks` exits 3 when the table changed since a warning showed it, so the exit 3 entry
/// of each man page whose command takes the flag says so.
#[test]
fn the_man_pages_name_unapproved_hooks_under_exit_3() -> R {
    for page in ["man/dotman.1", "man/dotman-sync.1"] {
        let text = read(page)?;
        let three = text
            .split_once(".SH \"EXIT STATUS\"")
            .and_then(|(_, status)| status.split_once("\\fB3\\fR"))
            .map(|(_, rest)| rest.split(".TP").next().unwrap_or_default())
            .ok_or_else(|| format!("{page} has no exit status 3"))?
            .replace('\n', " ");
        assert!(three.contains("hooks were not approved"), "{page}: {three}");
        assert!(
            three.contains("\\-\\-approve\\-hooks showed the new table and approved nothing"),
            "{page}: {three}"
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

/// The text of each code span and of each line in a fenced block.
fn code_spans(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if fenced {
            spans.push(line);
        } else {
            spans.extend(line.split('`').skip(1).step_by(2));
        }
    }
    spans
}

/// `--name` when `word` is a long flag, without a `=value` or trailing punctuation.
fn long_flag(word: &str) -> Option<&str> {
    let flag = word
        .split('=')
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    let name = flag.strip_prefix("--")?;
    (name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
    .then_some(flag)
}

fn longs(cmd: &Command) -> BTreeSet<String> {
    cmd.get_arguments()
        .filter_map(Arg::get_long)
        .map(|long| format!("--{long}"))
        .collect()
}

fn all_longs(cmd: &Command) -> BTreeSet<String> {
    let mut found = longs(cmd);
    for sub in cmd.get_subcommands() {
        found.extend(all_longs(sub));
    }
    found
}

/// Every `--flag` in a code span of the README exists: after `dotman <command>`, or a span that
/// starts with a command name, on that command; in a span of flags alone, on some command. Flags
/// after `dotman git` are git's, and spans without a dotman command, such as the cargo install
/// line, are someone else's.
#[test]
fn every_flag_the_readme_names_exists() -> R {
    let mut cli = Cli::command();
    cli.build();
    let anywhere = all_longs(&cli);
    let readme = read("README.md")?;
    let mut checked = 0_usize;
    for span in code_spans(&readme) {
        let words: Vec<&str> = span.split_whitespace().collect();
        let start = match words.first() {
            Some(first) if first.starts_with("--") => {
                for flag in words.iter().filter_map(|w| long_flag(w)) {
                    assert!(
                        anywhere.contains(flag),
                        "README names `{flag}`, which no command has: `{span}`"
                    );
                    checked = checked.saturating_add(1);
                }
                continue;
            }
            Some(first) if cli.find_subcommand(first).is_some() => 0,
            _ => match words.iter().position(|w| matches!(*w, "dotman" | "dot")) {
                Some(at) => at.saturating_add(1),
                None => continue,
            },
        };
        let mut scope = &cli;
        let mut rest = words.iter().skip(start).peekable();
        while let Some(sub) = rest.peek().and_then(|word| scope.find_subcommand(word)) {
            scope = sub;
            rest.next();
        }
        if scope.get_name() == "git" {
            continue;
        }
        let own = longs(scope);
        for flag in rest.filter_map(|w| long_flag(w)) {
            assert!(
                own.contains(flag),
                "README names `{flag}` for `{}`, which lacks it: `{span}`",
                scope.get_name()
            );
            checked = checked.saturating_add(1);
        }
    }
    assert!(checked > 40, "only {checked} flags were checked");
    Ok(())
}

/// Each never, always or every claim in README.md and SECURITY.md, by a piece of its sentence that
/// no other claim shares, with the tests that prove it.
const CLAIMS: &[(&str, &[&str])] = &[
    // README.md
    (
        "It never writes through a symlink, never runs a hook you have not approved",
        &[
            "a_target_symlink_is_never_written_through",
            "hostile_symlinks_never_carry_a_write_outside_home",
            "hooks_never_run_without_approval",
            "a_template_cannot_run_a_command",
        ],
    ),
    (
        "When `origin` has no branch yet",
        &["status_counts_the_commits_never_pushed_and_the_printed_sync_pushes_them"],
    ),
    (
        "Keeping `$HOME`'s version of a conflicting or existing file",
        &[
            "keep_local_in_sync_replaces_the_repo_version_for_every_machine_and_says_so",
            "the_way_to_keep_a_conflict_to_this_machine_is_offered_before_the_capture_and_works",
        ],
    ),
    (
        "A backup copy is never overwritten",
        &["a_file_backed_up_by_both_passes_of_a_sync_keeps_both_copies"],
    ),
    (
        "Backups are never pruned",
        &[
            "backups_are_never_pruned_by_later_syncs_and_applies",
            "nothing_in_src_deletes_from_the_backups_dir",
        ],
    ),
    (
        "which is meant for every machine, or when the repo's mode changed",
        &[
            "a_looser_mode_written_into_modes_by_hand_is_honored",
            "recorded_modes_are_applied_without_special_bits",
            "a_mode_loosened_on_another_machine_is_applied_and_not_kept_private",
            "a_stricter_mode_kept_on_one_machine_stays_on_that_machine",
        ],
    ),
    (
        "dotman never captures a local edit into a template",
        &["a_local_edit_is_template_drift_and_is_never_captured"],
    ),
    (
        "The prompt shows every command",
        &[
            "the_approval_prompt_shows_the_table_even_with_quiet",
            "approving_runs_a_command_once_for_two_changed_files",
        ],
    ),
    (
        "Never allowed: `/`, `/proc`",
        &[
            "the_never_allowed_list_bans_paths_and_a_root_of_slash",
            "never_allowed_keys_are_refused",
            "a_root_of_slash_is_reported_by_doctor_and_never_used",
        ],
    ),
    (
        "encrypted to every recipient in `.dotman.toml`",
        &[
            "one_keyed_machine_re_encrypts_and_the_next_finds_nothing_to_do",
            "an_imported_recipient_can_decrypt_after_a_keyed_machine_syncs",
        ],
    ),
    (
        "Plaintext is never written into the repo",
        &["the_plaintext_of_an_encrypted_file_never_reaches_the_repo"],
    ),
    (
        "stripped from every other child process",
        &[
            "the_age_key_is_removed_from_the_environment",
            "every_child_process_in_src_starts_here",
            "the_age_key_never_reaches_a_hook_or_the_editor",
            "hooks_and_the_editor_never_see_the_age_key",
        ],
    ),
    (
        "here or upstream, the next `sync` on a machine whose key decrypts every encrypted file",
        &[
            "an_imported_recipient_can_decrypt_after_a_keyed_machine_syncs",
            "one_keyed_machine_re_encrypts_and_the_next_finds_nothing_to_do",
        ],
    ),
    (
        "A finding names the rule and the line, never the value",
        &[
            "add_refuses_a_named_file_that_holds_a_credential",
            "a_directory_holds_a_file_that_holds_a_credential",
            "the_staging_guard_keeps_a_credential_out_of_the_commit",
            "the_line_is_where_the_match_starts_counted_from_one",
        ],
    ),
    (
        "The content guard runs on every plain capture",
        &[
            "add_refuses_a_named_file_that_holds_a_credential",
            "a_directory_holds_a_file_that_holds_a_credential",
            "a_local_edit_that_adds_a_credential_is_held",
            "imports_leave_out_files_that_hold_a_credential",
            "the_staging_guard_keeps_a_credential_out_of_the_commit",
            "a_repo_file_given_an_age_secret_key_by_edit_is_held_out_of_the_commit",
        ],
    ),
    (
        "An age identity is never stored in plain text",
        &[
            "an_age_secret_key_is_never_overridable",
            "add_refuses_a_plain_age_key_and_the_identity_files",
            "an_age_key_nested_in_another_credential_is_never_stored",
            "allow_entries_match_by_key_and_rule_and_never_cover_an_age_key",
        ],
    ),
    (
        "an entry another machine pushed never lets a credential out of this one",
        &["a_pushed_allow_entry_never_lets_a_credential_out_of_another_machine"],
    ),
    (
        "It never modifies the source and reports every entry",
        &[
            "a_legacy_import_leaves_its_source_unchanged",
            "a_yadm_import_leaves_its_source_unchanged",
            "a_chezmoi_import_leaves_its_source_unchanged_and_runs_nothing",
            "a_farm_import_and_swap_leave_the_farm_unchanged",
            "an_overlay_import_leaves_its_source_unchanged",
            "yadm_alternates_become_one_commit_and_the_accounting_holds",
            "the_report_counts_each_entry_once",
        ],
    ),
    (
        "converts every commit, keeping parents, author, time and message",
        &[
            "import_rebuilds_the_history_and_matches_the_readme_manifest",
            "commits_come_parents_first_and_a_merge_keeps_its_parent_order",
        ],
    ),
    (
        "the first sync would capture every file in those directories",
        &[
            "import_lists_the_tracked_directories_and_reports_what_it_leaves_behind",
            "tracked_directories_go_through_the_key_rules_and_are_only_reported",
            "overlay_names_the_tracked_directories_and_leaves_dirs_alone",
        ],
    ),
    (
        "`--keep-history` imports every commit anyway",
        &[
            "keep_history_imports_a_credential_history_with_a_warning",
            "a_history_that_holds_a_credential_is_squashed_into_its_current_tree",
        ],
    ),
    (
        "Templates never run by default",
        &[
            "a_default_import_keeps_modes_and_runs_neither_templates_nor_scripts",
            "a_hostile_source_cannot_make_chezmoi_run_anything_by_default",
        ],
    ),
    // SECURITY.md
    (
        "A hostile repo, import source or system-write plan must never cause",
        &[
            "hostile_symlinks_never_carry_a_write_outside_home",
            "a_system_path_outside_the_roots_is_refused",
            "a_target_symlink_is_never_written_through",
            "special_mode_bits_are_stripped_on_apply",
            "a_system_mode_of_6755_is_written_as_0755",
            "hooks_never_run_without_approval",
            "a_template_cannot_run_a_command",
            "row_9_changed_locally_is_reported_and_never_overwritten",
        ],
    ),
    (
        "refuses a symlinked parent, never writes through a symlink at the target",
        &[
            "invalid_keys_are_rejected",
            "a_parent_symlink_pointing_outside_is_refused",
            "a_target_symlink_is_never_written_through_but_can_be_replaced_by_a_link",
            "special_bits_are_masked_and_modes_are_exact",
            "a_target_changed_after_classification_is_a_conflict",
            "row_10_changed_in_repo_is_backed_up_before_it_is_written",
        ],
    ),
    (
        "Every git child process runs with the isolation flags",
        &[
            "user_global_excludes_do_not_hide_files",
            "repo_hooks_do_not_run",
            "ignored_crlf_and_text_auto_content_survives_commit_and_clone",
            "status_ignores_an_fsmonitor_hook_and_submodules",
            "status_reports_renamed_paths",
            "inherited_git_variables_never_redirect_sync",
            "planted_git_hooks_never_run",
            "the_user_s_git_config_and_template_dir_never_shape_the_checkout",
        ],
    ),
    (
        "and every clone write `.git/info/attributes`",
        &[
            "init_sets_main_attributes_and_stable_id",
            "ignored_crlf_and_text_auto_content_survives_commit_and_clone",
            "a_tracked_merge_driver_never_joins_a_conflict",
            "a_merge_driver_in_the_user_s_git_config_never_settles_a_conflict",
        ],
    ),
    (
        "always uses git's built-in text merge, whatever `merge.<name>.driver`",
        &[
            "a_merge_driver_in_the_user_s_git_config_never_settles_a_conflict",
            "a_tracked_merge_driver_never_joins_a_conflict",
        ],
    ),
    (
        "writes the line again before it changes anything",
        &[
            "an_attributes_line_an_older_dotman_wrote_is_rewritten_before_the_next_write",
            "an_attributes_line_an_older_dotman_wrote_warns_with_a_step_that_works",
            "only_context_lock_takes_the_lock_to_write",
        ],
    ),
    (
        "in every spelling git refuses on NTFS",
        &[
            "a_git_component_is_refused_at_any_depth",
            "a_repo_path_with_a_git_component_is_never_applied",
        ],
    ),
    (
        "so the user's chezmoi config and its hooks are never read",
        &[
            "the_users_chezmoi_hooks_do_not_run_unless_templates_are_rendered",
            "a_hostile_source_cannot_make_chezmoi_run_anything_by_default",
        ],
    ),
    (
        "written by a helper that checks everything again",
        &[
            "hand_made_plans_are_rejected_with_no_write",
            "every_kind_of_bad_entry_is_refused_before_any_write",
            "a_bad_plan_dir_config_or_field_is_refused_with_no_write",
            "a_plan_at_0644_or_behind_a_symlink_is_refused",
            "a_group_writable_config_is_refused",
        ],
    ),
    (
        "is not on the never-allowed list",
        &[
            "every_kind_of_bad_entry_is_refused_before_any_write",
            "the_never_allowed_list_bans_paths_and_a_root_of_slash",
        ],
    ),
    (
        "Every other child process, including git",
        &[
            "the_age_key_is_removed_from_the_environment",
            "every_child_process_in_src_starts_here",
            "the_age_key_never_reaches_a_hook_or_the_editor",
            "hooks_and_the_editor_never_see_the_age_key",
        ],
    ),
    (
        "so decrypted content never sits in the repo",
        &["the_plaintext_of_an_encrypted_file_never_reaches_the_repo"],
    ),
    (
        "When `[age] recipients` in `.dotman.toml` changes",
        &[
            "an_imported_recipient_can_decrypt_after_a_keyed_machine_syncs",
            "one_keyed_machine_re_encrypts_and_the_next_finds_nothing_to_do",
        ],
    ),
    (
        "read every encrypted file after that sync",
        &["an_imported_recipient_can_decrypt_after_a_keyed_machine_syncs"],
    ),
    (
        "every file `sync` is about to commit",
        &[
            "add_refuses_a_named_file_that_holds_a_credential",
            "a_directory_holds_a_file_that_holds_a_credential",
            "a_local_edit_that_adds_a_credential_is_held",
            "imports_leave_out_files_that_hold_a_credential",
            "the_staging_guard_keeps_a_credential_out_of_the_commit",
            "a_repo_file_given_an_age_secret_key_by_edit_is_held_out_of_the_commit",
        ],
    ),
    (
        "dotman never reads one from the repo",
        &[
            "a_pushed_allow_entry_never_lets_a_credential_out_of_another_machine",
            "allow_plain_is_recorded_by_path_and_rule_never_by_line",
        ],
    ),
    (
        "to every machine and remote the repo reaches",
        &[
            "allow_plain_is_recorded_by_path_and_rule_never_by_line",
            "a_second_device_gets_what_the_first_synced",
        ],
    ),
    (
        "Everything else about the helper is tested unprivileged",
        &[
            "a_valid_plan_writes_the_file_with_its_mode_and_skips_the_owner",
            "hand_made_plans_are_rejected_with_no_write",
            "every_kind_of_bad_entry_is_refused_before_any_write",
            "the_helper_refuses_a_user_unless_escalate_is_none",
        ],
    ),
];

/// Claims that no test proves yet. Each one is a test to write, not a sentence to soften.
const GAPS: &[&str] = &[
    // Each prompt in the table has a test for its flag; none checks that the table lists them all.
    "Every prompt has a flag, so unattended runs never guess",
    // Only the git layer's missing-git error is tested, not each command without git.
    "every command but `completion` runs it",
    // The sentence itself says that this path has never run.
    "`restorecon` runs after a system write",
];

/// The prose of `text` as sentences, whitespace collapsed: headings, list items and table rows are
/// units of their own, paragraphs are joined, and fenced blocks are left out. A sentence ends at
/// `.`, `?` or `!` before whitespace, so `.dotman.toml` and `0.1.0` stay whole.
fn sentences(text: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            fenced = !fenced;
        }
        let alone = line.starts_with('#') || line.starts_with('|');
        let new_unit =
            line.is_empty() || alone || line.starts_with("- ") || line.starts_with("```");
        if new_unit && !current.is_empty() {
            units.push(current.join(" "));
            current.clear();
        }
        if fenced || line.is_empty() || line.starts_with("```") || line.starts_with("|-") {
            continue;
        }
        if alone {
            units.push(line.to_owned());
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        units.push(current.join(" "));
    }
    let mut found = Vec::new();
    for unit in units {
        let unit = unit.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut sentence = String::new();
        let mut chars = unit.chars().peekable();
        while let Some(c) = chars.next() {
            sentence.push(c);
            let end = matches!(c, '.' | '?' | '!') && chars.peek().is_none_or(|n| *n == ' ');
            if end {
                found.push(sentence.trim().to_owned());
                sentence.clear();
            }
        }
        if !sentence.trim().is_empty() {
            found.push(sentence.trim().to_owned());
        }
    }
    found
}

/// Whether a sentence says never, always, or every, everything and the like.
fn is_claim(sentence: &str) -> bool {
    sentence
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .any(|word| word == "never" || word == "always" || word.starts_with("every"))
}

fn claims() -> Result<Vec<(&'static str, String)>, Box<dyn Error>> {
    let mut found = Vec::new();
    for doc in ["README.md", "SECURITY.md"] {
        found.extend(
            sentences(&read(doc)?)
                .into_iter()
                .filter(|sentence| is_claim(sentence))
                .map(|sentence| (doc, sentence)),
        );
    }
    Ok(found)
}

/// The name of each function marked `#[test]` under `src/` and `tests/`, proptest ones included.
fn test_functions() -> Result<BTreeSet<String>, Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut names = BTreeSet::new();
    for dir in ["src", "tests"] {
        for entry in WalkDir::new(root.join(dir)) {
            let entry = entry?;
            if entry.path().extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let mut marked = false;
            for line in fs::read_to_string(entry.path())?.lines() {
                let line = line.trim();
                if line == "#[test]" {
                    marked = true;
                    continue;
                }
                if !marked || line.starts_with("#[") || line.starts_with("//") {
                    continue;
                }
                marked = false;
                let name: String = line
                    .strip_prefix("fn ")
                    .unwrap_or_default()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    names.insert(name);
                }
            }
        }
    }
    Ok(names)
}

#[test]
fn every_claim_in_the_docs_names_the_tests_that_prove_it() -> R {
    let claims = claims()?;
    assert!(claims.len() > 30, "the extractor found only {claims:?}");
    let keys = || {
        CLAIMS
            .iter()
            .map(|(key, _)| *key)
            .chain(GAPS.iter().copied())
    };
    let unmapped: Vec<String> = claims
        .iter()
        .filter(|(_, sentence)| !keys().any(|key| sentence.contains(key)))
        .map(|(doc, sentence)| format!("{doc}: {sentence}"))
        .collect();
    assert!(
        unmapped.is_empty(),
        "these claims have no entry in CLAIMS or GAPS:\n{}",
        unmapped.join("\n")
    );
    for key in keys() {
        let hits = claims
            .iter()
            .filter(|(_, sentence)| sentence.contains(key))
            .count();
        assert!(
            hits > 0,
            "`{key}` matches no claim in README.md or SECURITY.md"
        );
    }
    let tests = test_functions()?;
    assert!(tests.len() > 500, "found only {} tests", tests.len());
    for (key, proofs) in CLAIMS {
        assert!(!proofs.is_empty(), "`{key}` names no test; put it in GAPS");
        for proof in *proofs {
            assert!(
                tests.contains(*proof),
                "`{key}` names {proof}, which is no #[test] under src/ or tests/"
            );
        }
    }
    Ok(())
}

/// A file under `src/` by its path from the crate root, with its lines.
type Source = (String, Vec<String>);

/// Each `.rs` file under `src/` with the lines before its `#[cfg(test)]` module, comments left out.
fn source_lines() -> Result<Vec<Source>, Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for entry in WalkDir::new(root.join("src")).sort_by_file_name() {
        let entry = entry?;
        if entry.path().extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let name = entry
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .into_owned();
        let lines = fs::read_to_string(entry.path())?
            .lines()
            .map(str::trim)
            .take_while(|line| *line != "#[cfg(test)]")
            .filter(|line| !line.starts_with("//"))
            .map(str::to_owned)
            .collect();
        files.push((name, lines));
    }
    Ok(files)
}

/// Every call under `src/` that deletes a file or a directory, by file, with what it deletes;
/// tempfile's own cleanup removes only what it created. None reaches the backups directory, so
/// nothing prunes it. A new deletion needs a row that says what it deletes.
const DELETIONS: &[(&str, usize, &str)] = &[
    (
        "src/commands/add.rs",
        2,
        "the plain repo source that `add --encrypt` replaces, and add's own temp link in the repo",
    ),
    (
        "src/commands/forget.rs",
        2,
        "a forgotten source in the repo, and the repo directories that leaves empty",
    ),
    (
        "src/commands/init.rs",
        1,
        "the staging directories a killed init left beside the repo",
    ),
    (
        "src/context.rs",
        2,
        "leftover `.dotman-tmp-` entries, a name no key and so no backup copy can have",
    ),
    (
        "src/engine/write.rs",
        3,
        "the directory `import --swap` staged, and the link or tree it set aside",
    ),
    (
        "src/import/gittree.rs",
        6,
        "the import's scratch history, files it leaves out or renames, and the repo it replaces",
    ),
    (
        "src/sys.rs",
        2,
        "the `unlinkat` wrapper that the helper calls",
    ),
    (
        "src/system/escalate.rs",
        1,
        "the plan directory under `system-plan/`",
    ),
    (
        "src/system/helper.rs",
        1,
        "the helper's own temp file beside a system target",
    ),
];

#[test]
fn nothing_in_src_deletes_from_the_backups_dir() -> R {
    let mut found: Vec<(String, usize)> = Vec::new();
    for (name, lines) in source_lines()? {
        let deletions = lines
            .iter()
            .filter(|line| {
                [
                    "remove_file(",
                    "remove_dir(",
                    "remove_dir_all(",
                    "unlinkat(",
                ]
                .iter()
                .any(|call| line.contains(call))
            })
            .count();
        if deletions > 0 {
            found.push((name, deletions));
        }
    }
    let listed: Vec<(String, usize)> = DELETIONS
        .iter()
        .map(|(name, count, _)| ((*name).to_owned(), *count))
        .collect();
    assert_eq!(
        found, listed,
        "a deletion was added or removed; say in DELETIONS what it deletes, after checking that it cannot reach the backups"
    );
    Ok(())
}

/// The lock is taken to write only by `Context::lock`, which rewrites a stale
/// `.git/info/attributes`; every other `Lock::acquire` is a dry-run probe, which writes nothing.
#[test]
fn only_context_lock_takes_the_lock_to_write() -> R {
    let mut writing = Vec::new();
    for (name, lines) in source_lines()? {
        for line in lines.iter().filter(|line| line.contains("Lock::acquire(")) {
            if !line.contains(", true)") {
                writing.push(format!("{name}: {line}"));
            }
        }
    }
    assert_eq!(
        writing,
        ["src/context.rs: let lock = Lock::acquire(&self.paths.lock_file(), dry_run)?;"]
    );
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
