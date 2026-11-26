//! Size-capped logs with one rotated `.1` file (issue #45).
//!
//! Service logs are rotated by **copy and truncate**, never by rename. A
//! running service writes its log through a descriptor (Unix) or handle
//! (Windows) it inherited from `taba run`, which opened the file in append
//! mode (`O_APPEND`; `FILE_APPEND_DATA` without `FILE_WRITE_DATA`). taba
//! cannot reopen that descriptor inside another process, so:
//!
//! - renaming the file would leave the service writing into `<name>.log.1`
//!   forever (Unix) and may be refused on Windows while a reader holds the
//!   file without `FILE_SHARE_DELETE`;
//! - copying the contents to `<name>.log.1` and truncating the original in
//!   place keeps the path and the file identity. Because every write of the
//!   service is an append, the kernel places its next write at the new end
//!   of file (offset 0 after the truncate) on macOS, Linux and Windows alike,
//!   so the service keeps logging without a gap or a sparse hole, and
//!   readers that follow the path (`taba logs -f`, the TUI) only see the
//!   length drop and restart from the beginning.
//!
//! The price is the classic copytruncate window: output written between the
//! last read of the copy and the truncate (microseconds) can be lost. When
//! no process writes the log (`taba run` rotates before it starts the new
//! process) nothing can be lost.
//!
//! `events.jsonl` is opened, appended to and closed by each taba process
//! under the global lock, so it is rotated by a plain rename.

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// `<file>.1`, the single rotated generation of `path`.
pub fn rotated(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = rotated(path).into_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

fn len_of(path: &Path) -> io::Result<Option<u64>> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some(m.len())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Moves the contents of `path` to `path.1` (replacing it) and truncates
/// `path` in place when it is larger than `max` bytes. `Ok(false)` when
/// nothing was due. Call under the global lock so that two taba processes do
/// not rotate the same file at once.
///
/// On an error before the truncate, `path` is left as it was.
pub fn copy_truncate(path: &Path, max: u64) -> io::Result<bool> {
    if len_of(path)?.is_none_or(|len| len <= max) {
        return Ok(false);
    }
    // Read and write, never truncate on open: the truncate below is the
    // only change to the live file.
    let mut src = OpenOptions::new().read(true).write(true).open(path)?;
    let dest = rotated(path);
    let tmp = tmp_path(path);
    let copied = (|| {
        let mut out = crate::state::private_file()
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        io::copy(&mut src, &mut out)?;
        drop(out);
        fs::rename(&tmp, &dest)
    })();
    if let Err(e) = copied {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Catch up with what the service appended while the copy ran, then
    // truncate right away to keep the window small.
    let mut out = OpenOptions::new().append(true).open(&dest)?;
    io::copy(&mut src, &mut out)?;
    src.set_len(0)?;
    src.seek(SeekFrom::Start(0))?;
    Ok(true)
}

/// Renames `path` to `path.1` (replacing it) when it is at least `max`
/// bytes. For files no process keeps open (`events.jsonl`). Call under the
/// global lock.
pub fn rename_if_over(path: &Path, max: u64) -> io::Result<bool> {
    if len_of(path)?.is_none_or(|len| len < max) {
        return Ok(false);
    }
    fs::rename(path, rotated(path))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("taba-rotate-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn copy_truncate_keeps_an_open_append_handle_writing_at_the_start() {
        let d = dir("open-handle");
        let log = d.join("web.log");
        // The service's handle, opened like `taba run` opens it.
        let mut child = crate::state::private_file()
            .append(true)
            .open(&log)
            .unwrap();
        child.write_all(&[b'a'; 100]).unwrap();
        assert!(!copy_truncate(&log, 100).unwrap(), "not over the cap");
        child.write_all(b"\n").unwrap();
        assert!(copy_truncate(&log, 100).unwrap());
        let mut old = vec![b'a'; 100];
        old.push(b'\n');
        assert_eq!(fs::read(rotated(&log)).unwrap(), old);
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);
        child.write_all(b"after\n").unwrap();
        // No hole of zeros: the append lands at the new end of file.
        assert_eq!(fs::read(&log).unwrap(), b"after\n");
        assert!(!tmp_path(&log).exists());

        // A second rotation replaces the `.1` file.
        child.write_all(&[b'b'; 200]).unwrap();
        assert!(copy_truncate(&log, 100).unwrap());
        assert!(fs::read(rotated(&log)).unwrap().starts_with(b"after\nbbb"));
        drop(child);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_files_are_not_rotated() {
        let d = dir("missing");
        assert!(!copy_truncate(&d.join("none.log"), 0).unwrap());
        assert!(!rename_if_over(&d.join("events.jsonl"), 1).unwrap());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_rotates_at_the_cap() {
        let d = dir("rename");
        let ev = d.join("events.jsonl");
        fs::write(&ev, "x\n").unwrap();
        assert!(!rename_if_over(&ev, 3).unwrap());
        assert!(rename_if_over(&ev, 2).unwrap());
        assert!(!ev.exists());
        assert_eq!(fs::read_to_string(rotated(&ev)).unwrap(), "x\n");
        let _ = fs::remove_dir_all(&d);
    }
}
