# Linux desktop

`gui/` is a separate Rust 1.90 workspace. Tauri embeds the local WebView assets and delegates Dotman operations to the matching CLI.

## Where to look

| Change | Location |
|---|---|
| Workbench layout, theme, and static controls | `dist/index.html`, `dist/style.css` |
| File navigator, diff reader, and selected lines | `frontend/file-tree.ts`, `frontend/diff-reader.ts`, `frontend/app.ts`, and the matching CSS in `dist/` |
| Identity, policy, and frontend IPC types | `frontend/keys.ts`, `frontend/policy-trust.ts`, `frontend/protocol.ts` |
| Native commands and reviewed writes | [src/AGENTS.md](src/AGENTS.md) |
| Setup, supported operations, and packaging | [docs/linux-gui.md](../docs/linux-gui.md) |

## Assets and checks

- Edit TypeScript in `frontend/` and regenerate the tracked `dist/*.js` with `just gui-frontend`. Use the compiler pinned in the desktop build guide (TypeScript 6.0.3, with `TSC` available as an override). Keep `.js` specifiers in TypeScript imports for the browser's native ES module loader.
- Edit HTML and CSS directly in `dist/`. Tauri loads that directory as shipped, with no bundler or Node runtime. Keep assets local and use text nodes for CLI-supplied strings.
- Run the Just recipes from the repository root. `just gui-frontend-check` compares the compiler output with tracked JavaScript and runs the Node tests. `tests/theme.js` and `tests/workbench-layout.js` use headless Chromium (`CHROMIUM` overrides the executable) through `tests/browser.js`.
- `just gui-check` adds the separate Cargo workspace's format, lint, test, locked metadata, and dependency policy checks. A native build needs GTK 3 and WebKitGTK 4.1 development files.
- The Arch package is `packaging/arch/gui/PKGBUILD`. The Debian and RPM helper `scripts/package-linux-gui.sh` packages a locally built pair. Follow the receipt procedure in the desktop build guide.
- `vendor/` contains two patched GTK/GLib macro crates. Check `vendor/PROVENANCE.md` before changing either override or its lockfile entries.

```sh
just gui-frontend
just gui-frontend-check
cargo test --manifest-path gui/Cargo.toml --locked --workspace
cargo build --manifest-path gui/Cargo.toml --release --locked
```

For a native smoke test, place regular builds of `dotman-gui` and `dotman` from the same checkout side by side. Launch the GUI with a disposable Home and XDG tree so status and reviews describe the intended fixture.
