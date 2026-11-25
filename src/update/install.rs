//! Linux and macOS replacement of a provably official standalone install.
//!
//! The running executable is replaced only when its bytes are identical to the
//! `taba` member of the official archive for the running version (verified the
//! same way as the new archive). Anything else (source builds, package-manager
//! or Cargo installs, modified binaries, shared or foreign-owned files) is sent
//! to a manual upgrade. Replacement is a same-directory atomic `rename(2)`; the
//! original bytes stay in place until that rename succeeds.
//!
//! On macOS the new file is a fresh inode (never an in-place overwrite, which
//! would invalidate the kernel's cached code signature), loses any
//! `com.apple.quarantine` attribute before the swap, and must answer
//! `--version` with the new version after the swap. If that self-check fails
//! (including a code-signing `SIGKILL`), the original file is renamed back.
use super::{Fetch, Release, Target, api_tag, archive, failure, manual, read_bounded};
use crate::error::{Output, Result};
use semver::Version;
use serde_json::json;
use std::{
    fs::{self, File, Permissions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    os::unix::io::AsRawFd,
    path::Path,
};

const METADATA_LIMIT: usize = 1024 * 1024;

/// System locations owned by the distribution's package manager or other managers.
const MANAGED_PREFIXES: &[&str] = &[
    "/bin/",
    "/sbin/",
    "/usr/bin/",
    "/usr/sbin/",
    "/usr/lib/",
    "/usr/libexec/",
    "/usr/share/",
    "/nix/",
    "/snap/",
    "/opt/homebrew/",
    "/var/lib/flatpak/",
    "/home/linuxbrew/",
];

/// macOS-only system, application and package-manager locations (MacPorts, Homebrew's
/// Intel prefix internals). Checked after symlinks are resolved.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
const MACOS_MANAGED_PREFIXES: &[&str] = &[
    "/System/",
    "/Library/",
    "/Applications/",
    "/opt/local/",
    "/usr/local/Homebrew/",
];

/// Post-replacement self-check: runs the new file and returns an error to roll back.
pub(super) type SelfCheck<'a> = &'a dyn Fn(&Path) -> io::Result<()>;

/// Path fragments of Cargo, Homebrew, version managers and build trees.
/// A byte-identical binary there is still not ours to replace.
const MANAGED_PARTS: &[&str] = &[
    "/.linuxbrew/",
    "/Cellar/",
    "/.cargo/bin/",
    "/.local/share/mise/",
    "/.asdf/",
    "/.local/share/aquaproj-aqua/",
    "/target/debug/",
    "/target/release/",
];

const NO_CURRENT_RELEASE: &str = "The running version has no matching stable official release.";

pub(super) fn update(
    fetch: &impl Fetch,
    current: &Version,
    release: &Release,
    version: &Version,
    target: Target,
) -> Result<Output> {
    let exe = std::env::current_exe()
        .map_err(|e| failure(format!("could not locate the running taba executable: {e}")))?;
    #[cfg(target_os = "macos")]
    {
        // macOS reports the invoked path, which may be a symlink (e.g. a Homebrew link).
        // Resolve it so the real file is checked and replaced, never the link itself.
        let exe = std::fs::canonicalize(&exe).map_err(|e| {
            failure(format!(
                "could not resolve the running taba executable: {e}"
            ))
        })?;
        let check = |path: &Path| version_check(path, version);
        update_at(fetch, &exe, current, release, version, target, Some(&check))
    }
    #[cfg(not(target_os = "macos"))]
    update_at(fetch, &exe, current, release, version, target, None)
}

pub(super) fn update_at(
    fetch: &impl Fetch,
    exe: &Path,
    current: &Version,
    release: &Release,
    version: &Version,
    target: Target,
    self_check: Option<SelfCheck>,
) -> Result<Output> {
    if let Err(reason) = eligible_path(exe) {
        return Ok(manual(version, reason));
    }
    // Provenance: the official archive for the *running* version must contain these exact bytes.
    // A source build of an unreleased version has no release to compare
    // against (GitHub answers 404): that is a manual update, not a failure.
    let bytes = match fetch.get(
        super::Download::Api,
        api_tag(&format!("v{current}")),
        METADATA_LIMIT,
    ) {
        Err(e) if super::is_not_found(&e) => return Ok(manual(version, NO_CURRENT_RELEASE)),
        res => res?,
    };
    let installed: Release =
        serde_json::from_slice(&bytes).map_err(|_| failure("invalid GitHub release metadata"))?;
    if installed.stable_version().as_ref() != Some(current) {
        return Ok(manual(version, NO_CURRENT_RELEASE));
    }
    let official_current = archive::verified_binary(fetch, &installed, target)?;
    let new = archive::verified_binary(fetch, release, target)?;
    match replace(exe, &official_current, &new, || Ok(()), self_check)? {
        Replaced::No(reason) => Ok(manual(version, reason)),
        Replaced::Yes => Ok(Output::new(
            json!({"status": "updated", "previous": current.to_string(),
                   "installed": version.to_string(), "path": exe}),
            format!(
                "Updated taba v{current} -> v{version} at {}.\nRunning taba services are not restarted; new commands use v{version}.",
                exe.display()
            ),
        )),
    }
}

fn eligible_path(exe: &Path) -> std::result::Result<(), &'static str> {
    eligible_path_for(exe, cfg!(target_os = "macos"))
}

