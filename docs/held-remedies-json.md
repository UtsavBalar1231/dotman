# Held-file review

`dotman remedy --from - --json` accepts one JSON request on standard input, up
to 65536 bytes:

```json
{"schema_version":1,"operation":"review","key":".config/app/data"}
```

The key is relative to Home. This read-only preview accepts an unmanaged regular
leaf reported as large or binary under an approved directory. It refuses a
content-guard finding and a destination that is not absent. The
`remedy_review` NDJSON record includes the hold reason and the observed Home
tuple. It contains no file contents.

The request, including its typed snapshot when present, is parsed and checked
before the repository lock or any child process is started.

The record has `executable: true` and a `snapshot` only when the file and
parent modes need no metadata changes. The native client must keep this
snapshot out of the renderer and pass it unchanged in a second request:

```json
{"schema_version":1,"operation":"execute","key":".config/app/data","snapshot":{"key":"..."}}
```

The abbreviated snapshot above is not valid input: use the complete object
returned by review. Execution checks it again under the lock and installs one
file without replacing an existing destination. It does not run hooks, commit,
or sync. If state cannot be saved after installation, the command reports that
the file was installed and status needs refreshing. A mode that needs a
metadata change remains read-only.

`review_plain` is a separate action for a regular text file held only because
its name matches a secret-name pattern. It refuses binary content and every
name or content finding, even one covered by a machine grant. The review sets
`plaintext_may_be_published: true`; the native client must show that warning
and obtain explicit consent before `execute_plain`. Its snapshot cannot be
used for another action. For a `0600` file, `prepare_mode: true` means the
review includes adding that exact entry to `[modes]`. No `[guard] allow` entry
is added. If publication fails after mode preparation, the setting may remain;
the result reports a partial change and the client must refresh.

Both `review_plain` and `review_encrypt` require an `occurrence` object with
`scope: "new"`, `source: null`, the status hold `reason`, and `finding`
(`null` for a name-only hold, otherwise an object with `rule`, `place`, and
one-based `line`). The returned occurrence is recomputed from the current
hold. Pass it unchanged with the matching execute request. The original
`review`/`execute` large-or-binary operation remains available without it.

To review a new secret-named or content-held file for encryption, use
`operation: "review_encrypt"`. Execute it with `operation: "execute_encrypt"`
and the returned native-only snapshot. This operation requires a regular Home
file observed at `0600` or `0644`, configured public recipients, and absent
plain and encrypted destinations. An existing recipient record must match the
configured recipients. If the record is absent, the guarded operation can create
the first record when no encrypted sources exist in the worktree, HEAD or index.
An age identity is not required. A `0644` Home file remains unchanged; the
encrypted source applies at `0600` by default on a later apply. The review
reports `recipients`, `destination`, and `plaintext_history_may_remain`.

For a managed Home- or repo-held occurrence, `review_encrypt` uses a distinct
managed snapshot and reports `managed_consequences`: the selected source,
all sources, the unencrypted recovery parent and consequence, the mode
consequence, whether the recipient record will be created, and the expected
state and mode changes. The source is moved to private recovery with its
original inode. The Home file must be `0600` or `0644` and the selected source must not
have a `[modes]` entry; other modes need a separate review. For a repo-held
occurrence, Home and the selected source must have identical bytes. A
Home-held occurrence may also use an unchanged safe source whose bytes and
Git mode match both HEAD and the index. The recovery backup is unencrypted and
outside the repository. If the private backups directory does not exist,
`creates_backup_directory` discloses that execution will create it at `0700`;
review does not create it.

A `0644` Home file remains `0644` during the conversion. The encrypted source
applies at `0600` on a later apply.

`review_forget` requires an `occurrence` object naming `scope` (`home` or
`repo`), the physical `source`, `reason: "content"`, and `finding` with
`rule`, `place`, and one-based `line`. The source and finding must still match
that exact held occurrence; repo occurrences must be tracked. Repeat the same
occurrence with `execute_forget`.

The review reports the occurrence, all source names and modes, source count, that Home is preserved, and
that each source gets a private backup. `execute_forget` accepts only that
review's snapshot. It removes the reviewed sources and their mode and state
metadata, adding an ignore when needed to prevent recapture. Selector names
that themselves contain a guard finding are refused; use terminal guidance
for those cases. This operation does not sync or commit.

After a parsed request is refused, a `remedy_error` record supplies a sanitized
`refused` or `partial` code and message; the final result record carries the
exit status. A partial result means a mode setting or file may already have
been installed. Review again rather than retrying automatically.
