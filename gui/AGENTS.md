# Linux desktop

`gui/` is a separate Rust 1.90 workspace. Tauri embeds the local WebView assets and delegates Dotman operations to the matching CLI.

## Where to look

| Change | Location |
|---|---|
| Workbench layout, React components, and theme | `frontend/workbench.tsx`, `frontend/shell/`, `frontend/styles/` |
| File navigator, diff reader, and selected lines | `frontend/file-tree.ts`, `frontend/diff-reader.ts`, `frontend/app.ts` |
| Identity, policy, and frontend IPC types | `frontend/keys.ts`, `frontend/policy-trust.ts`, `frontend/protocol.ts` |
| Native commands and reviewed writes | [src/AGENTS.md](src/AGENTS.md) |
| Setup, supported operations, and packaging | [docs/linux-gui.md](../docs/linux-gui.md) |

## Assets and checks

- Edit the React, TypeScript, and CSS sources in `frontend/`. With Node 24 and `npm ci --prefix gui`, regenerate the tracked Vite bundle using `just gui-frontend`. Keep assets local and render CLI-supplied strings as text.
- Tauri embeds the generated `dist/` directory. The Rust build and installed app use those assets without Node. Keep the production CSP and the restriction to the bundled base document.
- Run the Just recipes from the repository root. `just gui-frontend-check` rebuilds and compares the Vite bundle and runs the frontend contract and browser tests. Set `CHROMIUM` to override the headless Chromium executable.
- `just gui-check` adds the separate Cargo workspace's format, lint, test, locked metadata, and dependency policy checks. A native build needs GTK 3 and WebKitGTK 4.1 development files.
- The Arch package is `packaging/arch/gui/PKGBUILD`. The Debian and RPM helper `scripts/package-linux-gui.sh` packages a locally built pair. Follow the receipt procedure in the desktop build guide.
- `vendor/` contains two patched GTK/GLib macro crates. Check `vendor/PROVENANCE.md` before changing either override or its lockfile entries.

```sh
npm ci --prefix gui
just gui-frontend
just gui-frontend-check
cargo test --manifest-path gui/Cargo.toml --locked --workspace
cargo build --manifest-path gui/Cargo.toml --release --locked
```

For a native smoke test, place regular builds of `dotman-gui` and `dotman` from the same checkout side by side. Launch the GUI with a disposable Home and XDG tree so status and reviews describe the intended fixture.
