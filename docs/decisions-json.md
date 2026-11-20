# Experimental reviewed decisions

`dotman apply --decide-from -` and `dotman sync --decide-from -` read one JSON
document from stdin. Add `--json` for [command outcome records](outcomes-json.md).
This input schema is experimental, version 1; it is distinct from `state.toml`.

The CLI never starts a child before parsing and checking the syntax of the complete document.
Unknown fields, duplicate fields or entries, invalid paths or tuples, trailing
input and unsupported versions fail with exit 1. Per-key decisions naming
unmanaged, system or dotman's own keys reject the batch before writer effects,
including entries outside an apply path filter. Semantic checks such as managed-key
membership can use read-only Git children; they finish before writer effects.

`--decide-from` conflicts with the bare file-decision and approval flags,
`--system` and `--yes`. Stdin is reserved for the document, not prompt answers.
`--dry-run` validates and projects the decisions without changing files or approvals.

## Envelope

```json
{
  "schema_version": 1,
  "repo_id": "repository ID from context.repo_id.raw",
  "home": {"raw": "/home/example"},
  "repo": {"raw": "/home/example/.local/share/dotman"},
  "machine": {
    "host": "example", "user": "example", "os": "linux",
    "distro": "", "like": "", "arch": "x86_64", "profiles": []
  },
  "files": [],
  "approvals": {}
}
```

Copy context values from [status JSON](status-json.md), stripping text wrappers
to their `raw` strings. Input paths contain exactly `{"raw":"…"}` or
`{"bytes":[…]}`; omit `shown`. The resolved home/repo bytes, repository identity
and machine identity must match the writer. Context or reviewed HEAD changes
stop the run with exit 3.

Each file decision has required fields `key`, nullable `source`, `r`,
`source_tuple`, `h`, `raw_h` and `action`. Use the observations from the
classified home entry, not a supplemental hold occurrence; for a blocked
observation, use its reason's `raw` string rather than the output text wrapper.
Actions are
`take_repo`, `keep_local`, `forget` and `restore`; the row must support the
choice. `forget` answers a local deletion, not an arbitrary unmanaged path.

Observations use `present` with a full tuple, `missing`, `unavailable` with a
string reason, or `blocked` with a string reason. Tuple kinds are `file` and
`symlink`; hashes are 32 lowercase hexadecimal xxh3-128 digits and modes are four
octal digits. Symlink mode is `0777`. Unavailable and blocked observations do
not authorize a file write. Missing is a separate observation.

The source name, rendered/decrypted R, pre-kept-mode source tuple, portable H
and raw H must match. A stale file answer stays undecided while independent
matching answers can proceed. Template drift requires a matching repo choice.
The write engine also checks the home snapshot before replacing it.

## Approvals

Optional members of `approvals` are:

| Member | Required fields and comparison |
| --- | --- |
| `identity` | Nullable `recorded` machine and `current` machine, each with the envelope's machine fields |
| `hooks` | Exact canonical table string `canonical`, ordered `commands`, and set `pending_keys` |
| `directories` | Complete set of unapproved directory strings |
| `ignores` | Set `retained` and ordered list `current` of ignore patterns |
| `mass_delete` | Complete deletion set as input path objects |

Use status's review context for identity/directories/ignores and the outgoing
review's `hooks` record for the commands due in that plan. A stale supplied hook
token blocks execution even if the table already has standing approval.
It never falls back to standing approval for a stale supplied token.
Canonical bytes are the table itself, not a digest. Approval covers command
text, not scripts it calls. An approved hook can change and commit repository
content; that is trusted code execution.

For pending hooks alone, `apply --review-hooks --decide-from -` accepts no file
decisions or other approvals. Copy `canonical`, `pending_keys`, and
`pending_commands` from status's `review_context.hooks`, using the latter as
`commands`. It approves the exact table and runs those pending commands without
applying files or changing identity. It requires an existing lock file and
refuses if it cannot acquire that lock. A dry run does not approve or execute.

For directory or retained-ignore trust alone on Linux, first run
`apply --review-trust directories --dry-run --json` or
`apply --review-trust ignores --dry-run --json`. Copy the `review` object from
the `trust_review` record unchanged into the envelope as `trust`, then submit
it with the same `--review-trust` value and `--decide-from -`. The envelope
must have no file decisions, sync review or other approvals. A reviewed
`--dry-run` checks the token without granting it.

The `trust` object includes `paths` with `state`, `machine_config`, and
`repo_config` members. Each is either `{"raw":"..."}` for a UTF-8 path or
`{"bytes":[...]}` for a non-UTF-8 path. `lock_dev` and `lock_ino` are the
device and inode numbers of the held lock. Copy these fields unchanged.

