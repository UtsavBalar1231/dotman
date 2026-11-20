use crate::protocol::{
    self, Header, ResultRecord, StatusContext, StatusReply, StatusReport, StatusText,
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;

const MAX_DOCUMENT: usize = 1_048_576;

#[path = "setup_import.rs"]
mod import;
pub use import::{ImportIntent, ReviewedImport};

/// An absent destination and its pinned, checked parent.
pub struct SetupIntent {
    context: StatusContext,
    parent: Arc<File>,
    dev: u64,
    ino: u64,
    source: Option<CloneSource>,
}

/// A literal clone URI accepted by the guarded CLI protocol.
#[derive(Clone, PartialEq, Eq)]
pub struct CloneSource(String);

impl CloneSource {
    /// Validates a URI without probing the source or Git installation.
    ///
    /// # Errors
    /// Rejects sources outside the guarded clone grammar.
    pub fn parse(source: String) -> Result<Self, String> {
        if source.len() > 8192 {
            return Err("clone source is too long".into());
        }
        let (protocol, path) = if let Some(path) = source.strip_prefix("file://") {
            ("file", path)
        } else if let Some(rest) = source.strip_prefix("https://") {
            let (authority, path) = rest.split_once('/').ok_or("invalid clone source")?;
            let (host, port) = authority
                .split_once(':')
                .map_or((authority, None), |(host, port)| (host, Some(port)));
            if let Some(port) = port
                && (port.is_empty()
                    || !port.bytes().all(|b| b.is_ascii_digit())
                    || !port.parse::<u16>().is_ok_and(|n| n != 0))
            {
                return Err("invalid clone port".into());
            }
            let ipv4 = host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.to_string() == host);
            let dns = host.len() <= 253
                && host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                && host.rsplit('.').next().is_some_and(|last| {
                    !last.bytes().all(|b| b.is_ascii_digit())
                        && !last.to_ascii_lowercase().starts_with("0x")
                });
            if !ipv4 && !dns {
                return Err("invalid clone host".into());
            }
            ("https", path)
        } else {
            return Err("invalid clone source".into());
        };
        let path = if protocol == "file" {
            path.strip_prefix('/').ok_or("invalid file source")?
        } else {
            path
        };
        if !path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        }) {
            return Err("invalid clone path".into());
        }
        Ok(Self(source))
    }
}

fn literal(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes.starts_with(b"/")
        && bytes.len() > 1
        && !bytes.contains(&0)
        && bytes.get(1..).is_some_and(|rest| {
            rest.split(|byte| *byte == b'/')
                .all(|part| !matches!(part, b"" | b"." | b".."))
        })
}

impl SetupIntent {
    /// Checks the unavailable status and opens the exact reported destination parent.
    ///
    /// # Errors
    /// Rejects ineligible status, nonliteral paths, unsafe parents and occupied destinations.
    pub fn from_status(reply: &StatusReply) -> Result<Self, String> {
        Self::checked_status(reply).map_err(|error| error.to_string())
    }