fn eligible_path_for(exe: &Path, macos: bool) -> std::result::Result<(), &'static str> {
    let text = exe
        .to_str()
        .ok_or("The executable path is not valid UTF-8.")?;
    if !exe.is_absolute() || text.ends_with(" (deleted)") {
        return Err("The running executable path could not be resolved.");
    }
    if exe.file_name().and_then(|n| n.to_str()) != Some("taba") {
        return Err("The executable is not named taba, so it is not a standalone install.");
    }
    if MANAGED_PREFIXES.iter().any(|dir| text.starts_with(dir))
        || MANAGED_PARTS.iter().any(|part| text.contains(part))
        || (macos
            && MACOS_MANAGED_PREFIXES
                .iter()
                .any(|dir| text.starts_with(dir)))
    {
        return Err(
            "This taba appears to be managed by a package manager, Cargo, or a build tree.",
        );
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Replaced {
    Yes,
    No(&'static str),
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn io_failure(what: &str, error: io::Error) -> crate::error::TabaError {
    failure(format!(
        "{what}: {error}; the existing taba executable was not changed"
    ))
}

/// Replace `path` with `new` if it currently holds exactly `expected`.
/// `before_rename` is a test seam for failures at the last moment before commit.
/// With `self_check`, the original file is kept as a hard link until the check
/// passes on the replaced path, and is renamed back if it fails.
pub(super) fn replace(
    path: &Path,
    expected: &[u8],
    new: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
    self_check: Option<SelfCheck>,
) -> Result<Replaced> {
    let dir = path
        .parent()
        .ok_or_else(|| failure("executable has no parent directory"))?;
    let uid = euid();
    if uid == 0 {
        return Ok(Replaced::No(
            "taba update does not run as root; upgrade a system-wide install manually.",
        ));
    }
    let current = File::open(path).map_err(|e| io_failure("could not open the executable", e))?;
    // An exclusive advisory lock on the old inode serializes concurrent `taba update` runs.
    // SAFETY: flock on an open descriptor owned by `current`.
    if unsafe { libc::flock(current.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(failure(
            "another taba update is in progress; the existing taba executable was not changed",
        ));
    }
    let meta = current
        .metadata()
        .map_err(|e| io_failure("could not inspect the executable", e))?;
    let dir_meta =
        fs::metadata(dir).map_err(|e| io_failure("could not inspect the install directory", e))?;
    if !meta.is_file() || meta.uid() != uid || dir_meta.uid() != uid {
        return Ok(Replaced::No(
            "The executable or its directory is not owned by the current user.",
        ));
    }
    if meta.nlink() != 1 {
        return Ok(Replaced::No(
            "The executable has other hard links, so replacing it would split the install.",
        ));
    }
    if meta.mode() & 0o7022 != 0 || dir_meta.mode() & 0o022 != 0 {
        return Ok(Replaced::No(
            "The executable or its directory has group/world-writable or special permission bits.",
        ));
    }
    let bytes = read_bounded(&current, archive::BINARY_LIMIT)?;
    if bytes != expected {
        return Ok(Replaced::No(
            "The running binary is not byte-identical to the official release archive (source build, package manager, or modified file).",
        ));
    }

    let mut temp = tempfile::Builder::new()
        .prefix(".taba-update-")
        .tempfile_in(dir)
        .map_err(|e| {
            io_failure(
                "could not create a temporary file next to the executable",
                e,
            )
        })?;
    temp.write_all(new)
        .and_then(|()| {
            temp.as_file()
                .set_permissions(Permissions::from_mode(meta.mode() & 0o755))
        })
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| io_failure("could not write the new executable", e))?;
    if fs::read(temp.path()).map_err(|e| io_failure("could not re-read the new executable", e))?
        != new
    {
        return Err(failure(
            "staged executable did not match the verified bytes; the existing taba executable was not changed",
        ));
    }
    let now = fs::symlink_metadata(path)
        .map_err(|e| io_failure("could not re-check the executable", e))?;
    if now.dev() != meta.dev() || now.ino() != meta.ino() || !now.file_type().is_file() {
        return Err(failure(
            "the executable changed during the update; nothing was replaced",
        ));
    }
    #[cfg(target_os = "macos")]
    clear_quarantine(temp.as_file())
        .map_err(|e| io_failure("could not clear quarantine on the new executable", e))?;
    // The original inode stays reachable under a second name until the self-check passes.
    let backup = match self_check {
        Some(_) => Some(Backup::link(path, dir)?),
        None => None,
    };
    before_rename().map_err(|e| io_failure("update aborted before replacement", e))?;
    // rename(2) within one directory is atomic: readers see the old or the new file, never a mix.
    // On failure the temporary file is removed when `temp` is dropped.
    temp.persist(path)
        .map_err(|e| io_failure("could not replace the executable", e.error))?;
    // On success the backup link is dropped (removed) here; the old inode lives on only
    // as long as running processes hold it.
    if let (Some(check), Some(backup)) = (self_check, backup)
        && let Err(error) = check(path)
    {
        return Err(backup.restore(path, &error));
    }
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(Replaced::Yes)
}

/// A hard link to the original executable, removed on drop unless it must be kept.
struct Backup {
    path: std::path::PathBuf,
    keep: bool,
}

impl Backup {
    fn link(original: &Path, dir: &Path) -> Result<Self> {
        // The directory is owned by us and not group/world-writable (checked above),
        // so a predictable name cannot be raced by another user.
        let path = dir.join(format!(".taba-update-previous-{}", std::process::id()));
        fs::hard_link(original, &path)
            .map_err(|e| io_failure("could not keep a rollback link to the executable", e))?;
        Ok(Self { path, keep: false })
    }

    /// Atomically put the original file back after a failed self-check.
    fn restore(mut self, target: &Path, cause: &io::Error) -> crate::error::TabaError {
        match fs::rename(&self.path, target) {
            Ok(()) => failure(format!(
                "the new executable failed its self-check ({cause}); the previous taba executable was restored"
            )),
            Err(error) => {
                self.keep = true;
                failure(format!(
                    "the new executable failed its self-check ({cause}) and could not be rolled back ({error}); the previous taba executable is at {}",
                    self.path.display()
                ))
            }
        }
    }
}

impl Drop for Backup {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Remove `com.apple.quarantine` from the staged, already verified executable only.
/// The original file is never modified.
#[cfg(target_os = "macos")]
fn clear_quarantine(file: &File) -> io::Result<()> {
    // SAFETY: valid descriptor and NUL-terminated attribute name.
    let rc = unsafe { libc::fremovexattr(file.as_raw_fd(), c"com.apple.quarantine".as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOATTR) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Run `path --version` and require exactly `taba <version>` within a time limit.
/// A code-signing rejection shows up here as a `SIGKILL`.
#[cfg(any(target_os = "macos", test))]
pub(super) fn version_check(path: &Path, version: &Version) -> io::Result<()> {
    use std::io::Read as _;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // Generous: the first run of an x86_64 binary under Rosetta is translated first.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("`--version` timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if let Some(signal) = status.signal() {
        return Err(io::Error::other(format!(
            "`--version` was killed by signal {signal}{}",
            if signal == libc::SIGKILL {
                ", possibly by code-signing enforcement"
            } else {
                ""
            }
        )));
    }
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        // `--version` output is tiny; it fits in the pipe buffer, so reading after exit is safe.
        let _ = (&mut out).take(4096).read_to_string(&mut stdout);
    }
    let expected = format!("taba {version}");
    if !status.success() || stdout.trim_end() != expected {
        return Err(io::Error::other(format!(
            "`--version` printed {:?} with {status}, expected {expected:?}",
            stdout.trim_end()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn staged_files(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".taba-update-"))
            })
            .collect()
    }

    fn setup(mode: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), Permissions::from_mode(0o755)).unwrap();
        let exe = dir.path().join("taba");
        fs::write(&exe, b"old official bytes").unwrap();
        fs::set_permissions(&exe, Permissions::from_mode(mode)).unwrap();
        (dir, exe)
    }

    fn skip_as_root() -> bool {
        euid() == 0
    }

    #[test]
    fn replaces_atomically_and_keeps_mode() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        assert_eq!(
            replace(&exe, b"old official bytes", b"new", || Ok(()), None).unwrap(),
            Replaced::Yes
        );
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert_eq!(fs::metadata(&exe).unwrap().mode() & 0o777, 0o755);
        assert!(staged_files(dir.path()).is_empty());
    }

    #[test]
    fn failure_before_rename_preserves_original() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        let err = replace(
            &exe,
            b"old official bytes",
            b"new",
            || Err(io::Error::other("injected")),
            None,
        )
        .unwrap_err();
        assert!(err.to_json().to_string().contains("not changed"));
        assert_eq!(fs::read(&exe).unwrap(), b"old official bytes");
        assert!(
            staged_files(dir.path()).is_empty(),
            "temporary file must be cleaned up"
        );
    }

    #[test]
    fn rename_failure_preserves_original() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        // Swap the target for a directory at the last moment: rename(file, non-empty dir) fails.
        let moved = dir.path().join("moved");
        let err = replace(
            &exe,
            b"old official bytes",
            b"new",
            || {
                fs::rename(&exe, &moved)?;
                fs::create_dir(&exe)?;
                fs::write(exe.join("keep"), b"x")
            },
            None,
        )
        .unwrap_err();
        assert!(err.to_json().to_string().contains("could not replace"));
        assert_eq!(fs::read(&moved).unwrap(), b"old official bytes");
        assert!(staged_files(dir.path()).is_empty());
    }

    #[test]
    fn unwritable_directory_preserves_original() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        fs::set_permissions(dir.path(), Permissions::from_mode(0o555)).unwrap();
        let result = replace(&exe, b"old official bytes", b"new", || Ok(()), None);
        fs::set_permissions(dir.path(), Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old official bytes");
    }

    #[test]
    fn refuses_unofficial_shared_or_writable_binaries() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        assert!(matches!(
            replace(&exe, b"other bytes", b"new", || Ok(()), None).unwrap(),
            Replaced::No(_)
        ));
        fs::hard_link(&exe, dir.path().join("link")).unwrap();
        assert!(matches!(
            replace(&exe, b"old official bytes", b"new", || Ok(()), None).unwrap(),
            Replaced::No(_)
        ));
        fs::remove_file(dir.path().join("link")).unwrap();
        fs::set_permissions(&exe, Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            replace(&exe, b"old official bytes", b"new", || Ok(()), None).unwrap(),
            Replaced::No(_)
        ));
        fs::set_permissions(&exe, Permissions::from_mode(0o4755)).unwrap();
        assert!(matches!(
            replace(&exe, b"old official bytes", b"new", || Ok(()), None).unwrap(),
            Replaced::No(_)
        ));
        assert_eq!(fs::read(&exe).unwrap(), b"old official bytes");
    }

    #[test]
    fn concurrent_update_is_refused() {
        if skip_as_root() {
            return;
        }
        let (_dir, exe) = setup(0o755);
        let holder = File::open(&exe).unwrap();
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) }, 0);
        let err = replace(&exe, b"old official bytes", b"new", || Ok(()), None).unwrap_err();
        assert!(err.to_json().to_string().contains("in progress"));
        assert_eq!(fs::read(&exe).unwrap(), b"old official bytes");
    }

    #[test]
    fn symlinked_target_is_not_swapped_for_link() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        let link = dir.path().join("link-taba");
        symlink(&exe, &link).unwrap();
        // current_exe() resolves symlinks; replacing the resolved file keeps the link valid.
        assert_eq!(
            replace(&exe, b"old official bytes", b"new", || Ok(()), None).unwrap(),
            Replaced::Yes
        );
        assert_eq!(fs::read(&link).unwrap(), b"new");
    }

    #[test]
    fn failed_self_check_restores_the_original_inode() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        let before = fs::metadata(&exe).unwrap();
        let check = |path: &Path| {
            assert_eq!(fs::read(path).unwrap(), b"new");
            Err(io::Error::other("killed by signal 9"))
        };
        let err = replace(&exe, b"old official bytes", b"new", || Ok(()), Some(&check))
            .unwrap_err()
            .to_json()
            .to_string();
        assert!(
            err.contains("was restored") && err.contains("signal 9"),
            "{err}"
        );
        assert_eq!(fs::read(&exe).unwrap(), b"old official bytes");
        let after = fs::metadata(&exe).unwrap();
        assert_eq!((after.ino(), after.mode()), (before.ino(), before.mode()));
        assert!(staged_files(dir.path()).is_empty());
    }

    #[test]
    fn passed_self_check_removes_the_rollback_link() {
        if skip_as_root() {
            return;
        }
        let (dir, exe) = setup(0o755);
        let ok = |_: &Path| Ok(());
        assert_eq!(
            replace(&exe, b"old official bytes", b"new", || Ok(()), Some(&ok)).unwrap(),
            Replaced::Yes
        );
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(staged_files(dir.path()).is_empty());
        // A failure before the rename also leaves no rollback link behind.
        let (dir, exe) = setup(0o755);
        assert!(
            replace(
                &exe,
                b"old official bytes",
                b"new",
                || Err(io::Error::other("injected")),
                Some(&ok)
            )
            .is_err()
        );
        assert_eq!(fs::read(&exe).unwrap(), b"old official bytes");
        assert!(staged_files(dir.path()).is_empty());
    }

    #[test]
    fn version_check_requires_the_new_version_and_a_clean_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
            path
        };
        let version = Version::parse("0.3.0").unwrap();
        assert!(version_check(&script("ok", "echo 'taba 0.3.0'"), &version).is_ok());
        for (name, body, needle) in [
            ("old", "echo 'taba 0.2.1'", "expected"),
            ("fails", "echo 'taba 0.3.0'; exit 1", "exit status: 1"),
            ("killed", "kill -9 $$", "signal 9"),
        ] {
            let error = version_check(&script(name, body), &version).unwrap_err();
            assert!(error.to_string().contains(needle), "{name}: {error}");
        }
        assert!(version_check(&dir.path().join("missing"), &version).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn quarantine_is_cleared_from_the_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staged");
        let file = File::create(&path).unwrap();
        let name = c"com.apple.quarantine";
        let value = b"0081;00000000;Safari;";
        // SAFETY: valid descriptor, name and buffer.
        let set = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(set, 0);
        clear_quarantine(&file).unwrap();
        // SAFETY: as above; a null buffer queries the size.
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        assert_eq!(size, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOATTR)
        );
        // Clearing an absent attribute is not an error.
        clear_quarantine(&file).unwrap();
    }

    #[test]
    fn macos_managed_paths_need_manual_upgrade() {
        for path in [
            "/opt/homebrew/bin/taba",
            "/usr/local/Cellar/taba/0.2.1/bin/taba",
            "/usr/local/Homebrew/bin/taba",
            "/opt/local/bin/taba",
            "/Applications/Tools/taba",
            "/Library/Developer/bin/taba",
            "/System/Volumes/Data/taba",
            "/Users/u/.cargo/bin/taba",
            "/Users/u/src/taba/target/release/taba",
        ] {
            assert!(eligible_path_for(Path::new(path), true).is_err(), "{path}");
        }
        for path in [
            "/Users/u/.local/bin/taba",
            "/Users/u/bin/taba",
            "/usr/local/bin/taba",
        ] {
            assert!(eligible_path_for(Path::new(path), true).is_ok(), "{path}");
        }
        // The macOS-only list does not change Linux eligibility.
        assert!(eligible_path_for(Path::new("/opt/local/bin/taba"), false).is_ok());
    }

    #[test]
    fn managed_paths_need_manual_upgrade() {
        for path in [
            "/usr/bin/taba",
            "/home/u/.cargo/bin/taba",
            "/nix/store/x/bin/taba",
            "/home/u/src/taba/target/release/taba",
            "/home/u/bin/taba-old",
            "/home/linuxbrew/.linuxbrew/bin/taba",
            "/opt/homebrew/bin/taba",
            "relative/taba",
        ] {
            assert!(eligible_path(Path::new(path)).is_err(), "{path}");
        }
        assert!(eligible_path(Path::new("/home/u/.local/bin/taba")).is_ok());
        assert!(eligible_path(Path::new("/usr/local/bin/taba")).is_ok());
    }
}
