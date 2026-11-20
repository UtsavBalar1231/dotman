# dotman on Linux desktop

The file workspace opens with a folder tree. Use **Status** to group files by
what changed, or search by path or status. A wide window keeps the navigator
beside the comparison; a narrow window uses a unified diff. Close the comparison
to return to its file, or to Search if the file is filtered out. Expand
**Resolved paths** to check the active repository, Home, configuration, state
and identity paths. **Appearance** follows the system by default. You can
select light or dark and change the accent for the open window.

## Set up a repository

If the resolved destination is absent, you can create an empty repository,
clone into it, or import a selected directory of ordinary static dotfiles.
Each route shows the destination in a native confirmation.
Clone also shows the exact source and stops at a checkout for review; it does
not apply files or run hooks. Use a public HTTPS URL without credentials or
an absolute `file:///` URL with Git 2.45.1 or newer. The
[guarded setup contract](setup-json.md) describes supported paths and the
pre-existing private parent required for setup. If a result cannot be
verified, refresh status before deciding what to do; a repository might
already exist.

Import opens a local directory chooser and previews the selected working tree.
The native confirmation lists every file and directory with its mode; if the
complete list cannot fit safely, import is refused. The desktop currently accepts
at most 12 combined file and directory entries for this confirmation; source
and destination paths must be printable ASCII of at most 128 bytes, and entry
paths printable ASCII of at most 64 bytes. Import creates a snapshot
without history, remotes, hooks or automatic Home apply. It does not interpret
bare Git repositories, chezmoi names, symlink farms, scripts or templates.
See the [static import restrictions](setup-json.md#static-working-tree-import)
before selecting a source.

Use the terminal to adopt or repair a repository or import other formats. The
desktop can show reported sources, review a changed machine identity, and
create an exact per-host variant of an eligible managed plain or age-encrypted
file. Variants requiring unsupported settings or mode edits need terminal
review. Templates cannot be captured as host variants; eligible managed files
show a selectable `dotman edit` command for use in a terminal, and the desktop
does not launch an editor. System files are read-only. Directory and ignore
standing grants and pending hook approval have separate native reviews.

## Create or import an age identity

For an available repository, use **Create identity** or **Import identity**.
Import opens a native local-file chooser. The identity destination's private
parent directory must already exist. The first native confirmation names the
destination; it does not approve replacing an existing identity or editing
repository recipients.

After installation, a separate native confirmation names the repository,
settings file and derived public recipient. Declining it leaves the installed
identity in place without adding a recipient. If either result cannot be
verified, check status before trying again. See the
[reviewed identity contract](key-reviewed-json.md) for the supported source
format and freshness checks.

## Review changes

Select a file to inspect its diff. If a file needs a choice, select the
available action and review the collected choices before applying them. When
the action replaces a different Home file, it first backs up the old copy.
Skip a file by leaving it undecided.

**Review sync** previews the outgoing changes from the current report. It
does not fetch or predict what is upstream. A whole desktop sync requires native
confirmation and never executes hooks itself. Matching writes are held in the
same pending queue as the terminal for a separate native hook confirmation.
Selected commits and plain file apply do not queue hooks. If a run stops for terminal
recovery, use the same configuration in the terminal, then refresh when ready.
The desktop does not restart the interrupted operation.

For a partial capture, open **View capture diff** from the outgoing review.
When line choices are available, check the edits for one file, close the diff,
and inspect the selected lines before confirming. A replacement includes its
repository and Home lines together. The new commit contains the chosen edits;
other edits remain in Home. Clearing the last checkbox disables the selected
commit action rather than switching to whole-file sync.

Selected commits currently support a directly tracked plain-text file in a
clean repository, with no origin or a supported `file:///`, `https://`, or
`ssh://` push target.
With a target, dotman attempts a non-forced push without pulling or applying
files. A rejected push leaves the commit local. Selected HTTPS pushes use
verified TLS and can use an existing private Git credential cache; SSH pushes
use an agent and the invoking Home's known hosts, without SSH configuration
or identity files.
Ordinary sync is separate.

The default Rust tests do not run live transports. Release transport checks run
`sh tests/cli_selected/live.sh`; they require Node, OpenSSL, OpenSSH tools, Git,
Docker and a local image with `sshd`, Git and `useradd`. Set
`DOTMAN_SSH_FIXTURE_IMAGE` to that image; the default is `act-runner-rust:local`.
The runner requires the TLS and containerized SSH checks to pass.

## Build and package

Use the matching `dotman-gui` and `dotman-cli` packages. The regular binaries
must be in the same directory (for packages, `/usr/bin`). The GUI runs on
Linux with GTK 3 and WebKitGTK 4.1; the CLI needs Git. See [the CLI requirements](getting-started.md#requirements)
for age and rage support. Build the CLI with Rust 1.88 or newer and the
separate GUI workspace with Rust 1.90 or newer.

Frontend source is strict TypeScript in `gui/frontend`. The WebView loads the
compiled ES modules in `gui/dist` without a JavaScript runtime package or a
bundler. After editing the frontend, run `just gui-frontend` with the TypeScript
6.0.3 compiler (`tsc` on PATH, or set `TSC` to its executable). Run
`just gui-frontend-check` to check the generated modules and frontend tests.
Building from an unchanged release checkout uses the supplied modules.

```sh
cargo build --release --locked
cargo build --manifest-path gui/Cargo.toml --release --locked
```

The Arch GUI recipe is in `packaging/arch/gui`. For Debian and RPM archives,
put the two locked release builds from the same checkout in a local
directory. In that directory, write the CLI release number to
`release-version` and run `sha256sum 'dotman' 'dotman-gui' > SHA256SUMS`. This
local receipt checks the bytes against the pair you built; it does not
authenticate them. Pass that directory and an empty output directory to
the package helper:

```sh
bash scripts/package-linux-gui.sh deb /path/to/binaries /path/to/output
bash scripts/package-linux-gui.sh rpm /path/to/binaries /path/to/output
```

These builders package local artifacts; they do not install them or make
an older distribution compatible with binaries built on a newer system.
The checked host binaries need glibc 2.39. Build and verify on the intended
distribution for release. Departure Mono is bundled under the SIL Open Font
License 1.1; the package includes its notice.

## Display and graphics

Launch normally to let GTK choose the display and WebKitGTK use its graphics
stack. To isolate a backend issue, try `GDK_BACKEND=wayland dotman-gui` in a
Wayland session, or `GDK_BACKEND=x11 dotman-gui` in an Xorg session. On a
Wayland desktop the latter typically connects to Xwayland, not standalone
Xorg. Neither setting selects a GPU or proves that rendering is accelerated.

If the native window is blank, `WEBKIT_DISABLE_COMPOSITING_MODE=1 dotman-gui`
is a per-launch diagnostic, not the normal launch configuration. A successful
run with that workaround does not verify hardware compositing.
