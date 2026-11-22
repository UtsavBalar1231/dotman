use super::{AppState, HeldPaths, Inner, editable, protocol, run_status, status_index, variant};
use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

const LIMIT: usize = 1024 * 1024;
const STALE: &str = "The selected source changed. Keep your draft and reopen the file.";
static TEMP: AtomicU64 = AtomicU64::new(0);

pub(super) struct Document {
    parent: File,
    name: PathBuf,
    original: String,
    file: File,
    snapshot: Metadata,
}

fn descriptor(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn directory(path: &Path) -> Result<File, &'static str> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "Source directory is unavailable or linked")
}

impl Document {
    fn open(repo: &Path, source: &str) -> Result<Self, &'static str> {
        if !repo.is_absolute()
            || source
                .split('/')
                .any(|s| matches!(s, "" | "." | ".." | ".git" | ".dotman" | ".dotman.toml"))
        {
            return Err(STALE);
        }
        let mut parent = directory(Path::new("/"))?;
        for part in repo
            .components()
            .chain(Path::new(source).parent().ok_or(STALE)?.components())
        {
            match part {
                Component::Normal(name) => parent = directory(&descriptor(&parent).join(name))?,
                Component::RootDir => (),
                _ => return Err(STALE),
            }
        }
        let name = PathBuf::from(Path::new(source).file_name().ok_or(STALE)?);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(descriptor(&parent).join(&name))
            .map_err(|_| STALE)?;
        let m = file.metadata().map_err(|_| STALE)?;
        if !m.is_file() || m.mode() & 0o7000 != 0 || m.len() > LIMIT as u64 {
            return Err("Only regular text files up to 1 MiB can be edited");
        }
        let mut original = String::new();
        (&file)
            .take((LIMIT as u64).saturating_add(1))
            .read_to_string(&mut original)
            .map_err(|_| "Source is not UTF-8 text")?;
        if original.len() > LIMIT || original.contains('\0') {
            return Err("Source is binary or too large to edit");
        }
        Ok(Self {
            parent,
            name,
            original,
            file,
            snapshot: m,
        })
    }

    fn unchanged(&self) -> bool {
        self.file.metadata().is_ok_and(|m| {
            self.snapshot.ctime() == m.ctime() && self.snapshot.ctime_nsec() == m.ctime_nsec()
        })
    }

    fn same_version(&self, current: &Self) -> bool {
        self.unchanged()
            && current.unchanged()
            && self.snapshot.dev() == current.snapshot.dev()
            && self.snapshot.ino() == current.snapshot.ino()
            && self.original == current.original
    }

    fn save(&self, current: &Self, text: &str) -> Result<(), String> {
        let now = &current.snapshot;
        if !self.same_version(current) {
            return Err(STALE.into());
        }
        if self.original == text {
            return Ok(());
        }
        let dir = descriptor(&current.parent);
        let target = dir.join(&current.name);
        let tmp_name = format!(
            ".dotman-edit-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = dir.join(&tmp_name);
        let mut copy = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|_| "Cannot prepare source replacement")?;
        let mut prepare = || -> std::io::Result<()> {
            copy.write_all(text.as_bytes())?;
            copy.set_permissions(Permissions::from_mode(now.mode() & 0o777))?;
            copy.sync_all()?;
            let a = CString::new(tmp.as_os_str().as_bytes())?;
            let b = CString::new(target.as_os_str().as_bytes())?;
            dotman_gui::exchange_files(&a, &b)
        };
        if let Err(err) = prepare() {
            let _ = fs::remove_file(&tmp);
            return Err(format!("Could not replace source: {err}"));
        }
        let prior = fs::symlink_metadata(&tmp).map_err(|_| "Cannot verify displaced source")?;
        if prior.dev() != now.dev()
            || prior.ino() != now.ino()
            || prior.mtime() != now.mtime()
            || prior.mtime_nsec() != now.mtime_nsec()
            || prior.len() != now.len()
        {
            return Err(format!(
                "Another save overlapped this one. Its version was retained beside the source as {tmp_name}."
            ));
        }
        fs::remove_file(tmp)
            .and_then(|()| current.parent.sync_all())
            .map_err(|_| "Source replaced; directory cleanup could not be verified".into())
    }
}

fn selection(inner: &Inner, id: &str) -> Result<protocol::DiffSelection, &'static str> {
    if inner.active.is_some() || inner.recovery.is_some() {
        return Err("Another operation needs attention");
    }
    let index = status_index(inner, id).ok_or(STALE)?;
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err(STALE);
    };
    let entry = report.entries.get(index).ok_or(STALE)?;
    editable(entry, &HeldPaths::new(report), &report.context.home.path)
        .ok_or("Source editing is unavailable for this file")?;
    Ok(protocol::DiffSelection {
        context: report.context.clone(),
        entry: entry.clone(),
    })
}

