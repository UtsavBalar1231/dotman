use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn supports_non_utf8(dir: &Path) -> io::Result<bool> {
    let path = dir.join(std::ffi::OsStr::from_bytes(b".dotman-byte-probe-\xff"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => {
            drop(file);
            fs::remove_file(path)?;
            Ok(true)
        }
        Err(err) if err.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("filesystem rejects non-UTF-8 names: {err}");
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

pub fn supports_case_twins(dir: &Path) -> io::Result<bool> {
    let lower = dir.join(".dotman-case-probe");
    let upper = dir.join(".DOTMAN-CASE-PROBE");
    let first = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lower)?;
    let second = OpenOptions::new().write(true).create_new(true).open(&upper);
    drop(first);
    fs::remove_file(&lower)?;
    match second {
        Ok(file) => {
            drop(file);
            fs::remove_file(upper)?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!("filesystem does not distinguish case twins");
            Ok(false)
        }
        Err(err) => Err(err),
    }
}
