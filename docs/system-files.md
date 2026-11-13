# System files

Managing files outside `$HOME`. Back to the [README](../README.md).

Files outside `$HOME`, such as `/etc/keyd/default.conf`, are opt-in. Nothing happens until the machine config lists `[system] roots = ["/etc"]` (`init --system-root /etc`, or answer the prompt in `add --system`). A cloned repo cannot enable it.

```bash
dotman add --system /etc/keyd/default.conf
dotman apply --system
```

- A file owned by a package needs `--package-owned`. Files you cannot read are refused, and so are files the content guard holds, since system scope does not encrypt.
- `apply --system` (and `sync --system`) shows each path with its diff, owner and mode, asks once, then escalates once through `sudo`, `doas` or `run0` to write.
- Never allowed: `/`, `/proc`, `/sys`, `/dev`, `/run`, `/tmp`, `/var/tmp`, and on macOS the SIP-protected paths.
- `sync` captures local edits to system files but does not write them without `--system`.

See [SECURITY.md](../SECURITY.md) before adding a sudoers rule, and install dotman root-owned on a machine that uses system files.
