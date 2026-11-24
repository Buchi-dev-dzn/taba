use super::{Fetch, Release, Target, download_asset, failure, read_bounded};
use crate::error::Result;
use flate2::bufread::GzDecoder;
use sha2::{Digest, Sha256};
use std::{io::Cursor, str};

const SUM_LIMIT: usize = 32 * 1024;
const ARCHIVE_LIMIT: usize = 32 * 1024 * 1024;
pub(super) const BINARY_LIMIT: usize = 64 * 1024 * 1024;
const DOC_LIMIT: usize = 1024 * 1024;
pub(super) const TAR_LIMIT: usize = BINARY_LIMIT + 2 * DOC_LIMIT + 64 * 1024;

pub(super) fn verified_binary(
    fetch: &impl Fetch,
    release: &Release,
    target: Target,
) -> Result<Vec<u8>> {
    let name = target.asset(&release.tag_name);
    let sums = download_asset(fetch, release, "SHA256SUMS", SUM_LIMIT)?;
    let expected = checksum_for(&sums, &name)?;
    let archive = download_asset(fetch, release, &name, ARCHIVE_LIMIT)?;
    if !format!("{:x}", Sha256::digest(&archive)).eq_ignore_ascii_case(expected) {
        return Err(failure(format!("SHA-256 mismatch for {name}")));
    }
    let executable = executable_from_tar_gz(&archive)?;
    validate_executable(&executable, target)?;
    Ok(executable)
}

fn checksum_for<'a>(sums: &'a [u8], name: &str) -> Result<&'a str> {
    let sums = str::from_utf8(sums).map_err(|_| failure("invalid SHA256SUMS encoding"))?;
    let mut expected = None;
    for line in sums.lines() {
        let (hash, asset) = line
            .split_once("  ")
            .ok_or_else(|| failure("malformed SHA256SUMS line"))?;
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || asset.is_empty()
            || !asset
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        {
            return Err(failure("malformed SHA256SUMS line"));
        }
        if asset == name && expected.replace(hash).is_some() {
            return Err(failure("duplicate archive entry in SHA256SUMS"));
        }
    }
    expected.ok_or_else(|| failure("archive is not listed in SHA256SUMS"))
}

fn unsafe_archive() -> crate::error::TabaError {
    failure("release archive does not match the official three-file layout")
}

fn executable_from_tar_gz(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut gzip = GzDecoder::new(bytes);
    let decoded = read_bounded(&mut gzip, TAR_LIMIT)?;
    if !gzip.into_inner().is_empty() {
        return Err(unsafe_archive());
    }
    let mut archive = tar::Archive::new(Cursor::new(decoded.as_slice()));
    let mut executable = None;
    let mut license = false;
    let mut readme = false;
    let mut pax = false;
    for entry in archive.entries().map_err(|_| unsafe_archive())?.raw(true) {
        let mut entry = entry.map_err(|_| unsafe_archive())?;
        let kind = entry.header().entry_type();
        if kind.is_pax_local_extensions() && !pax && (1..=1024).contains(&entry.size()) {
            // Python's official packager adds a local PAX record for fractional mtime.
            // Never apply PAX data (especially path/size overrides) when reading members.
            let mut extensions = entry
                .pax_extensions()
                .map_err(|_| unsafe_archive())?
                .ok_or_else(unsafe_archive)?;
            let field = extensions
                .next()
                .ok_or_else(unsafe_archive)?
                .map_err(|_| unsafe_archive())?;
            if field.key_bytes() != b"mtime"
                || !field
                    .value()
                    .is_ok_and(|s| s.parse::<f64>().is_ok_and(f64::is_finite))
                || extensions.next().is_some()
            {
                return Err(unsafe_archive());
            }
            pax = true;
            continue;
        }
        if !kind.is_file() || entry.size() == 0 {
            return Err(unsafe_archive());
        }
        let size = entry.size();
        match entry.path_bytes().as_ref() {
            b"taba" if executable.is_none() && size <= BINARY_LIMIT as u64 => {
                let binary = read_bounded(&mut entry, BINARY_LIMIT)?;
                if binary.len() as u64 != size {
                    return Err(unsafe_archive());
                }
                executable = Some(binary);
            }
            b"LICENSE" if !license && size <= DOC_LIMIT as u64 => license = true,
            b"README.md" if !readme && size <= DOC_LIMIT as u64 => readme = true,
            _ => return Err(unsafe_archive()),
        }
        pax = false;
    }
    let end = archive.into_inner().position() as usize;
    // tar consumes the first zero block; require another and reject hidden trailing members.
    if pax
        || !license
        || !readme
        || decoded.len() % 512 != 0
        || end < 512
        || !decoded
            .get(end - 512..)
            .is_some_and(|footer| footer.len() >= 1024 && footer.iter().all(|&b| b == 0))
    {
        return Err(unsafe_archive());
    }
    executable.ok_or_else(unsafe_archive)
}

fn validate_executable(bytes: &[u8], target: Target) -> Result<()> {
    match target.triple {
        "x86_64-unknown-linux-gnu" => validate_elf(bytes, 62),
        "aarch64-unknown-linux-gnu" => validate_elf(bytes, 183),
        // CPU_TYPE_X86_64 and CPU_TYPE_ARM64 (CPU_ARCH_ABI64 | base type).
        "x86_64-apple-darwin" => validate_mach_o(bytes, 0x0100_0007),
        "aarch64-apple-darwin" => validate_mach_o(bytes, 0x0100_000c),
        _ => Err(failure("not a supported self-update target")),
    }
}

/// A thin 64-bit little-endian Mach-O executable (`MH_EXECUTE`) for one CPU type.
/// Universal (fat) binaries are rejected: official archives contain one architecture.
fn validate_mach_o(bytes: &[u8], cpu_type: u32) -> Result<()> {
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    if bytes.len() < 32 || word(0) != 0xfeed_facf || word(4) != cpu_type || word(12) != 2 {
        return Err(failure(
            "archive executable does not match the expected macOS Mach-O target",
        ));
    }
    Ok(())
}

fn validate_elf(bytes: &[u8], expected_machine: u8) -> Result<()> {
    if bytes.len() < 64
        || !bytes.starts_with(b"\x7fELF\x02\x01\x01")
        || !matches!(bytes[16..18], [2 | 3, 0])
        || bytes[18..20] != [expected_machine, 0]
        || bytes[20..24] != [1, 0, 0, 0]
        || bytes[52..54] != [64, 0]
    {
        return Err(failure(
            "archive executable does not match the expected Linux ELF target",
        ));
    }
    Ok(())
}
