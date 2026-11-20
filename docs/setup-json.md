# Experimental guarded setup JSON

On Linux, `dotman init --setup-from - --json` takes a versioned request on
standard input. This lane is for preparing a new repository; ordinary `init`
keeps its interactive and non-interactive behavior. Do not combine the request
with the ordinary `init` flags or source operand.

```json
{
  "schema_version": 1,
  "home": {"raw": "/home/example"},
  "repo": {"raw": "/home/example/.local/share/dotman"},
  "parent_dev": 42,
  "parent_ino": 12345,
  "operation": {"kind": "fresh"}
}
```

Use the exact resolved home and repository paths from
[status JSON](status-json.md). For a non-UTF-8 path, use `{"bytes":[…]}` instead
of `{"raw":"…"}`. Omit the display form. The parent numbers are the unsigned
device and inode of the opened repository parent, not the repository itself.
The parent must exist, be owned by the invoking user, and be neither symlinked
nor writable by group or other users. The repository destination must be absent;
an existing directory, even an empty one, is not eligible.

Fresh setup creates a default repository, without adopting another repository
or running the setup wizard. It prepares the new files in a private sibling
directory before publishing them without replacing the destination. It takes
the per-user lock, which may create the private lock file under the state
directory. The parent of that state directory must also exist and be safe to
open. Setup does not apply home files, run hooks, create an age identity, or
change the machine configuration.

For clone-only, use `{"kind":"clone","source":"https://example.com/repo"}`.
The source is limited to 8192 bytes, independently of the request size limit.
It is a literal HTTPS URL with an ASCII DNS name or canonical IPv4
address, an optional port from 1 to 65535, and a nonempty path. The path
components contain only ASCII letters, digits, `-`, `.`, `_`, and `~`; empty
and relative components are refused. DNS labels contain only ASCII letters,
digits and hyphens, cannot start or end with a hyphen, and are at most 63
bytes; the host is at most 253 bytes. A DNS final label cannot be all digits
or start with `0x` (case-insensitive). IPv6 and alternate numeric IPv4
spellings are refused. A literal `file:///absolute/path` with the same path
grammar is also accepted with a numeric Git version of 2.45.1 or newer;
prerelease and other suffixed version tokens are refused for file clones.
Userinfo, escapes, queries, fragments, redirects, proxies, credential helpers
and URL rewrites are not supported.
Clone prepares a clean checkout for review; it does not apply it.

## Static working-tree import

Import accepts one explicitly selected, canonical local directory containing
ordinary dotfiles. It reads the working tree, not Git history or a Git object
database. A top-level `.git` entry is skipped without opening it. Bare Git,
yadm or vcsh repositories, chezmoi name conversion, symlink farms, templates,
scripts and other import formats are not supported here; use the terminal
importer when those conversions are needed.

First send the same request envelope with this operation:

```json
{"kind":"import_preview","source":{"raw":"/home/example/selected-dotfiles"}}
```

The `import_preview` record contains `kind: "static_tree"`, `semantics`, the
selected `source`, a file `count`, and a `review` object. Display the source,
semantics, each `review.files[].path`, `mode`, and `size`, and each
`review.directories[].path` and `mode` before confirmation. The review contains
source and destination path bytes and directory identities; each file has its
device, inode, mode, size and Git blob ID. It contains no file contents. The
preview creates no destination and produces no `setup` record.

`review.manifest_sha256` is a 64-character lowercase SHA-256 Git blob ID
computed by the CLI, after the content guard passes. Its blob bytes start with
`dotman-static-import-v1` and a NUL. Directory records follow in traversal
order, then file records in traversal order. Each record begins with `d` or
`f`, followed by the UTF-8 path length as a little-endian `u64`, the path, and
the source mode as a little-endian `u32`. A file record also contains its
content length as a little-endian `u64` and its exact bytes. Git hashes that
sequence with its normal `blob <length>\0` header. The digest covers the
reviewed file bytes, paths and modes, without sending file contents to the
client. The per-file Git blob IDs are for checking the published tree; they
are not the SHA-256 manifest.

After explicit confirmation, send a new request with the same envelope and
source, substituting the complete, unmodified review object:

```json
{"kind":"import","source":{"raw":"/home/example/selected-dotfiles"},"review":{}}
```

The empty object above is a placeholder, not a valid review. Execution checks
the source and the destination again under the lock, prepares one fresh
snapshot commit, checks its contents, rechecks the source and publishes only
if the destination remains absent. Success produces a `setup` record with
`operation: "import"`, the new `repo_id`, and the accepted `review`, followed
by the final `result`. No original history, remote, hook, machine configuration,
bootstrap or home-file application is imported. If the source changes, obtain
and review a new preview. Treat a missing setup record or final result as
uncertain rather than retrying publication blindly.

Only UTF-8 text files without control characters other than tab, newline and
carriage return are accepted. The source cannot contain symlinks, hard-linked
files, special files, empty directories, executable files, shebang scripts, Git
control files, brace names, recognized foreign transformations, unsafe keys,
colliding names, probable-secret file names such as `.env`, or content or names
held by the content guard. Sensitive source path components are also refused.
Unsupported entries fail the entire preview; nothing is silently omitted except
the top-level `.git`.
The limit is 4,096 entries, 32 directory levels, 8 MiB of file content and a
900,000-byte serialized review. The request itself is limited to 1 MiB.
The content guard is pattern-based, so review the selected files for secrets
before importing; a clean scan is not a guarantee.

The published repository directory is private (0700). Source file permissions
and parent-directory permissions are recorded in `.dotman.toml` `[modes]` for
later dotman application when they differ from the normal Git defaults; group
and other write bits are not imported. Special and executable bits are not
carried. Git does not preserve non-executable
permission bits: a separate Git checkout may create files as 0644 subject to
its umask. Protect that checkout separately; `[modes]` does not chmod a Git
checkout. Confined traversal requires Linux `openat2` with `RESOLVE_NO_XDEV`
and `RESOLVE_NO_SYMLINKS` and a mounted `/proc`; there is no fallback.
This import also requires Git support for SHA-256 repositories. It creates one
inside the private stage only to calculate the review digest; it fails closed
if Git cannot create one. The published repository keeps Git's ordinary
default object format. Fresh and clone setup do not require this capability.

The destination cannot overlap dotman's machine configuration, lock and state,
or identity paths. Git runs with a private home and configuration directory
and no inherited Git configuration. Publication requires Linux filesystem
support for `RENAME_NOREPLACE`; there is no fallback that can replace an
existing destination.

Stdout is NDJSON. If resolution succeeds, `context` has `command: "init"` and
`repo_id: null`. A `setup` record after publication names `operation: "fresh"`, `"clone"`,
or `"import"`
and carries the new `repo_id` as `{raw, shown}`. A final `result` matches the
process exit status, as in [other outcomes](outcomes-json.md).

A published repository can outlive an interrupted or incomplete receipt. A
client should report uncertainty, rather than retrying, if it cannot confirm
both a successful final result and a fresh status report for the same repository
path and new identity.
