//! Folder-local ignore rules for Home inventories.

use super::{Git, NO_TEMPLATE, failure};
use anyhow::{Context as _, Result, ensure};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

// A single scan may copy at most the size Git accepts in one ignore file.
const MAX_RULE_BYTES: u64 = 100 * 1024 * 1024;

impl Git {
    pub(crate) fn folder_ignores<'a>(
        &self,
        home: &Path,
        folder: &Path,
        candidates: impl IntoIterator<Item = (&'a Path, bool)>,
    ) -> Result<BTreeSet<PathBuf>> {
        let candidates: Vec<_> = candidates.into_iter().collect();
        let mut directories = BTreeSet::new();
        for &(path, _) in &candidates {
            check_relative(path)?;
            for dir in path.ancestors().skip(1) {
                if !directories.insert(dir) {
                    break;
                }
            }
        }
        if directories.is_empty() {
            return Ok(BTreeSet::new());
        }
        check_relative(folder)?;
        let mut rules = Rules::default();
        {
            let home = File::options()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(home)?;
            let root = open_directory(&home, folder)?;
            for dir in directories {
                let parent = open_directory(&root, dir)?;
                if crate::sys::kind_at(parent.as_fd(), c".gitignore")?
                    == Some(crate::sys::EntryKind::File)
                {
                    let file = crate::sys::openat_read(parent.as_fd(), c".gitignore")?;
                    let metadata = file.metadata()?;
                    ensure!(metadata.is_file(), "folder ignore file changed");
                    ensure!(
                        metadata.len() <= MAX_RULE_BYTES.saturating_sub(rules.bytes),
                        "folder ignore rules exceed the size limit"
                    );
                    rules.add(&dir.join(".gitignore"), file)?;
                }
            }
        }
        rules.matches(self, candidates)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn folder_ignores_from_rules<'a>(
        &self,
        snapshots: &BTreeMap<PathBuf, Vec<u8>>,
        candidates: impl IntoIterator<Item = (&'a Path, bool)>,
    ) -> Result<BTreeSet<PathBuf>> {
        let mut rules = Rules::default();
        for (path, bytes) in snapshots {
            rules.add(path, bytes.as_slice())?;
        }
        rules.matches(self, candidates)
    }
}

fn open_directory(root: &File, path: &Path) -> Result<File> {
    let mut dir = root.try_clone()?;
    for name in path {
        dir = File::from(crate::sys::openat_dir(
            dir.as_fd(),
            &CString::new(name.as_bytes())?,
        )?);
    }
    Ok(dir)
}

fn check_relative(path: &Path) -> Result<()> {
    let valid = path.components().all(|component| match component {
        Component::Normal(name) => {
            !name.as_bytes().eq_ignore_ascii_case(b".git") && !name.as_bytes().contains(&0)
        }
        _ => false,
    });
    ensure!(
        !path.as_os_str().is_empty() && valid,
        "invalid relative folder path"
    );
    Ok(())
}

// Only .gitignore files enter this private worktree. Git never discovers the source tree's
// repository, opens a special file there, or consults its index to determine a path's kind.
#[derive(Default)]
struct Rules {
    scope: Option<TempDir>,
    bytes: u64,
    directories: BTreeSet<PathBuf>,
}

impl Rules {
    fn add(&mut self, path: &Path, input: impl Read) -> Result<()> {
        check_relative(path)?;
        ensure!(
            path.file_name() == Some(OsStr::new(".gitignore")),
            "invalid rule file"
        );
        let scope = match &self.scope {
            Some(scope) => scope,
            None => self.scope.insert(crate::paths::private_temp_dir(
                ".dotman-tmp-ignore-",
                &std::env::temp_dir(),
            )?),
        };
        let destination = scope.path().join("work").join(path);
        fs::create_dir_all(destination.parent().context("missing rule directory")?)?;
        let mut file = File::create_new(destination)?;
        let left = MAX_RULE_BYTES.saturating_sub(self.bytes);
        let count = io::copy(&mut input.take(left.saturating_add(1)), &mut file)?;
        ensure!(count <= left, "folder ignore rules exceed the size limit");
        self.bytes = self.bytes.saturating_add(count);
        self.directories
            .insert(path.parent().context("missing rule directory")?.to_owned());
        Ok(())
    }

