# legacy-v2 fixture

A dotman v2 repository as written by the old library (commit 3725269), for testing legacy detection and migration. `repo/` is the repository directory (the old `~/.dotman`).

Layout: `HEAD` (`ref: refs/heads/main`) and `refs/heads/main` (c3), both with a trailing newline (the old writer emits none; the end-of-file pre-commit hook added it, and the old reader trims), `commits/<id>.zst`, `objects/<hash>.zst`, `remote-mappings.toml` (c1 and c2 only, remote `origin`), `index.bin` (empty v2 index from `Index::new().save`; the old `save` only serializes and writes). `.empty` uses the all-zero hash and has no object. Modes are `metadata.mode() & 0o777`, as the old add path stores them (no file-type bits). Commits hold cumulative trees; the root's parent is the null id. The tree hash is not a git-style tree hash: it is xxh3 of the sorted `path\0hash` lines (NUL separator). Ordering inside `remote-mappings.toml` and `Snapshot.files` is hash-map order, so bytes differ between runs; decoded content does not.

Built with `git archive 3725269` extracted and used as a `path` dependency (`cargo build --offline`, 3725269's `Cargo.lock`). Equivalent to `dotman = { git = "file:///home/utsav/dev/dotman", rev = "3725269" }`. The probe loads each commit with `SnapshotManager::new(repo, 3).load_snapshot(id)`.

## Expected manifest

Commits in id order. Modes are `mode & 0o7777` in octal. The `/home/legacyuser/...` key is a literal absolute path.

### 5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d

| field | value |
|---|---|
| id | `5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d` |
| parents | `c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1` |
| message | side: add side.toml |
| author | legacyuser |
| timestamp | 1700000200 |

| path | mode | xxh3 |
|---|---|---|
| `.bashrc` | 0644 | `0bc997ecc5a1d852c576787b4ed8d1fa` |
| `.config/side.toml` | 0644 | `cb891aff7d073dd2e3076d70272c8ca2` |
| `.empty` | 0644 | `00000000000000000000000000000000` |
| `.local/bin/run` | 0755 | `35d709aba01c13cbfa76c57292b6ec0e` |
| `.ssh/config` | 0600 | `044442cc692e68f4c0ad058053236021` |

### c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1

| field | value |
|---|---|
| id | `c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1` |
| parents | `00000000000000000000000000000000` |
| message | c1: initial |
| author | legacyuser |
| timestamp | 1700000000 |

| path | mode | xxh3 |
|---|---|---|
| `.bashrc` | 0644 | `0bc997ecc5a1d852c576787b4ed8d1fa` |
| `.empty` | 0644 | `00000000000000000000000000000000` |
| `.local/bin/run` | 0755 | `35d709aba01c13cbfa76c57292b6ec0e` |
| `.ssh/config` | 0600 | `044442cc692e68f4c0ad058053236021` |

### c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2

| field | value |
|---|---|
| id | `c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2` |
| parents | `c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1` |
| message | c2: edit bashrc, add absolute key |
| author | legacyuser |
| timestamp | 1700000100 |

| path | mode | xxh3 |
|---|---|---|
| `/home/legacyuser/.config/abs.conf` | 0644 | `0b7c91d785e41df5639779f3afec3634` |
| `.bashrc` | 0644 | `4658b874248d98c42d6da39e3e2e9e11` |
| `.empty` | 0644 | `00000000000000000000000000000000` |
| `.local/bin/run` | 0755 | `35d709aba01c13cbfa76c57292b6ec0e` |
| `.ssh/config` | 0600 | `044442cc692e68f4c0ad058053236021` |

### c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3

| field | value |
|---|---|
| id | `c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3` |
| parents | `c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2`, `5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d` |
| message | c3: merge side into main |
| author | legacyuser |
| timestamp | 1700000300 |

| path | mode | xxh3 |
|---|---|---|
| `/home/legacyuser/.config/abs.conf` | 0644 | `0b7c91d785e41df5639779f3afec3634` |
| `.bashrc` | 0644 | `4658b874248d98c42d6da39e3e2e9e11` |
| `.config/side.toml` | 0644 | `cb891aff7d073dd2e3076d70272c8ca2` |
| `.empty` | 0644 | `00000000000000000000000000000000` |
| `.local/bin/run` | 0755 | `35d709aba01c13cbfa76c57292b6ec0e` |
| `.ssh/config` | 0600 | `044442cc692e68f4c0ad058053236021` |

## Generator source

Dependencies: `dotman` (3725269), `anyhow = "1"`, `zstd = "0.13"`. Usage: `legacy-gen <dir>`.

```rust
// Writes a legacy dotman v2 repo (dotman at 3725269) into the directory given as argv[1].
// Layout and encodings mirror the old SnapshotManager::create_snapshot / store_file_content:
//   commits/<id>.zst = zstd(bincode-legacy(Snapshot)); objects/<hash>.zst = zstd(bytes).
// Snapshot.files is a HashMap and is deliberately left unsorted; only the tree hash sorts.
use anyhow::Result;
use dotman::mapping::CommitMapping;
use dotman::storage::file_ops::hash_bytes;
use dotman::storage::index::Index;
use dotman::storage::snapshots::{Snapshot, SnapshotFile};
use dotman::storage::Commit;
use dotman::utils::serialization;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use zstd::stream::encode_all;

// Hard-coded hash the old code assigns to empty files; no object is stored for it.
const ZERO: &str = "00000000000000000000000000000000";
// Old root commits carry this sentinel as their only parent.
const NULL_COMMIT_ID: &str = "00000000000000000000000000000000";
const LEVEL: i32 = 3;

const C1: &str = "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1";
const C2: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";
const SIDE: &str = "5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d";
const C3: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";
const AUTHOR: &str = "legacyuser";

type Tree = HashMap<PathBuf, SnapshotFile>;

fn put(dir: &Path, tree: &mut Tree, key: &str, mode: u32, content: &[u8]) -> Result<()> {
    let hash = if content.is_empty() {
        ZERO.to_string()
    } else {
        let h = hash_bytes(content);
        let obj = dir.join("objects").join(format!("{h}.zst"));
        fs::write(obj, encode_all(content, LEVEL)?)?;
        h
    };
    tree.insert(
        PathBuf::from(key),
        SnapshotFile { hash: hash.clone(), mode, content_hash: hash },
    );
    Ok(())
}

fn tree_hash(tree: &Tree) -> String {
    let mut lines: Vec<String> = tree
        .iter()
        .map(|(p, f)| format!("{}\0{}", p.display(), f.hash))
        .collect();
    lines.sort();
    hash_bytes(lines.join("\n").as_bytes())
}

fn commit(dir: &Path, id: &str, parents: &[&str], msg: &str, ts: i64, tree: &Tree) -> Result<()> {
    let snap = Snapshot {
        commit: Commit {
            id: id.to_string(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            message: msg.to_string(),
            author: AUTHOR.to_string(),
            timestamp: ts,
            tree_hash: tree_hash(tree),
        },
        files: tree.clone(),
    };
    let bytes = serialization::serialize(&snap)?;
    fs::write(
        dir.join("commits").join(format!("{id}.zst")),
        encode_all(&bytes[..], LEVEL)?,
    )?;
    Ok(())
}

fn main() -> Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: legacy-gen <dir>"))?);
    fs::create_dir_all(dir.join("commits"))?;
    fs::create_dir_all(dir.join("objects"))?;
    fs::create_dir_all(dir.join("refs/heads"))?;

    let mut t1 = Tree::new();
    put(&dir, &mut t1, ".bashrc", 0o644, b"# bashrc v1\nexport EDITOR=vi\n")?;
    put(&dir, &mut t1, ".local/bin/run", 0o755, b"#!/bin/sh\necho run\n")?;
    put(&dir, &mut t1, ".ssh/config", 0o600, b"Host example\n  User legacyuser\n")?;
    put(&dir, &mut t1, ".empty", 0o644, b"")?;
    commit(&dir, C1, &[NULL_COMMIT_ID], "c1: initial", 1_700_000_000, &t1)?;

    let mut t2 = t1.clone();
    put(&dir, &mut t2, ".bashrc", 0o644, b"# bashrc v2\nexport EDITOR=vim\n")?;
    put(&dir, &mut t2, "/home/legacyuser/.config/abs.conf", 0o644, b"abs=true\n")?;
    commit(&dir, C2, &[C1], "c2: edit bashrc, add absolute key", 1_700_000_100, &t2)?;

    let mut ts = t1.clone();
    put(&dir, &mut ts, ".config/side.toml", 0o644, b"side = 1\n")?;
    commit(&dir, SIDE, &[C1], "side: add side.toml", 1_700_000_200, &ts)?;

    let mut t3 = t2.clone();
    put(&dir, &mut t3, ".config/side.toml", 0o644, b"side = 1\n")?;
    commit(&dir, C3, &[C2, SIDE], "c3: merge side into main", 1_700_000_300, &t3)?;

    fs::write(dir.join("refs/heads/main"), C3)?;
    fs::write(dir.join("HEAD"), "ref: refs/heads/main")?;

    let mut m = CommitMapping::new();
    m.add_mapping("origin", C1, "1111111111111111111111111111111111111111");
    m.add_mapping("origin", C2, "2222222222222222222222222222222222222222");
    m.save(&dir.join("remote-mappings.toml"))?;

    Index::new().save(&dir.join("index.bin"))?;
    Ok(())
}
```
