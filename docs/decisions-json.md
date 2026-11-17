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

Directory, ignore and mass-delete approvals are accepted only with sync.
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

## HEAD receipts and compatibility

A reviewed commit publishes by expected-old comparison on the full reviewed
branch ref. A changed symbolic HEAD refuses index installation and continuation;
an unrelated branch is not used as the publication target.
Publication uses a non-dereferencing prepared Git transaction. While Git holds
the branch lock, the CLI checks that the branch ref is direct, not symbolic.
Git versions without this transaction protocol, including 2.25, refuse reviewed
publication with exit 3. The project Git minimum and ordinary terminal sync are
unchanged; the protocol-unavailable regression is not a runtime Git2.25 test.

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
