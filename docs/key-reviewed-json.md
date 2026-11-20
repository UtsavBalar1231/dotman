# Reviewed identity requests

On Linux, `dotman key reviewed` reads one JSON request from standard input. It
does not accept a key or source path as an argument. This interface is for a
trusted native client; a file import must use a path selected by a native
local-file chooser, not one supplied by web content.

The envelope is:

```json
{
  "schema_version": 1,
  "repo_id": "the current repository ID",
  "home": {"raw": "/home/example"},
  "repo": {"raw": "/home/example/.local/share/dotman"},
  "config": {"raw": "/home/example/.config/dotman/config.toml"},
  "identity": {"raw": "/home/example/.config/dotman/age-identity.txt"},
  "config_observation": null,
  "parent_dev": 42,
  "parent_ino": 12345,
  "operation": {"kind": "create"}
}
```

Paths use `{"raw":"…"}` or, for non-UTF-8 paths, `{"bytes":[…]}`.
They must equal the current resolved paths. The identity parent must exist,
be owned by the user, private to that user, and have no symlink in its path.
Destinations overlapping the repository, state, reserved paths or managed paths
are refused, including aliases of their existing parent directories.
`parent_dev` and `parent_ino` identify that directory. The machine config
observation is null when absent; otherwise it has these fields from `stat`:

```json
{"dev":42,"ino":123,"size":50,"mtime":100,"mtime_nsec":0,"ctime":100,"ctime_nsec":0}
```

`mtime` and `ctime` are seconds, with nanosecond fractions in the corresponding
`_nsec` fields. Capture the observations and paths during native review.
The command checks the context and observations again under the per-user lock.
For reviewed writes, it validates the state directory and the lock file's owner,
privacy, type and link count before acquiring the strict lock, then rechecks the
lock file's identity before publication.

`create` generates a native age identity. `import` instead uses:

```json
{"kind":"import","source":{"raw":"/home/example/selected-key"},
 "source_observation":{"dev":42,"ino":124,"size":100,"mtime":100,"mtime_nsec":0,"ctime":100,"ctime_nsec":0}}
```

The source must be a private, user-owned regular file with one link and exactly
one native age identity. Symlinks in the source path, plugin identities and
other formats are refused. The source is checked before and after reading and again before
publication. Neither operation edits `.dotman.toml`. Both require an absent
destination and publish without replacing it at mode 0600. Successful stdout is
one JSON line, `{"record":"identity","recipients":["age1…"]}`. It contains
public recipients only. The native client must check the process exit status;
if publication happened but the receipt was lost, refresh instead of retrying.

To review the installed identity, send the same envelope with
`"operation":{"kind":"grant_preflight"}`. It reads without taking the write
lock or changing files. On success it emits a `grant_preflight` record containing
`schema_version`, `repo_id`, `home`, `repo`, `config`, `identity`,
`config_observation`, `parent_dev`, `parent_ino`, `repo_dev`, `repo_ino`,
`identity_observation`, `repo_config_observation`, `repo_config_digest` and
`recipients`, `recipient_present`, `backup_consequence`, `recovery_parent` (a
typed path), `recovery_parent_dev`, and `recovery_parent_ino`. The state directory
must already exist, be user-owned and private. `recipient_present` reports exact
membership of the derived public recipient in the parsed `[age].recipients`
list. The digest is the project's xxh3-128 content hash, not a
cryptographic authenticator. No identity bytes or settings content are emitted.
The `grant` operation checks the digest and repository directory identity from
this record. The native confirmation must disclose the backup consequence and
recovery parent.

To add the displayed recipient, obtain a separate native confirmation and send
another request with the same envelope and:

```json
{"kind":"grant",
 "identity_observation":{"dev":42,"ino":125,"size":100,"mtime":100,"mtime_nsec":0,"ctime":100,"ctime_nsec":0},
 "repo_config_observation":{"dev":42,"ino":126,"size":100,"mtime":100,"mtime_nsec":0,"ctime":100,"ctime_nsec":0},
 "repo_config_digest":"the digest from preflight",
 "repo_dev":42,"repo_ino":123,
 "recovery_parent":{"raw":"/home/example/.local/state/dotman"},
 "recovery_parent_dev":42,"recovery_parent_ino":127,
 "recipients":["age1…"]}
```

The recipient list must exactly match the public recipient freshly derived from
the installed identity. Send the preflight recovery path as `{"raw":"…"}`
or `{"bytes":[…]}`, omitting the display-only `shown` field, and copy its
device and inode. The grant checks the exact resolved path and directory identity
before locking and again under the lock before exchange. The repository settings observation is for
`.dotman.toml`. If the recipient is already listed, the settings file is left
untouched. Otherwise the grant emits a `settings_recovery` record containing a
typed `directory` path and `phase: "prepared"` before publication. At that point
`reviewed` is a byte copy and `candidate` is the proposed file, not a backup.
After exchange and verification a second record has `phase: "published"`; then
`candidate` names the displaced inode and `reviewed` retains the reviewed bytes.
An open descriptor to the displaced inode can still change `candidate`. The
directory is not automatically deleted. A `recipients_added` record follows only
after verification. On failure, refresh and inspect the directory if it still
exists; neither record authorizes a blind restore over a later edit. Stale observations or a different recipient
are refused. Declining or failing this
second operation does not delete or roll back the installed identity. Refresh
the status and settings after a failure; do not report both effects as complete.

The request does not carry identity bytes or secret hashes. Do not forward
source paths, observations, or private key material through a WebView. Error
details go to stderr; stdout alone is not proof of success.
