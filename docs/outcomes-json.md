# Experimental command outcomes

`dotman apply --json`, `dotman sync --json` and `dotman add --json` emit
schema-versioned NDJSON on stdout. Human diagnostics and child output go to stderr.
The flags do not grant approvals or change the ordinary command's decisions.

## Framing and results

Use the [status JSON framing, version, text, path and observation rules](status-json.md).
Split on LF bytes; raw metadata may contain Unicode line separators. Resolved
`context` carries `schema_version: 1`, `experimental: true`, the command and paths,
and a nullable repository identity. Failures before resolution may have no context.
Clap usage errors occur before this protocol starts.

`main` emits one terminal `result`, after mapping the command's result. The process
exit is authoritative. A missing terminal record means interruption or output
failure, not success. Refresh `status --json` after the child exits, including after
a failed or interrupted writer.

| Field | Meaning |
| --- | --- |
| `outcome` | `done` for exit 0; `error` otherwise, including exit 3 |
| `exit_code` | Mapped integer: 0 success, 1 failure, 3 attention, 4 recovery needed |
| `error_kind` | Nullable; `root_refused`, `lock_held`, `in_progress`, `pull_conflict`, `mass_delete`, or `other` |
| `recovery` | Optional ordered terminal recipe, described below |

Outcome observations never include file contents or decrypted bytes. Reports also
exclude template/environment values, matched content and arbitrary error chains.
Metadata is not secret-free: paths, commit subjects and literal hook commands can
contain secrets. Stderr is outside this schema and can include a requested diff.

## Progress and decisions

| Record | Fields |
| --- | --- |
| `stage` | `stage`, `dry_run` |
| `undecided` | `reason`, nullable text `key`, `scope`, `pass`, `attention`, `skip_next_pass` |
| `resolved` | Text `key`, `scope`, `effect`, `hypothetical` |
| `refusal` | `kind` (`push` or `render`), nullable `source` path and text `key` |
| `hooks` | Text `canonical`, command lists `commands` and `pending_commands`, `pending_keys`, `approved`, `unshown`, `dry_run` |
| `hook` | Nullable text `command`, `outcome`, nullable integer `exit_code` |

Stages identify reached work, not completed transactions. Current stages include
`add`, `capture`, `apply`, `commit`, `pull`, `reencrypt`, `hooks`, `push`,
`expected_hooks_current_repo` and `blocked_by_repo_hold`.

Undecided records retain occurrences across passes. `scope` identifies `home`,
`system`, `repo_source`, `context` or `hooks`. `pass` identifies `capture`, `apply`
or the emitting command. `attention` and `skip_next_pass` are independent: an
unreadable capture can be skipped without attention; a refused tracked source
can require attention without a skip. Locked sources, unapproved directories,
retained ignores and system-off entries can be report-only. A newly held file
under an unapproved directory separately requires attention. A null key is a
context-wide condition, not a missing filename.

Reasons include row/category decisions, `ambiguous`, `invalid_source`,
`identity_changed`, `template_drift`, `forgotten_edited`, `refused_tracked`,
`capture_unreadable`, `capture_held`, `capture_shape`, `capture_own_path`,
`new_held`, `write_skipped`, `changed_during_write`, `locked`,
`directory_approval`, `unlisted_ignore`, `system_off`, `system_refused`,
`system_unreadable`, `system_confirmation`, `repo_held`, `hook_table_unshown`,
and add's `name_held`, `content_held`, `special_held`, `system_root_approval`.
Reasons do not replace the booleans or process exit.

Resolved effects are `captured`, `written`, `forgotten` or `kept`.
`hypothetical: true` describes the current dry-run plan, not a completed mutation.
For decision parity, compare the still-unresolved evaluated keys with refreshed
status, accounting for resolved effects and supplemental status warnings.
`blocked_by_repo_hold` means sync stopped before home planning: those records name
the repository blocker, not a complete home decision set.

Hook outcomes are `disabled`, `would_run`, `would_hold`, `unshown`, `unapproved`,
`started`, `succeeded`, `failed` or `spawn_failed`. Approval compares the canonical
table itself, not a digest. Pending keys and commands describe what approval would release.
`unshown` can require attention even when no command is due. A dry-run records
neither approval nor pending state and executes no hook.