fn checked(sel: &protocol::DiffSelection) -> Result<Document, &'static str> {
    let doc = Document::open(
        &sel.context.repo.path,
        &sel.entry.source.as_ref().ok_or(STALE)?.raw,
    )?;
    let protocol::StatusReply::Report(report) = run_status(&AtomicBool::new(false))? else {
        return Err(STALE);
    };
    let mut rows = report
        .entries
        .iter()
        .filter(|e| e.scope == sel.entry.scope && e.key.raw == sel.entry.key.raw);
    let row = rows.next().ok_or(STALE)?;
    if rows.next().is_some()
        || !variant::same_context(&sel.context, &report.context)
        || row.source != sel.entry.source
        || row.source_tuple != sel.entry.source_tuple
        || editable(row, &HeldPaths::new(&report), &report.context.home.path).is_none()
    {
        return Err(STALE);
    }
    let current = Document::open(
        &report.context.repo.path,
        &row.source.as_ref().ok_or(STALE)?.raw,
    )?;
    if !doc.same_version(&current) {
        return Err(STALE);
    }
    Ok(current)
}

#[tauri::command]
pub async fn open_source(id: String, state: tauri::State<'_, AppState>) -> Result<String, String> {
    let shared = state.inner.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let sel = {
            let inner = shared.lock().map_err(|_| STALE)?;
            selection(&inner, &id)?
        };
        let doc = checked(&sel)?;
        let mut inner = shared.lock().map_err(|_| STALE)?;
        selection(&inner, &id)?;
        let text = doc.original.clone();
        inner.source_doc = Some((id, Arc::new(doc)));
        drop(inner);
        Ok(text)
    })
    .await
    .map_err(|_| "Unable to open source")?
}

#[tauri::command]
pub async fn save_source(
    id: String,
    text: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if text.len() > LIMIT || text.contains('\0') {
        return Err("Only text up to 1 MiB can be saved".into());
    }
    let shared = state.inner.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (sel, doc) = {
            let inner = shared.lock().map_err(|_| STALE)?;
            let (_, doc) = inner
                .source_doc
                .as_ref()
                .filter(|(key, _)| key == &id)
                .ok_or(STALE)?;
            let selection = (selection(&inner, &id)?, Arc::clone(doc));
            drop(inner);
            selection
        };
        let path = &sel.entry.source.as_ref().ok_or(STALE)?.shown;
        if !crate::confirm::ask(
            &window,
            "Save repository source?",
            "Save source",
            format!(
                "Save edits to {path} in {}? The Home copy is unchanged.",
                sel.context.repo.shown
            ),
            Arc::new(AtomicBool::new(false)),
        )? {
            return Err("Save cancelled. Your draft is still open.".into());
        }
        let current = checked(&sel)?;
        let mut inner = shared.lock().map_err(|_| STALE)?;
        selection(&inner, &id)?;
        if !inner
            .source_doc
            .as_ref()
            .is_some_and(|(_, d)| Arc::ptr_eq(&doc, d))
        {
            return Err(STALE.into());
        }
        doc.save(&current, &text)?;
        inner.source_doc = None;
        inner.reply = None;
        inner.review = None;
        inner.diff_receipts.clear();
        drop(inner);
        Ok(())
    })
    .await
    .map_err(|_| "Unable to save source")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn edits_only_the_reviewed_regular_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("dotman-source-test-{}", std::process::id()));
        fs::create_dir(&dir)?;
        let path = dir.join("sample");
        fs::write(&path, "before\r\n")?;
        fs::set_permissions(&path, Permissions::from_mode(0o750))?;
        let doc = Document::open(&dir, "sample")?;
        assert_eq!(doc.original, "before\r\n");
        doc.save(&Document::open(&dir, "sample")?, "after\r\n")?;
        assert_eq!(fs::read(&path)?, b"after\r\n");
        assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o750);
        assert!(doc.save(&Document::open(&dir, "sample")?, "retry").is_err());
        let reopened = Document::open(&dir, "sample")?;
        fs::write(&path, "concurrent change")?;
        assert!(
            reopened
                .save(&Document::open(&dir, "sample")?, "draft")
                .is_err()
        );
        assert_eq!(fs::read(&path)?, b"concurrent change");
        symlink(&path, dir.join("link"))?;
        symlink(&dir, dir.join("loop"))?;
        for invalid in ["link", "loop/sample", "../sample", "/sample", ".git/config"] {
            assert!(Document::open(&dir, invalid).is_err());
        }
        fs::write(&path, [0_u8, 1])?;
        assert!(Document::open(&dir, "sample").is_err());
        fs::write(&path, [0xff])?;
        assert!(Document::open(&dir, "sample").is_err());
        File::create(&path)?.set_len((LIMIT as u64).saturating_add(1))?;
        assert!(Document::open(&dir, "sample").is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