    fn checked_status(reply: &StatusReply) -> io::Result<Self> {
        let StatusReply::Error {
            exit_code: 1,
            context: Some(context),
            error_kind: Some(kind),
            ..
        } = reply
        else {
            return Err(io::Error::other("status is not eligible for fresh setup"));
        };
        context.validate("status").map_err(io::Error::other)?;
        if kind != "other"
            || context.repo_id.is_some()
            || !literal(&context.home.path)
            || !literal(&context.repo.path)
        {
            return Err(io::Error::other("invalid fresh setup context"));
        }
        let path = context
            .repo
            .path
            .parent()
            .ok_or_else(|| io::Error::other("missing parent"))?;
        if fs::canonicalize(path)?.as_os_str() != path.as_os_str() {
            return Err(io::Error::other("parent is not canonical"));
        }
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = parent.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != crate::sys::effective_uid()
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::other("unsafe parent"));
        }
        match fs::symlink_metadata(&context.repo.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::other("destination exists")),
            Err(error) => return Err(error),
        }
        let current = fs::metadata(path)?;
        if fs::canonicalize(path)?.as_os_str() != path.as_os_str()
            || (current.dev(), current.ino()) != (metadata.dev(), metadata.ino())
        {
            return Err(io::Error::other("parent changed"));
        }
        Ok(Self {
            context: context.clone(),
            parent: Arc::new(parent),
            dev: metadata.dev(),
            ino: metadata.ino(),
            source: None,
        })
    }

    /// Selects clone-only using the validated literal source.
    #[must_use]
    pub fn for_clone(mut self, source: CloneSource) -> Self {
        self.source = Some(source);
        self
    }

    /// The literal clone source, if this is a clone intent.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_ref().map(|source| source.0.as_str())
    }

    /// The original status context.
    #[must_use]
    pub const fn context(&self) -> &StatusContext {
        &self.context
    }

    pub(crate) fn parent(&self) -> Arc<File> {
        Arc::clone(&self.parent)
    }

    /// Encodes the bounded guarded setup request.
    ///
    /// # Errors
    /// Rejects serialization failures or documents larger than the CLI limit.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let document = Request {
            schema_version: 1,
            home: input_path(&self.context.home.path),
            repo: input_path(&self.context.repo.path),
            parent_dev: self.dev,
            parent_ino: self.ino,
            operation: self
                .source()
                .map_or(Operation::Fresh, |source| Operation::Clone { source }),
        };
        let mut output = Bounded(Vec::new());
        serde_json::to_writer(&mut output, &document).map_err(|_| "invalid setup request")?;
        Ok(output.0)
    }

    /// Parses a complete successful CLI receipt, without treating it as corroboration.
    ///
    /// # Errors
    /// Rejects incomplete, malformed, mismatched or unsuccessful output.
    pub fn parse_publication(
        &self,
        stdout: &[u8],
        exit: Option<i32>,
    ) -> Result<SetupPublication, String> {
        if exit != Some(0) || stdout.len() > protocol::MAX_REPORT || !stdout.ends_with(b"\n") {
            return Err("invalid setup output or exit".into());
        }
        let mut lines = stdout
            .strip_suffix(b"\n")
            .ok_or("missing newline")?
            .split(|byte| *byte == b'\n');
        let [Some(context), Some(setup), Some(result), None] =
            [lines.next(), lines.next(), lines.next(), lines.next()]
        else {
            return Err("invalid setup framing".into());
        };
        for (line, name) in [(context, "context"), (setup, "setup"), (result, "result")] {
            if line.is_empty() || line.len() > protocol::MAX_RECORD || line.ends_with(b"\r") {
                return Err("invalid setup record".into());
            }
            let header: Header = serde_json::from_slice(line).map_err(|_| "invalid header")?;
            if header.record != name {
                return Err("unexpected setup record".into());
            }
        }
        let context: StatusContext =
            serde_json::from_slice(context).map_err(|_| "invalid context")?;
        context.validate("init")?;
        if context.repo_id.is_some() || !same_paths_and_machine(&self.context, &context) {
            return Err("setup context changed".into());
        }
        let setup: SetupRecord = serde_json::from_slice(setup).map_err(|_| "invalid setup")?;
        if setup.operation
            != if self.source.is_some() {
                "clone"
            } else {
                "fresh"
            }
            || !valid_id(&setup.repo_id.raw)
        {
            return Err("invalid setup identity".into());
        }
        let result: ResultRecord = serde_json::from_slice(result).map_err(|_| "invalid result")?;
        if result.exit_code != 0
            || result.outcome != "done"
            || result.error_kind.is_some()
            || result.recovery.is_some()
        {
            return Err("invalid setup result".into());
        }
        let mut context = self.context.clone();
        context.repo_id = Some(setup.repo_id);
        Ok(SetupPublication {
            context,
            source: self.source.clone(),
        })
    }

    /// Checks a bound publication against a complete fresh status report.
    #[must_use]
    pub fn is_published_in(&self, publication: &SetupPublication, after: &StatusReport) -> bool {
        after.context.validate("status").is_ok()
            && same_paths_and_machine(&self.context, &publication.context)
            && self.source == publication.source
            && same_paths_and_machine(&self.context, &after.context)
            && after.context.repo_id.as_ref().map(|id| &id.raw)
                == publication.context.repo_id.as_ref().map(|id| &id.raw)
    }
}