## Outgoing review

For example:

```sh
dotman sync --dry-run --json
dotman sync --dry-run --json --keep-local
```

The review concerns this machine's outgoing changes. It does not fetch or predict
incoming changes. Incoming conflicts still stop for review; an upstream hook-table
change needs approval, and recipient changes can require a later re-encryption commit.

| Record | Fields |
| --- | --- |
| `outgoing` | Nullable text `head`, `tracking_ref`, `tracking_commit`; `knowledge: "last_fetched"`; symbolic `warnings` |
| `ahead_commit` | Text `id`, `subject`; `paths` |
| `capture` | Text `key`, `source` path, `scope`; observations `logical_r`, `raw_h`, `portable_h`, `stored_before`, `stored_after`; nullable octal `kept_mode`, `repo_mode`; `hypothetical`, `encrypted` |
| `repo_edit`, `own` | `path`, final-index `stored` observation, nullable physical `observed` observation, `basis`, `remove_tracked_temporary` |
| `message` | Text `message`, generated or supplied by the caller |

Without a tracking ref, the review lists the commits on HEAD. An unborn HEAD is
null and has no ahead commits. `own` identifies dotman's settings, internal files
and bootstrap script; it is not a home-key decision.

Capture distinguishes the pre-capture logical repo value from raw home, portable
home and stored source. A portable symlink can differ from raw home. Stored template
text and ciphertext are not rendered/decrypted tuples. Hypothetical encryption
uses `{"state":"unavailable","reason":"hypothetical_ciphertext"}` rather than an
invented ciphertext hash.

The outgoing path set compares the projected resulting index with HEAD. It
includes deterministic settings changes and removal of all forgotten variants.
Exclusions prevent staging new worktree changes but retain already staged changes;
held paths are reset for the commit. A worktree edit that cancels an index edit
has no outgoing record. This projection writes no index or Git object.

`basis` is `worktree`, `projected`, `index`, `held_reset` or `temporary_removal`.
`stored` uses Git's executable/non-executable modes (0755/0644). With effective
`core.filemode` enabled, Git uses the owner execute bit. When disabled, staging
retains an existing regular index entry's mode and gives a new regular entry 0644.
For `worktree`, `observed` retains the same-read physical mode; it is null for the
other bases. Capture's `stored_after` describes the source written before staging,
so its physical mode can differ from the final-index `stored` mode.
Regular file/directory replacements remove the displaced tracked leaf entries;
symlinked parents and unreadable or special nodes are not treated as replacements.
Missing means a deletion; unavailable is not missing.
The experimental review supports file and symlink entries. An encountered outgoing
gitlink fails closed with exit 1 rather than inventing a file tuple.
A directory displacing a tracked leaf is also refused if it contains `.git`
metadata, including invalid files, empty directories and symlinks.

Hook planning uses current-repository writes and managed pending keys after
hypothetical forgetting. Logical post-capture source/state snapshots also let
the apply planner include follow-up writes, such as tightening a loosened
encrypted file. A capture with no subsequent home write releases no hook by itself.
Planning cannot predict hooks due to incoming changes.
The review alone does not authorize a later write; use
[tuple-bound reviewed decisions](decisions-json.md). Public diff JSON is separate work.

## Terminal recovery

Recovery is copied to a terminal, not executed automatically by a client. Preserve
order, manual instructions and conditions; commands can be alternatives.

- `{"kind":"instruction","text":{"raw":"…","shown":"…"}}` describes manual work.
- `{"kind":"command","argv":["dotman","git","…"],"shown":["…"],"when":null}`
  contains unquoted argv, including the program.
- A non-UTF-8 argv element is `{"bytes":[…]}`, retaining its exact Unix bytes.
- `when` states a condition such as resolving a stop or an abort having failed.
  It is not an extra argument.

Use `argv`, not `shown`, to reconstruct a command. Explicit Git path arguments
carry `:(literal)` pathspecs; `--` alone does not prevent wildcard expansion.
Filesystem removal paths, refs and generic Git passthrough arguments are not
pathspec-prefixed. Printed recipes describe the same action, using an octal
`printf` pipeline for names that cannot be pasted literally. Rebase recipes retain
the selected merge strategy and the observed branch recovery.

After terminal recovery, discard stale review decisions and refresh status.