Each control-file observation has `kind`, `hash`, `mode`, `dev`, `ino`, `uid`,
`gid`, and `nlink`. A successful review record carries
`"display_policy":"all_values_scanned_v1"` only after its display inputs pass
the policy checks. Consumers must reject a missing or unknown value.

`inventory_hash` binds the observed repository inventory, parent directories,
and the directory scan before and after the proposed trust change. It is a
32-character hexadecimal digest. The scan refuses trees beyond its file count,
byte, depth and review-size limits; it also refuses repository files whose
source is not recorded in local state. No partial grant is made on refusal.
State with extended attributes or ACLs is refused rather than rewritten.

The trust lane limits decision input and the emitted review to 1 MiB each,
individual state and configuration files to 8 MiB, and the repository ID read
to 128 bytes (the ID itself must be 32 lowercase hexadecimal bytes). Git stdout
is capped at 1 KiB for version and HEAD, 1 MiB for unmerged entries, 4 KiB for
the settings tree entry, and 8 MiB for the committed settings blob. The
directory scan allows at most 4096 entries, 8 MiB of file content, and 32
directory levels. These are payload limits, not execution-time limits.

The preflight reports the unapproved directory set, retained and ordered
current ignore rules, state, repo and machine configuration file tuples, and
projected candidate and directory tuples. Tuples use the same non-cryptographic
hashes as other reviewed decisions. A grant changes only the corresponding local
standing state; it does not capture files.

Both standalone trust and hook review take an existing lock in read-only mode,
require it to be held, and run the operation guard. They do not recover a
pending pull or refresh Git attributes; the guard refuses an unfinished
operation instead. If the lock file is absent or the filesystem cannot lock,
the operation refuses without writing state.

The `approvals` members for directories, ignores and mass deletion are accepted only with sync.
Retained ignore patterns remain active without a matching approval to drop them.

## Sync review

Sync requires a `sync` object; apply rejects one. It has required nullable
strings `head` and `tracking_ref`, plus `captures` and `outgoing` arrays. Build
these from `sync --dry-run --json`:

- Copy `outgoing.head.raw` and `outgoing.tracking_ref.raw`, retaining nulls.
- A capture has `key`, `source`, `logical_r`, `raw_h` and `stored_before`.
  Copy them from its capture record. Capture binds raw home and stored source
  as well as logical repo content.
- An outgoing entry has `path` and `stored`, copied from a `repo_edit` or `own`
  record. Paths are literal repository-relative byte paths without empty,
  `.` or `..` components.

The commit includes listed paths whose projected Git tuples still match.
Unlisted or stale changes remain uncommitted and produce exit 3. A private
index builds the reviewed commit, preserving unrelated staged entries.
Dotman's own paths can appear in outgoing entries, but not file decisions.
Real staging of a hypothetical ciphertext entry requires a matching capture and
the exact ciphertext that same run produced; it is not a wildcard approval.
During a dry run, a validated projected encrypted capture can retain
`hypothetical_ciphertext`. That projection grants no real staging permission.
An encrypted capture rechecks source selection and its stored snapshot after the
encryption child, then keeps that observation bound through the write engine.
Its concrete authorization retains the produced ciphertext bytes, not a later
source reread.
New directory captures report logical R as missing only when the key has no
source or state entry and the stored destination was observed missing.
Their raw H and portable link observations remain distinct.
Automatic system captures are left undecided in a reviewed run; they do not
inherit permission from a whole-repository sync review.

### Selected lines

For a selected-only commit, copy the outgoing report's `selected_target`
binding into `sync.selected_target`. Supply one reviewed capture in `captures`,
an empty `outgoing` array, and `sync.selected_lines` mapping that capture's
logical key to the chosen one-based IDs from its [exact diff](diff-json.md).
Use `--no-hooks`, with no file decisions or approvals. The IDs refer to edits
in those content observations; they are not line numbers in a later file.

This mode requires a clean repository and index, an existing directly
tracked regular text file, and local content edits with unchanged mode.
The CLI checks the observed inputs, reconstructs the selected content,
checks the result against the guard, and records that content as the new
baseline. Home is not rewritten, so a subsequent status can still show local
edits. An insertion after an unterminated line may require selecting the
newline change as well.

`local` identifies a no-origin operation. A supported `file:///`, `https://`
or `ssh://` push endpoint and its branch instead produce an opaque `git-…` binding. This
is a destination change detector, not a credential. Selected push uses the
new commit and the bound branch without force, pull or rebase. Pushing a
branch can also publish its existing ancestors. On push failure, inspect local
and remote state before taking another action: the new local commit remains.
An absent target binding is not permission to push.