fn same_paths_and_machine(a: &StatusContext, b: &StatusContext) -> bool {
    let paths = [
        (&a.home.path, &b.home.path),
        (&a.repo.path, &b.repo.path),
        (&a.config.path, &b.config.path),
        (&a.repo_config.path, &b.repo_config.path),
        (&a.state.path, &b.state.path),
        (&a.identity.path, &b.identity.path),
    ];
    paths
        .into_iter()
        .all(|(a, b)| a.as_os_str().as_bytes() == b.as_os_str().as_bytes())
        && a.age_key_file
            .as_ref()
            .map(|p| p.path.as_os_str().as_bytes())
            == b.age_key_file
                .as_ref()
                .map(|p| p.path.as_os_str().as_bytes())
        && protocol::same_identity_raw(&a.machine, &b.machine)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A successful bound receipt, requiring a fresh status before success is displayed.
pub struct SetupPublication {
    context: StatusContext,
    source: Option<CloneSource>,
}

impl SetupPublication {
    /// The approved status context with the newly published repository ID.
    #[must_use]
    pub const fn context(&self) -> &StatusContext {
        &self.context
    }
}

#[derive(Deserialize)]
struct SetupRecord {
    operation: String,
    repo_id: StatusText,
}

#[derive(Serialize)]
#[serde(untagged)]
enum InputPath<'a> {
    Raw { raw: &'a str },
    Bytes { bytes: &'a [u8] },
}

fn input_path(path: &Path) -> InputPath<'_> {
    path.to_str().map_or_else(
        || InputPath::Bytes {
            bytes: path.as_os_str().as_bytes(),
        },
        |raw| InputPath::Raw { raw },
    )
}

#[derive(Serialize)]
struct Request<'a> {
    schema_version: u8,
    home: InputPath<'a>,
    repo: InputPath<'a>,
    parent_dev: u64,
    parent_ino: u64,
    operation: Operation<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Operation<'a> {
    Fresh,
    Clone { source: &'a str },
}

struct Bounded(Vec<u8>);

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_DOCUMENT.saturating_sub(self.0.len()) {
            return Err(io::Error::other("setup request too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const DONE: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
    const FAIL: &str =
        r#"{"record":"result","outcome":"error","exit_code":1,"error_kind":"other"}"#;
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    pub struct Fixture(pub PathBuf);

    impl Fixture {
        pub(crate) fn new() -> io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "dotman-gui-setup-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            Ok(Self(path))
        }

        pub(crate) fn context(&self, id: Option<&str>) -> serde_json::Value {
            let path = |path: &Path| serde_json::json!({"raw": path.to_str(), "shown":"fixture"});
            let text = |value: &str| serde_json::json!({"raw":value,"shown":value});
            serde_json::json!({
                "record":"context", "schema_version":1, "experimental":true, "command":"status",
                "home":path(&self.0), "repo":path(&self.0.join("repo")),
                "config":path(&self.0.join("config")), "repo_config":path(&self.0.join("repo/.dotman.toml")),
                "state":path(&self.0.join("state")), "identity":path(&self.0.join("identity")),
                "age_key_file":null, "repo_id":id.map(text),
                "machine":{"host":text("h"),"user":text("u"),"os":text("linux"),"distro":text(""),"like":text(""),"arch":text("x"),"profiles":[]}
            })
        }

        pub(crate) fn status(&self) -> Result<StatusReply, String> {
            let input = format!("{}\n{FAIL}\n", self.context(None));
            protocol::parse_status(input.as_bytes(), Some(1))
        }

        fn published(&self, id: &str) -> Result<StatusReply, String> {
            let review = r#"{"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}}"#;
            let summary = r#"{"record":"summary","rows":0,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
            let input = format!("{}\n{review}\n{summary}\n{DONE}\n", self.context(Some(id)));
            protocol::parse_status(input.as_bytes(), Some(0))
        }

        fn receipt(&self) -> String {
            let mut context = self.context(None);
            if let Some(command) = context.get_mut("command") {
                *command = "init".into();
            }
            let setup = serde_json::json!({"record":"setup","operation":"fresh","repo_id":{"raw":ID,"shown":ID}});
            format!("{context}\n{setup}\n{DONE}\n")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove fixture");
        }
    }

    #[test]
    fn clone_source_matches_guarded_grammar() {
        for source in [
            "https://example.com/repo",
            "https://Example.COM:65535/a-b._~/repo",
            "https://127.0.0.1/repo",
            "file:///absolute/path",
        ] {
            assert!(CloneSource::parse(source.into()).is_ok(), "{source}");
        }
        for source in [
            "https://user@example.com/repo",
            "https://example.com/a//b",
            "https://example.com/../repo",
            "https://example.com/a%2fb",
            "https://127.1/repo",
            "https://0x7f000001/repo",
            "https://127.00.0.1/repo",
            "https://example.com:0/repo",
            "https://example.com:65536/repo",
            "https://example.com/repo?q=x",
            "https://example.com/repo#x",
            "file://localhost/repo",
            "file:///a/../b",
            "ssh://example.com/repo",
            "https://example.com\\evil/repo",
            "https://example.0Xaa/repo",
            "https://-example.com/repo",
            "https://example.com/é",
        ] {
            assert!(CloneSource::parse(source.into()).is_err(), "{source}");
        }
        let prefix = "https://example.invalid/";
        let limit = format!(
            "{prefix}{}",
            "a".repeat(8192_usize.saturating_sub(prefix.len()))
        );
        assert!(CloneSource::parse(limit.clone()).is_ok());
        assert!(CloneSource::parse(format!("{limit}a")).is_err());
        assert!(CloneSource::parse(format!("https://{}.com/repo", "a".repeat(64))).is_err());
    }

    #[test]
    fn clone_request_receipt_and_parent_pin() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let fresh = SetupIntent::from_status(&fixture.status()?)?;
        let pinned = fresh.parent();
        let source = "https://example.com:443/a-B_~.git";
        let clone = fresh.for_clone(CloneSource::parse(source.into())?);
        assert!(Arc::ptr_eq(&pinned, &clone.parent()));
        assert_eq!(clone.source(), Some(source));
        let value: serde_json::Value = serde_json::from_slice(&clone.encode()?)?;
        assert_eq!(
            value.get("operation"),
            Some(&serde_json::json!({"kind":"clone","source":source}))
        );
        let fresh_receipt = fixture.receipt();
        assert!(
            clone
                .parse_publication(fresh_receipt.as_bytes(), Some(0))
                .is_err()
        );
        let clone_receipt = fresh_receipt.replace("\"fresh\"", "\"clone\"");
        let publication = clone.parse_publication(clone_receipt.as_bytes(), Some(0))?;
        assert_eq!(
            publication
                .context()
                .repo_id
                .as_ref()
                .map(|id| id.raw.as_str()),
            Some(ID)
        );
        let StatusReply::Report(report) = fixture.published(ID)? else {
            return Err("missing report".into());
        };
        assert!(clone.is_published_in(&publication, &report));
        let other = SetupIntent::from_status(&fixture.status()?)?;
        assert!(!other.is_published_in(&publication, &report));
        let other_clone = other.for_clone(CloneSource::parse("https://other.example/repo".into())?);
        assert!(!other_clone.is_published_in(&publication, &report));
        assert!(
            clone
                .parse_publication(clone_receipt.replace(DONE, "").as_bytes(), Some(0))
                .is_err()
        );
        assert!(
            clone
                .parse_publication(
                    clone_receipt
                        .replace("\"raw\":\"h\"", "\"raw\":\"different\"")
                        .as_bytes(),
                    Some(0)
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn eligibility_and_request() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let status = fixture.status()?;
        let setup = SetupIntent::from_status(&status)?;
        let value: serde_json::Value = serde_json::from_slice(&setup.encode()?)?;
        let metadata = fs::metadata(&fixture.0)?;
        assert_eq!(
            value,
            serde_json::json!({"schema_version":1,"home":{"raw":fixture.0.to_str()},"repo":{"raw":fixture.0.join("repo").to_str()},"parent_dev":metadata.dev(),"parent_ino":metadata.ino(),"operation":{"kind":"fresh"}})
        );
        assert_eq!(setup.encode()?, setup.encode()?);
        let known = format!("{}\n{FAIL}\n", fixture.context(Some(ID)));
        assert!(
            SetupIntent::from_status(&protocol::parse_status(known.as_bytes(), Some(1))?).is_err()
        );
        fs::write(fixture.0.join("repo"), b"existing")?;
        assert!(SetupIntent::from_status(&status).is_err());
        fs::remove_file(fixture.0.join("repo"))?;
        symlink("missing", fixture.0.join("repo"))?;
        assert!(SetupIntent::from_status(&status).is_err());
        fs::remove_file(fixture.0.join("repo"))?;
        fs::create_dir(fixture.0.join("repo"))?;
        assert!(SetupIntent::from_status(&status).is_err());
        fs::remove_dir(fixture.0.join("repo"))?;
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o720))?;
        assert!(SetupIntent::from_status(&status).is_err());
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700))?;
        assert!(!literal(Path::new("/a/../b")));
        assert!(!literal(Path::new("/a//b")));
        let mut too_large = Bounded(Vec::new());
        assert!(
            too_large
                .write_all(&vec![0; MAX_DOCUMENT.saturating_add(1)])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn byte_paths_and_symlink_parent() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let name = OsString::from_vec(b"repo-\xff".to_vec());
        let repo = fixture.0.join(name);
        let mut context = fixture.context(None);
        *context.get_mut("repo").ok_or("repo")? = serde_json::json!({
            "bytes": repo.as_os_str().as_bytes(), "shown":"fixture"
        });
        let input = format!("{context}\n{FAIL}\n");
        let status = protocol::parse_status(input.as_bytes(), Some(1))?;
        let setup = SetupIntent::from_status(&status)?;
        let value: serde_json::Value = serde_json::from_slice(&setup.encode()?)?;
        assert_eq!(
            value.pointer("/repo/bytes"),
            Some(&serde_json::json!(repo.as_os_str().as_bytes()))
        );

        let alias = fixture.0.join("alias");
        symlink(&fixture.0, &alias)?;
        *context.get_mut("repo").ok_or("repo")? = serde_json::json!({
            "raw":alias.join("new").to_str(), "shown":"fixture"
        });
        let input = format!("{context}\n{FAIL}\n");
        let status = protocol::parse_status(input.as_bytes(), Some(1))?;
        assert!(SetupIntent::from_status(&status).is_err());
        Ok(())
    }

    #[test]
    fn receipt_requires_complete_binding_and_fresh_status() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new()?;
        let setup = SetupIntent::from_status(&fixture.status()?)?;
        let good = fixture.receipt();
        let receipt = setup.parse_publication(good.as_bytes(), Some(0))?;
        let StatusReply::Report(report) = fixture.published(ID)? else {
            return Err("missing report".into());
        };
        assert!(setup.is_published_in(&receipt, &report));
        let StatusReply::Report(other) = fixture.published("1123456789abcdef0123456789abcdef")?
        else {
            return Err("missing report".into());
        };
        assert!(!setup.is_published_in(&receipt, &other));
        let other_fixture = Fixture::new()?;
        let other_setup = SetupIntent::from_status(&other_fixture.status()?)?;
        assert!(!other_setup.is_published_in(&receipt, &report));
        for bad in [
            good.trim_end().to_owned(),
            good.replace(DONE, ""),
            good.replace("\"fresh\"", "\"clone\""),
            good.replace(ID, "INVALID"),
            good.replace("\"command\":\"init\"", "\"command\":\"status\""),
            good.replace("\"schema_version\":1", "\"schema_version\":2"),
            good.replace("\"raw\":\"h\"", "\"raw\":\"other\""),
            good.replace(DONE, FAIL),
            format!("{good}{DONE}\n"),
        ] {
            assert!(
                setup.parse_publication(bad.as_bytes(), Some(0)).is_err(),
                "{bad}"
            );
        }
        assert!(setup.parse_publication(good.as_bytes(), None).is_err());
        assert!(setup.parse_publication(good.as_bytes(), Some(1)).is_err());
        assert!(
            setup
                .parse_publication(&vec![b'x'; protocol::MAX_REPORT.saturating_add(1)], Some(0))
                .is_err()
        );
        Ok(())
    }
}
