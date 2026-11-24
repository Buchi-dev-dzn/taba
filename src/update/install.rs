//! Linux-only replacement of a provably official standalone install.
//!
//! The running executable is replaced only when its bytes are identical to the
//! `taba` member of the official archive for the running version (verified the
//! same way as the new archive). Anything else (source builds, package-manager
//! or Cargo installs, modified binaries, shared or foreign-owned files) is sent
//! to a manual upgrade. Replacement is a same-directory atomic `rename(2)`; the
//! original bytes stay in place until that rename succeeds.
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

pub(super) fn update(
    fetch: &impl Fetch,
    current: &Version,
    release: &Release,
    version: &Version,
    target: Target,
) -> Result<Output> {
    let exe = std::env::current_exe()
        .map_err(|e| failure(format!("could not locate the running taba executable: {e}")))?;
    update_at(fetch, &exe, current, release, version, target)
}

pub(super) fn update_at(
    fetch: &impl Fetch,
    exe: &Path,
    current: &Version,
    release: &Release,
    version: &Version,
    target: Target,
) -> Result<Output> {
    if let Err(reason) = eligible_path(exe) {
        return Ok(manual(version, reason));
    }
    // Provenance: the official archive for the *running* version must contain these exact bytes.
    let bytes = fetch.get(
        super::Download::Api,
        api_tag(&format!("v{current}")),
        METADATA_LIMIT,
    )?;
    let installed: Release =
        serde_json::from_slice(&bytes).map_err(|_| failure("invalid GitHub release metadata"))?;
    if installed.stable_version().as_ref() != Some(current) {
        return Ok(manual(
            version,
            "The running version has no matching stable official release.",
        ));
    }
    let official_current = archive::verified_binary(fetch, &installed, target)?;
    let new = archive::verified_binary(fetch, release, target)?;
    match replace(exe, &official_current, &new, || Ok(()))? {
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
pub(super) fn replace(
    path: &Path,
    expected: &[u8],
    new: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
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
    before_rename().map_err(|e| io_failure("update aborted before replacement", e))?;
    // rename(2) within one directory is atomic: readers see the old or the new file, never a mix.
    // On failure the temporary file is removed when `temp` is dropped.
    temp.persist(path)
        .map_err(|e| io_failure("could not replace the executable", e.error))?;
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(Replaced::Yes)
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
            replace(&exe, b"old official bytes", b"new", || Ok(())).unwrap(),
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
        let err = replace(&exe, b"old official bytes", b"new", || {
            Err(io::Error::other("injected"))
        })
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
        let err = replace(&exe, b"old official bytes", b"new", || {
            fs::rename(&exe, &moved)?;
            fs::create_dir(&exe)?;
            fs::write(exe.join("keep"), b"x")
        })
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
        let result = replace(&exe, b"old official bytes", b"new", || Ok(()));
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
            replace(&exe, b"other bytes", b"new", || Ok(())).unwrap(),
            Replaced::No(_)
        ));
        fs::hard_link(&exe, dir.path().join("link")).unwrap();
        assert!(matches!(
            replace(&exe, b"old official bytes", b"new", || Ok(())).unwrap(),
            Replaced::No(_)
        ));
        fs::remove_file(dir.path().join("link")).unwrap();
        fs::set_permissions(&exe, Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            replace(&exe, b"old official bytes", b"new", || Ok(())).unwrap(),
            Replaced::No(_)
        ));
        fs::set_permissions(&exe, Permissions::from_mode(0o4755)).unwrap();
        assert!(matches!(
            replace(&exe, b"old official bytes", b"new", || Ok(())).unwrap(),
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
        let err = replace(&exe, b"old official bytes", b"new", || Ok(())).unwrap_err();
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
            replace(&exe, b"old official bytes", b"new", || Ok(())).unwrap(),
            Replaced::Yes
        );
        assert_eq!(fs::read(&link).unwrap(), b"new");
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