    fn matches<'a>(
        &self,
        git: &Git,
        candidates: impl IntoIterator<Item = (&'a Path, bool)>,
    ) -> Result<BTreeSet<PathBuf>> {
        let Some(scope) = &self.scope else {
            return Ok(BTreeSet::new());
        };
        let command = |dir: &Path| -> Command {
            let mut cmd = git.base();
            cmd.env_clear();
            if let Some(path) = std::env::var_os("PATH") {
                cmd.env("PATH", path);
            }
            cmd.current_dir(dir)
                .env("HOME", scope.path())
                .env("XDG_CONFIG_HOME", scope.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null");
            cmd
        };
        let init_args = ["init", "--bare", "-q", NO_TEMPLATE, "db"];
        let mut init = command(scope.path());
        init.args(init_args);
        let out = git.spawn(init)?;
        ensure!(out.status.success(), "{}", failure(&init_args, &out));

        let mut input = BufWriter::new(tempfile::tempfile_in(scope.path())?);
        for (path, directory) in candidates {
            check_relative(path)?;
            if !path
                .ancestors()
                .skip(1)
                .any(|ancestor| self.directories.contains(ancestor))
            {
                continue;
            }
            if directory {
                // A trailing slash would query a child of this directory: e.g. "foo/*"
                // matches "foo/" even though it does not exclude the directory itself.
                fs::create_dir_all(scope.path().join("work").join(path))?;
            }
            // The ./ prefix protects a leading colon from Git's pathspec parser.
            input.write_all(b"./")?;
            input.write_all(path.as_os_str().as_bytes())?;
            input.write_all(&[0])?;
        }
        input.flush()?;
        let mut input = input.into_inner()?;
        input.rewind()?;
        let args = ["check-ignore", "--no-index", "-z", "--stdin"];
        let mut cmd = command(&scope.path().join("work"));
        cmd.args([
            "--git-dir=../db",
            "--work-tree=.",
            "-c",
            "core.ignorecase=false",
        ])
        .args(args);
        let out = git.spawn_with(cmd, Stdio::from(input))?;
        ensure!(
            matches!(out.status.code(), Some(0 | 1)),
            "{}",
            failure(&args, &out)
        );
        out.stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                let relative = path
                    .strip_prefix(b"./")
                    .context("invalid path from git check-ignore")?;
                Ok(PathBuf::from(OsStr::from_bytes(relative)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn only_rules_below_the_selection_apply_including_inside_embedded_repositories() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("parent/selected");
        let managed = fixture.path().join("managed");
        fs::create_dir_all(root.join(".omca/.git"))?;
        fs::create_dir_all(root.join(".omca/rules"))?;
        fs::create_dir_all(root.join(".omca/state"))?;
        fs::create_dir_all(root.join("cache"))?;
        fs::create_dir_all(managed.join(".git"))?;
        fs::write(fixture.path().join("parent/.gitignore"), "*\n")?;
        fs::write(root.join(".gitignore"), "*.log\ncache/\n")?;
        fs::write(root.join(".omca/.gitignore"), "*\n!/rules/\n!/rules/**\n")?;
        for repo in [&managed, &root.join(".omca")] {
            fs::create_dir_all(repo.join(".git/objects"))?;
            fs::create_dir_all(repo.join(".git/refs"))?;
            fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n")?;
            fs::write(repo.join(".git/config"), "[invalid")?;
            fs::write(repo.join(".git/index"), b"original index")?;
        }
        let git = Git::new(&managed, "test").with_env([
            ("HOME", fixture.path().as_os_str()),
            (
                "GIT_CONFIG_PARAMETERS",
                OsStr::new("'core.ignorecase=true'"),
            ),
        ]);
        let candidates = [
            ("keep", false),
            ("keep.LOG", false),
            (":test.log", false),
            ("line\nbreak.log", false),
            ("cache", true),
            ("cache/file", false),
            (".omca", true),
            (".omca/.gitignore", false),
            (".omca/rules", true),
            (".omca/rules/build", false),
            (".omca/state", true),
            (".omca/state/tool", false),
        ];
        let ignored = git.folder_ignores(
            fixture.path(),
            Path::new("parent/selected"),
            candidates.map(|(p, d)| (Path::new(p), d)),
        )?;
        assert_eq!(
            ignored,
            [
                ":test.log",
                "line\nbreak.log",
                "cache",
                "cache/file",
                ".omca/.gitignore",
                ".omca/state",
                ".omca/state/tool"
            ]
            .map(PathBuf::from)
            .into()
        );
        for repo in [&managed, &root.join(".omca")] {
            assert_eq!(fs::read(repo.join(".git/index"))?, b"original index");
            assert_eq!(fs::read(repo.join(".git/config"))?, b"[invalid");
        }
        Ok(())
    }

    #[test]
    fn symlinks_and_fifos_cannot_supply_folder_rules() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("selected");
        fs::create_dir_all(root.join("child"))?;
        let outside = fixture.path().join("outside");
        fs::write(&outside, "*\n")?;
        symlink(outside, root.join(".gitignore"))?;
        let fifo = crate::proc::command("mkfifo")
            .arg(root.join("child/.gitignore"))
            .status()?;
        assert!(fifo.success());
        let git = Git::new(fixture.path().join("managed"), "test");
        assert!(
            git.folder_ignores(
                fixture.path(),
                Path::new("selected"),
                [(Path::new("child/file"), false)]
            )?
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn symlinked_directories_cannot_supply_ancestor_rules() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let home = fixture.path().join("home");
        fs::create_dir_all(home.join("selected"))?;
        let outside = fixture.path().join("outside");
        fs::create_dir_all(outside.join("sub"))?;
        fs::write(outside.join("sub/.gitignore"), "*")?;
        symlink(outside, home.join("selected/link"))?;
        let git = Git::new(fixture.path().join("unused"), "test");
        let result = git.folder_ignores(
            &home,
            Path::new("selected"),
            [(Path::new("link/sub/file"), false)],
        );
        assert!(result.is_err());
        let result = git.folder_ignores(
            &home,
            Path::new("selected/link/sub"),
            [(Path::new("file"), false)],
        );
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn private_rule_copies_share_a_single_size_limit_and_are_cleaned_up() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut rules = Rules {
            bytes: MAX_RULE_BYTES.saturating_sub(3),
            ..Rules::default()
        };
        rules.add(Path::new(".gitignore"), b"ab".as_slice())?;
        let scope = rules
            .scope
            .as_ref()
            .context("missing private directory")?
            .path()
            .to_owned();
        assert_eq!(scope.metadata()?.permissions().mode() & 0o777, 0o700);
        assert!(
            rules
                .add(Path::new("sub/.gitignore"), b"cd".as_slice())
                .is_err()
        );
        drop(rules);
        assert!(!scope.exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn snapshots_match_literal_path_bytes_and_refuse_rule_paths_outside_the_folder() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let git = Git::new(fixture.path(), "test");
        let raw = Path::new(OsStr::from_bytes(b":name\xff"));
        let rules = [(PathBuf::from(".gitignore"), b"*\n!keep\n".to_vec())].into();
        let paths = [(raw, false), (Path::new("keep"), false)];
        assert_eq!(
            git.folder_ignores_from_rules(&rules, paths)?,
            [raw.to_owned()].into()
        );
        let outside = [(PathBuf::from("../.gitignore"), b"*".to_vec())].into();
        assert!(git.folder_ignores_from_rules(&outside, paths).is_err());
        Ok(())
    }
}
