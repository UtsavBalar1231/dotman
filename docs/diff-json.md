# Experimental diff JSON

`dotman diff --json [--] [PATH ...]` compares the repo to home or to the
configured system scope. Paths use the same selection rules as terminal `diff`.
The command is read-only apart from private temporary comparison files and the
normal diagnostic temporary directory. It does not update state or approvals.

## Framing

Stdout is newline-delimited JSON. Split on LF bytes. A `context` record uses
`schema_version: 1`, `experimental: true`, and `command: "diff"`; its fields,
version rules, and the main-owned final `result` follow
[status JSON](status-json.md). The process exit is authoritative. A failed
validation can produce a final error result without a report.

A successful report contains zero or more `diff` records, then a `report`
record with `count`, then the final `result`. A `diff` record has:

| Field | Meaning |
| --- | --- |
| `key`, `scope`, `path` | Logical key as text, `home` or `system`, and the resolved home or synthetic-system path |
| `status` | `equal`, `different`, `blocked`, `locked`, `template_error`, `unreadable`, `invalid`, `ambiguous`, or `refused` |
| `system_off` | The system key lies outside the configured roots |
| `source` | Winning source path as text, or null |
| `r`, `source_tuple`, `h`, `raw_h` | Observations described below |
| `payload` | Git's captured no-index diff as a byte-preserving value, or null when Git did not run |

Text and paths use the `raw`/`shown` and `bytes`/`shown` representation in
[status JSON](status-json.md). The payload uses the same representation as a
path to preserve non-UTF-8 bytes; it is diff data, not a filesystem path.
`shown` is escaped display text. Treat `raw` and reconstructed `bytes` as
untrusted content, not terminal control sequences or markup. A `different`
record can have an empty payload when only permission bits Git does not display
differ. Git displays file executable bits, while tuple modes retain the
complete effective permission bits.

## Snapshots and observations

The `r`, `source_tuple`, `h` and `raw_h` observation forms and xxh3-128 tuple
encoding match status JSON. `source_tuple` describes the rendered or decrypted
source before a kept-mode adjustment; `r` describes the effective mode. `h`
describes the content compared with `r`, while `raw_h` describes the home
node before portable symlink conversion. For an absolute symlink into home,
the compared target is the relative portable target and its hash is `h`;
the original absolute target is represented by `raw_h`.

Comparison uses owned content snapshots: the H node supplies its observation,
equality decision and staged bytes; the R entry supplies its content and
tuple. Missing sides are represented as missing, and blocked or unavailable
sides do not acquire fabricated tuples. A system observation is read as the
invoking user, including when scope is disabled.

Git runs without a pager, colour, external diff, or text conversion. Its stdout
is captured in `payload`, not forwarded as a separate stream. Plaintext from
decryption is staged in a private temporary tree outside the repo and removed
when the comparison returns, including after an interrupt handled during Git.
A SIGKILL can leave the tree for the existing temporary-file sweep.
