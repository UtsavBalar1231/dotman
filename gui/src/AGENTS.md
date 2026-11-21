# Desktop IPC and CLI bridge

Rust owns the CLI process, retained status and reviews, native confirmations, and writer receipts. The WebView presents projections and submits the supported choices.

## Where to look

| Change | Location |
|---|---|
| IPC registration and document restrictions | `main.rs`, `../build.rs`, `../capabilities/main.json`, `../tauri.conf.json` |
| Review state and projection into the WebView | `app.rs`, with setup, keys, held files, policy, and variants in `app/` |
| Child argv, environment, pipe limits, and cancellation | `runner.rs` and `sys.rs` |
| CLI output parsing and decision input | `protocol.rs`, `decisions.rs`, `writer_outcome.rs`, and action-specific validators |
| Native confirmation and path selection | `confirm.rs`, `folder_chooser.rs`, `key_chooser.rs` |
| Terminal recovery steps | `recovery.rs` |

## Contracts

- When changing IPC, keep the Tauri handler, build manifest, window capability, and `../frontend/protocol.ts` in step. Keep the bundled-document navigation and new-window restrictions in `main.rs`.
- Resolve the companion through `runner.rs`. It must be the absolute regular file `dotman` beside the GUI executable. Keep fixed argv, noninteractive mode, bounded I/O, and the actual child exit status. Startup removes `DOTMAN_AGE_KEY` before building the WebView and the runner removes it at spawn.
- Accept only the operation-specific choices in the IPC contract. Resolve approved paths, raw CLI tuples, and decision contents from the retained Rust review. Preserve generation and one-use checks, including invalidation on refresh or cancellation.
- Use the native confirmation and chooser paths for trusted input. Whole sync defers hooks, pending hooks require separate review, and system writes stay in the terminal. Require native confirmation before displaying decrypted diff contents.
- Treat an invalid or incomplete receipt as unverified and direct recovery to the terminal. Cancellation requests SIGTERM for the verified process group while the runner owns the child.

## Protocol changes

Update the relevant contract under `docs/` and the CLI's integration tests when changing a producer consumed here. Validate parsing and stale-review behavior in the corresponding Rust tests. For projection or IPC changes, check the frontend contract and the fixture bridge in `../tests/ui.js`.

```sh
cargo test --manifest-path gui/Cargo.toml --locked --workspace
```