HTTPS endpoints have no userinfo, query or fragment; SSH endpoints may include
a user. Both accept a DNS-style host, optional numeric port and a restricted
ASCII path. IPv6 literals, scp syntax, URL rewrites and multiple push URLs are
not supported. HTTPS verifies TLS and rejects redirects, proxies and configured
credential helpers. For authentication it reads an existing private Git
credential-cache socket at `$XDG_CACHE_HOME/git/credential/socket`, or
`$HOME/.cache/git/credential/socket` when `XDG_CACHE_HOME` is unset. The
credential must match the HTTPS host, port and path; the cache is not started or
modified. The socket and its private parent must be owned by the invoking user.
The resolved socket path must contain only ASCII letters, digits, `/`, `.`,
`_`, or `-`. `GIT_SSL_CAINFO` can supply an explicit CA bundle. SSH uses only
`SSH_AUTH_SOCK` for authentication and `$HOME/.ssh/known_hosts` for host
verification, with SSH configuration and identity files disabled. The SSH
Home path must be absolute and contain only ASCII letters, digits, `/`, `.`,
`_`, or `-`. Both are noninteractive. No URL or credential is included in the
binding or error.

## HEAD receipts and compatibility

A reviewed commit publishes by expected-old comparison on the full reviewed
branch ref. A changed symbolic HEAD refuses index installation and continuation;
an unrelated branch is not used as the publication target.
Publication uses a non-dereferencing prepared Git transaction. While Git holds
the branch lock, the CLI checks that the branch ref is direct, not symbolic.
Git versions without this transaction protocol, including 2.25, refuse reviewed
publication with exit 3. The project Git minimum and ordinary terminal sync are
unchanged. A closed-input transaction probe checks support before publication;
constructing a reviewed commit may already have created unreachable Git objects.

An opaque pull/rebase has an operation-scoped branch-reflog boundary and unique
action marker, with reflog recording forced on. Its attributed transition must
start at the expected tip. Replay returns the exact OID supplied to its successful
branch compare-and-swap. A no-op or absent remote branch retains the previous
expected tip. The live branch and HEAD must match the receipt before incoming
apply, re-encryption, hooks or push. An arbitrary later HEAD is not a receipt.
Detached replay also retains an attributed HEAD-reflog chain across its rebase
and correction commands. Its explicit produced tip supplies the publication
candidate; subsequent HEAD reads only check equality. Attribution refusal leaves
external detached commits and staged work in place, with the pull marker for
terminal recovery.
Corrective commits are built in a private index with an explicit parent, then
published to detached HEAD using the same prepared direct-ref guard. The
transaction drains stderr concurrently with its stdout protocol and reaps the
child before returning, including on protocol refusal.

Missing, ambiguous or mismatching receipt evidence stops experimental reviewed
continuation with exit 3. The completed Git operation and external changes are
left in place; unlisted staged entries are restored where they do not overlap
unmerged recovery paths. Refresh status before retrying.

The Git minimum remains 2.25. That version's sequencer reflog formatter ignores
`GIT_REFLOG_ACTION`, so a rebase can lack an attributable receipt. Such a reviewed
run fails closed with exit 3 before downstream work or push. Ordinary terminal
sync retains its existing behavior and does not require these receipts.

The review covers outgoing changes, not incoming versions. Incoming changes
apply normally, and pull conflicts need terminal recovery. An upstream hook
table change needs a fresh approval. Recipient changes can produce a follow-up
re-encryption commit containing the rewritten sources and recipients record.
An uncommitted age source, `.dotman.toml` or recipients record holds that
re-encryption without replacing sources or the record.
Re-encryption freezes these inputs before preparation and checks the same
observations at replacement. It returns produced ciphertext and record bytes,
not authorization reconstructed from later reads. The commit gate requires the
complete produced batch and unchanged recipient settings; a late edit refuses
that commit together rather than publishing a subset with a full-batch record.
Age decrypts a private, temporary ciphertext snapshot containing the frozen
bytes; live source paths are not the preparation input. Source inventory and
protected index entries are bound as well. Checks reject new age paths, variants
and staged-only age changes before replacements, before the recipients record
and during commit selection, while allowing this batch's exact worktree writes.

Treat the process exit as authoritative and refresh status after a writer.
After exit 4, perform the reported terminal recovery and discard the old input.
Tuples are not cryptographic identities; backups remain the file-write safety net.
