# Experimental status JSON

`dotman status --json` is a read-only, lock-free client interface. It conflicts with
`--porcelain`; the ordinary human and porcelain formats are unchanged.

## Framing and version

Stdout is newline-delimited JSON, one object per line. A successful report starts
with `context`, carrying `schema_version: 1` and `experimental: true`. This version
is independent of the state-file version. Clients must reject unsupported schema
versions; incompatible changes can increment it while the interface is experimental.
Additive fields can appear within a version.
Split frames on LF bytes, not Unicode line boundaries: raw metadata can contain
U+2028 or U+2029.

After path/config resolution, `context` is emitted even when the guard or repository
identity validation fails. Its `repo_id` is a raw/shown text value on success, or
explicitly `null` (unavailable) when either validation fails. The original validation
error still determines stderr and the final result; a null identity is not a
fabricated ID or an empty successful report. This lets Setup show the resolved repo
path before a repo exists.

`main` owns the single final `result` record, after mapping the command result to
its exit code. The process exit is authoritative. A missing final record means the
report was interrupted or could not be written. Parsing failures in clap (exit 2)
happen before this protocol starts. Root refusal or failure before resolution
produces an error result without invented context paths or an empty success report.
The in-progress-operation guard still applies (exit 4).

| Record | Fields |
| --- | --- |
| `context` | Version, experimental marker, command, resolved `home`, `repo`, machine `config`, `repo_config`, `state`, age `identity` and optional `age_key_file` paths; `repo_id`; current `machine` identity |
| `entry` | `status`, `key`, `scope`, optional repo-source `path`; source and recorded source; encrypted/template/system flags; underlying row, row number and prompt category; observations; identity-change/update-state flags; kept and repo modes; alternative and invalid sources; warnings, hold, finding and remedies |
| `review_context` | Hooks; unapproved directories and waiting counts/keys; current and recorded identities; retained ignore patterns and affected entries; remote position; warnings and non-UTF-8 untracked repo paths |
| `summary` | Row and clean counts, new and held counts (including separate home/repo holds), and clean rows whose state needs recording |
| `result` | `outcome` (`done` or `error`), mapped integer `exit_code`, nullable `error_kind` (`root_refused`, `in_progress`, or `other`) |

Entry projection preserves every porcelain `(status, key)` occurrence, including
both `changed-locally` and `held-in-home` for the same key. Do not deduplicate entries
by key. `scope` is `home`, `system`, or `repo_source`. Collision occurrences name
target keys in home/system scope; held-repo occurrences name repo sources.
Path filtering has porcelain's scope.
Summary `rows` excludes supplemental new/held occurrences.

## Text, paths and observations

Text metadata uses `{raw, shown}`. `shown` is exactly `output::escape_line(raw)`;
render it as text, not markup. It is presentation, not a shell argument.
UTF-8 paths use the same fields. A non-UTF-8 Unix path instead has `bytes` (an array
of byte values) and `shown` (escaped replacement-character display), with no `raw`.
Reconstruct executable paths from `raw` or `bytes`, not from `shown`.

An observation has one of these forms:

- `{"state":"present","tuple":{"kind":"file","hash":"…","mode":"0600"}}`
- `{"state":"missing"}`
- `{"state":"blocked","reason":{"raw":"directory","shown":"directory"}}`
- `{"state":"unavailable","reason":"locked"}`

Tuple kinds are `file` and `symlink`. Hashes are exactly 32 lowercase hex digits
of xxh3-128, not cryptographic identities. Modes are canonical four-digit octal
strings. `r` is the effective rendered/decrypted repo tuple after kept-mode
adjustment. `source_tuple` is that tuple before kept-mode adjustment, not a hash
of template source text or ciphertext. `h` is the portable comparison observation;
`raw_h` is the separately retained observation the write-time binding uses.
An absolute symlink into this home can give these two H observations different
hashes. `s` is the recorded tuple, with `recorded_source` identifying its source.

Observations come from classification, without rereading home to fill the report.
Locked, unreadable, ambiguous and invalid entries are not represented as missing.
`unavailable` can also mean `not_observed` (classification stopped before reading
that side, or a supplemental scan occurrence has no classification snapshot),
`template_error`, or `invalid_recorded_hash`. A blocked parent names the parent
in its reason. Underlying row/category are null when classification cannot finish.

## Review context

Hooks include `standing` (`empty`, `approved`, `unapproved`, `changed`,
`old_format`, or `invalid`), the exact canonical ordered-table JSON bytes as a
text value, explicit pattern/command entries, and managed pending keys and the
deduplicated commands approval would release. Canonical comparison is byte for
byte, not a digest. Invalid tables, including malformed globs, release no commands.
Reading status grants
no approval and executes no hook.

Unapproved directories are listed even when their waiting count is zero. Retained
ignore patterns are the patterns this machine still applies that the repo no
longer lists; affected entries include their pattern and directory flag. Identity
fields include host, user, OS, distro, like, architecture and profiles.

Remote `basis` is `last_fetched`, `not_fetched`, or `unknown`. Ahead/behind counts
are nullable; no network fetch occurs. With origin refs but no ref for the current
branch, ahead counts commits absent from origin and behind is unknown.

Findings contain only `rule`, one-based `line`, and `place` (`content`, `name`,
or `target`), with remedies separate. Payloads contain no file contents, decrypted
bytes, template variable/environment values, matched credential text, or formatted
template error chains. This is a no-file-content guarantee, not a promise that
literal metadata is secret-free: names, identities and hook command lines can
themselves contain secrets. Stderr retains the CLI's diagnostics and is not this
schema. Context warnings include a raw key when available and a display diagnostic;
per-entry warnings and scan notes are text metadata.

[Command outcomes, outgoing review and terminal recovery](outcomes-json.md)
have a separate record set. [Reviewed decisions](decisions-json.md) consume these
snapshots; diff JSON remains separate work.
Status does not authorize a later write.

## Timing check

The ignored integration test `status_json_10k_same_tree_benchmark` creates 10,000
plain synthetic keys in the hermetic harness. Run it with
`cargo test --release --test cli_status_json status_json_10k_same_tree_benchmark -- --ignored --nocapture`.
It captures stdout, discards a warm round, alternates modes, prints seven samples
per mode and compares medians on that same tree. The acceptance bound is JSON
at most twice porcelain. Actual measurements belong in execution evidence, not
an unmeasured product-performance claim.
