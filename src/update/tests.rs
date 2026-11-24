//! Offline tests: every release, checksum file and archive here is a local mock.
use super::archive::verified_binary;
use super::*;
use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::HashMap, io::Write};

const LINUX: &str = "x86_64-unknown-linux-gnu";

#[derive(Default)]
struct Mock {
    responses: HashMap<String, Vec<u8>>,
    requests: RefCell<Vec<String>>,
}

impl Mock {
    fn with(mut self, url: Url, body: impl Into<Vec<u8>>) -> Self {
        self.responses.insert(url.to_string(), body.into());
        self
    }
}

impl Fetch for Mock {
    fn get(&self, _kind: Download, url: Url, limit: usize) -> Result<Vec<u8>> {
        self.requests.borrow_mut().push(url.to_string());
        let body = self
            .responses
            .get(url.as_str())
            .ok_or_else(|| failure(format!("GitHub release request returned HTTP 404 ({url})")))?;
        read_bounded(body.as_slice(), limit)
    }
}

fn release_json(
    tag: &str,
    draft: bool,
    prerelease: bool,
    assets: &[(&str, usize)],
) -> serde_json::Value {
    json!({"tag_name": tag, "draft": draft, "prerelease": prerelease,
           "assets": assets.iter().map(|(name, size)| json!({"name": name, "state": "uploaded", "size": size}))
               .collect::<Vec<_>>()})
}

fn parse(value: serde_json::Value) -> Release {
    serde_json::from_value(value).unwrap()
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn listing(releases: &[serde_json::Value]) -> Mock {
    Mock::default().with(api_list(1), serde_json::to_vec(releases).unwrap())
}

// ---- SemVer selection -------------------------------------------------------

#[test]
fn picks_highest_strictly_newer_stable_release() {
    let releases = vec![
        parse(release_json("v0.9.0", false, false, &[])),
        parse(release_json("v0.10.0", false, false, &[])),
        parse(release_json("v0.11.0-rc.1", false, false, &[])), // SemVer prerelease
        parse(release_json("v0.12.0", false, true, &[])),       // GitHub prerelease flag
        parse(release_json("v0.13.0", true, false, &[])),       // draft
        parse(release_json("0.14.0", false, false, &[])),       // missing v prefix
        parse(release_json("v0.15", false, false, &[])),        // not SemVer
        parse(release_json("v0.2.0", false, false, &[])),       // current
    ];
    let (version, release) = newest(releases, &v("0.2.0")).unwrap().unwrap();
    assert_eq!(version, v("0.10.0"));
    assert_eq!(release.tag_name, "v0.10.0");
}

#[test]
fn no_update_when_nothing_is_strictly_newer() {
    let releases = vec![
        parse(release_json("v0.2.0", false, false, &[])),
        parse(release_json("v0.1.9", false, false, &[])),
        parse(release_json("v0.2.0+rebuild", false, false, &[])),
        parse(release_json("v0.3.0-beta", false, false, &[])),
    ];
    assert!(newest(releases, &v("0.2.0")).unwrap().is_none());
}

#[test]
fn equal_precedence_newest_releases_are_ambiguous() {
    let releases = vec![
        parse(release_json("v0.3.0", false, false, &[])),
        parse(release_json("v0.3.0+build", false, false, &[])),
    ];
    assert!(newest(releases, &v("0.2.0")).is_err());
}

#[test]
fn listing_paginates_and_is_bounded() {
    let full: Vec<_> = (0..PER_PAGE)
        .map(|i| release_json(&format!("v0.1.{i}"), false, false, &[]))
        .collect();
    let last = vec![release_json("v0.4.0", false, false, &[])];
    let mock = Mock::default()
        .with(api_list(1), serde_json::to_vec(&full).unwrap())
        .with(api_list(2), serde_json::to_vec(&last).unwrap());
    assert_eq!(discover(&mock, &v("0.2.0")).unwrap().unwrap().0, v("0.4.0"));

    let mut endless = Mock::default();
    for page in 1..=MAX_PAGES + 1 {
        endless = endless.with(api_list(page), serde_json::to_vec(&full).unwrap());
    }
    assert!(discover(&endless, &v("0.2.0")).is_err());
    assert_eq!(endless.requests.borrow().len(), MAX_PAGES);
}

#[test]
fn invalid_or_oversized_metadata_fails() {
    assert!(discover(&Mock::default().with(api_list(1), "not json"), &v("0.2.0")).is_err());
    let huge = vec![b' '; METADATA_LIMIT + 1];
    assert!(discover(&Mock::default().with(api_list(1), huge), &v("0.2.0")).is_err());
}

// ---- check-only and target selection -----------------------------------------

fn never_install(_: &Release, _: &Version, _: Target) -> Result<Output> {
    panic!("check-only must not install")
}

#[test]
fn check_reports_update_and_only_reads_metadata() {
    for (triple, asset) in [
        (LINUX, "taba-v0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
        (
            "aarch64-unknown-linux-gnu",
            "taba-v0.3.0-aarch64-unknown-linux-gnu.tar.gz",
        ),
        (
            "aarch64-apple-darwin",
            "taba-v0.3.0-aarch64-apple-darwin.tar.gz",
        ),
        (
            "x86_64-apple-darwin",
            "taba-v0.3.0-x86_64-apple-darwin.tar.gz",
        ),
        (
            "x86_64-pc-windows-msvc",
            "taba-v0.3.0-x86_64-pc-windows-msvc.zip",
        ),
    ] {
        let mock = listing(&[release_json(
            "v0.3.0",
            false,
            false,
            &[(asset, 10), ("SHA256SUMS", 10)],
        )]);
        let out = run(&mock, true, &v("0.2.0"), triple, never_install).unwrap();
        assert_eq!(out.body["status"], "update_available");
        assert_eq!(out.body["available"], "0.3.0");
        assert_eq!(out.body["asset"], asset);
        assert_eq!(*mock.requests.borrow(), vec![api_list(1).to_string()]);
    }
}

#[test]
fn check_and_update_report_up_to_date() {
    let mock = listing(&[release_json("v0.2.0", false, false, &[])]);
    for check in [true, false] {
        let out = run(&mock, check, &v("0.2.0"), LINUX, never_install).unwrap();
        assert_eq!(out.body["status"], "up_to_date");
    }
}

#[test]
fn unknown_or_missing_target_is_manual() {
    let mock = listing(&[release_json(
        "v0.3.0",
        false,
        false,
        &[("taba-v0.3.0-x86_64-unknown-linux-gnu.tar.gz", 10)],
    )]);
    let out = run(
        &mock,
        true,
        &v("0.2.0"),
        "x86_64-unknown-linux-musl",
        never_install,
    )
    .unwrap();
    assert_eq!(out.body["asset"], serde_json::Value::Null);
    for triple in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-gnu"] {
        let out = run(&mock, false, &v("0.2.0"), triple, never_install).unwrap();
        assert_eq!(out.body["status"], "manual_update", "{triple}");
    }
}

#[test]
fn update_hands_matching_target_to_installer() {
    let mock = listing(&[release_json(
        "v0.3.0",
        false,
        false,
        &[("taba-v0.3.0-x86_64-unknown-linux-gnu.tar.gz", 10)],
    )]);
    let out = run(
        &mock,
        false,
        &v("0.2.0"),
        LINUX,
        |release, version, target| {
            assert_eq!(
                (
                    release.tag_name.as_str(),
                    version.to_string(),
                    target.triple
                ),
                ("v0.3.0", "0.3.0".into(), LINUX)
            );
            Ok(Output::new(
                json!({"status": "installed-by-test"}),
                String::new(),
            ))
        },
    )
    .unwrap();
    assert_eq!(out.body["status"], "installed-by-test");
}

// ---- origins ---------------------------------------------------------------

#[test]
fn urls_are_fixed_to_the_official_repository() {
    assert_eq!(
        api_list(2).as_str(),
        "https://api.github.com/repos/Buchi-dev-dzn/taba/releases?per_page=100&page=2"
    );
    assert_eq!(
        api_tag("v0.2.0").as_str(),
        "https://api.github.com/repos/Buchi-dev-dzn/taba/releases/tags/v0.2.0"
    );
    assert_eq!(
        api_tag("../x").as_str(),
        "https://api.github.com/repos/Buchi-dev-dzn/taba/releases/tags/..%2Fx"
    );
    assert_eq!(
        asset_url("v0.3.0", "SHA256SUMS").as_str(),
        "https://github.com/Buchi-dev-dzn/taba/releases/download/v0.3.0/SHA256SUMS"
    );
}

#[test]
fn only_github_release_asset_redirects_are_followed() {
    let ok = |s: &str| asset_redirect(&Url::parse(s).unwrap());
    assert!(ok(
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=x"
    ));
    assert!(ok(
        "https://objects.githubusercontent.com/github-production-release-asset-2e65be/1/2"
    ));
    assert!(!ok(
        "http://release-assets.githubusercontent.com/github-production-release-asset/1"
    ));
    assert!(!ok(
        "https://evil.example/github-production-release-asset/1"
    ));
    assert!(!ok(
        "https://release-assets.githubusercontent.com.evil.example/github-production-release-asset/1"
    ));
    assert!(!ok(
        "https://release-assets.githubusercontent.com:8443/github-production-release-asset/1"
    ));
    assert!(!ok(
        "https://user@release-assets.githubusercontent.com/github-production-release-asset/1"
    ));
    assert!(!ok("https://release-assets.githubusercontent.com/other/1"));
}

// ---- archives and integrity metadata -----------------------------------------

fn elf(machine: u8) -> Vec<u8> {
    let mut bytes = vec![0u8; 256];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16] = 3; // ET_DYN (PIE)
    bytes[18] = machine;
    bytes[20] = 1;
    bytes[52] = 64;
    bytes
}

enum Member<'a> {
    File(&'a str, &'a [u8]),
    Symlink(&'a str, &'a str),
    Pax(&'a str),
}

fn tar(members: &[Member]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for member in members {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o755);
        match member {
            Member::File(name, data) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(data.len() as u64);
                // Raw path bytes so tests can express names the builder would reject.
                header.as_ustar_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
                header.set_cksum();
                builder.append(&header, *data).unwrap();
            }
            Member::Symlink(name, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_path(name).unwrap();
                header.set_link_name(target).unwrap();
                header.set_cksum();
                builder.append(&header, &[][..]).unwrap();
            }
            Member::Pax(record) => {
                let body = format!(
                    "{} {record}\n",
                    record.len() + 2 + (record.len() + 2).to_string().len()
                );
                header.set_entry_type(tar::EntryType::XHeader);
                header.set_path("././@PaxHeader").unwrap();
                header.set_size(body.len() as u64);
                header.set_cksum();
                builder.append(&header, body.as_bytes()).unwrap();
            }
        }
    }
    builder.into_inner().unwrap() // writes the two zero end blocks
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// The member layout `scripts/package-release.py` produces with Python's tarfile.
fn official(binary: &[u8]) -> Vec<u8> {
    gzip(&tar(&[
        Member::Pax("mtime=1700000000.123456"),
        Member::File("taba", binary),
        Member::Pax("mtime=1700000000.5"),
        Member::File("LICENSE", b"MIT"),
        Member::File("README.md", b"# taba"),
    ]))
}

const ASSET: &str = "taba-v0.3.0-x86_64-unknown-linux-gnu.tar.gz";

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn published(archive: &[u8], sums: &str) -> (Mock, Release) {
    let release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[(ASSET, archive.len()), ("SHA256SUMS", sums.len())],
    ));
    let mock = Mock::default()
        .with(asset_url("v0.3.0", ASSET), archive.to_vec())
        .with(asset_url("v0.3.0", "SHA256SUMS"), sums.as_bytes().to_vec());
    (mock, release)
}

fn linux() -> Target {
    target(LINUX).unwrap()
}

fn verify(archive: &[u8]) -> Result<Vec<u8>> {
    let sums = format!(
        "{}  {ASSET}\n{}  taba-v0.3.0-x86_64-pc-windows-msvc.zip\n",
        sha(archive),
        "0".repeat(64)
    );
    let (mock, release) = published(archive, &sums);
    verified_binary(&mock, &release, linux())
}

#[test]
fn official_layout_verifies() {
    let binary = elf(62);
    assert_eq!(verify(&official(&binary)).unwrap(), binary);
    let arm = target("aarch64-unknown-linux-gnu").unwrap();
    let archive = official(&elf(183));
    let name = arm.asset("v0.3.0");
    let sums = format!("{}  {name}\n", sha(&archive));
    let release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[(&name, archive.len()), ("SHA256SUMS", sums.len())],
    ));
    let mock = Mock::default()
        .with(asset_url("v0.3.0", &name), archive)
        .with(asset_url("v0.3.0", "SHA256SUMS"), sums);
    assert_eq!(verified_binary(&mock, &release, arm).unwrap(), elf(183));
}

#[test]
fn missing_or_invalid_integrity_metadata_fails_before_archive_use() {
    let archive = official(&elf(62));
    let good = sha(&archive);
    for sums in [
        String::new(),                                               // empty
        format!("{good}  taba-v0.3.0-x86_64-pc-windows-msvc.zip\n"), // not listed
        format!("{good} {ASSET}\n"),                                 // single space
        format!("{}  {ASSET}\n", &good[..63]),                       // short hash
        format!("{}  {ASSET}\n", "g".repeat(64)),                    // non-hex
        format!("{good}  {ASSET}\n{good}  {ASSET}\n"),               // duplicate
        format!("{good}  ../{ASSET}\n"),                             // path in name
        format!("{}  {ASSET}\n", sha(b"other")),                     // mismatch
    ] {
        assert!(verify_with_sums(&archive, &sums).is_err(), "{sums:?}");
    }
    // SHA256SUMS absent from the release.
    let release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[(ASSET, archive.len())],
    ));
    let mock = Mock::default().with(asset_url("v0.3.0", ASSET), archive.clone());
    assert!(verified_binary(&mock, &release, linux()).is_err());
    assert!(
        mock.requests.borrow().is_empty(),
        "nothing downloaded without SHA256SUMS"
    );
}

fn verify_with_sums(archive: &[u8], sums: &str) -> Result<Vec<u8>> {
    let (mock, release) = published(archive, sums);
    verified_binary(&mock, &release, linux())
}

#[test]
fn asset_metadata_must_match_download() {
    let archive = official(&elf(62));
    let sums = format!("{}  {ASSET}\n", sha(&archive));
    // Declared size smaller than served bytes.
    let mut release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[(ASSET, archive.len() - 1), ("SHA256SUMS", sums.len())],
    ));
    let mock = Mock::default()
        .with(asset_url("v0.3.0", ASSET), archive.clone())
        .with(asset_url("v0.3.0", "SHA256SUMS"), sums.clone());
    assert!(verified_binary(&mock, &release, linux()).is_err());
    // Not fully uploaded.
    release = parse(
        json!({"tag_name": "v0.3.0", "draft": false, "prerelease": false, "assets": [
        {"name": ASSET, "state": "starter", "size": archive.len()},
        {"name": "SHA256SUMS", "state": "uploaded", "size": sums.len()}]}),
    );
    assert!(verified_binary(&mock, &release, linux()).is_err());
    // Duplicate asset names.
    release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[
            (ASSET, archive.len()),
            (ASSET, archive.len()),
            ("SHA256SUMS", sums.len()),
        ],
    ));
    assert!(verified_binary(&mock, &release, linux()).is_err());
    // Oversized archive is refused from metadata alone.
    release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[(ASSET, 1 << 30), ("SHA256SUMS", sums.len())],
    ));
    assert!(verified_binary(&mock, &release, linux()).is_err());
}

#[test]
fn unsafe_archives_are_rejected_after_checksum() {
    let bin = elf(62);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "extra member",
            gzip(&tar(&[
                Member::File("taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
                Member::File("evil", b"x"),
            ])),
        ),
        (
            "traversal",
            gzip(&tar(&[
                Member::File("../taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "absolute",
            gzip(&tar(&[
                Member::File("/tmp/taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "nested",
            gzip(&tar(&[
                Member::File("bin/taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "symlink",
            gzip(&tar(&[
                Member::Symlink("taba", "/bin/sh"),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "duplicate taba",
            gzip(&tar(&[
                Member::File("taba", &bin),
                Member::File("taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "missing LICENSE",
            gzip(&tar(&[
                Member::File("taba", &bin),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "missing binary",
            gzip(&tar(&[
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "pax path override",
            gzip(&tar(&[
                Member::Pax("path=../../taba"),
                Member::File("taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "pax size override",
            gzip(&tar(&[
                Member::Pax("size=1"),
                Member::File("taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ])),
        ),
        (
            "dangling pax",
            gzip(&tar(&[
                Member::File("taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
                Member::Pax("mtime=1"),
            ])),
        ),
        ("wrong machine", official(&elf(183))),
        ("not ELF", official(b"#!/bin/sh\necho pwned\n")),
        (
            "not gzip",
            tar(&[
                Member::File("taba", &bin),
                Member::File("LICENSE", b"x"),
                Member::File("README.md", b"x"),
            ]),
        ),
        ("trailing gzip data", {
            let mut a = official(&bin);
            a.extend_from_slice(b"junk");
            a
        }),
        (
            "hidden member after end",
            gzip(&{
                let mut t = tar(&[
                    Member::File("taba", &bin),
                    Member::File("LICENSE", b"x"),
                    Member::File("README.md", b"x"),
                ]);
                t.truncate(t.len() - 512); // keep one zero block, then append another member
                t.extend(tar(&[Member::File("evil", b"x")]));
                t
            }),
        ),
        (
            "decompression bomb",
            gzip(&vec![0u8; archive::TAR_LIMIT + 4096]),
        ),
    ];
    for (name, archive) in cases {
        let error = verify(&archive).expect_err(name);
        assert!(
            !error.to_json().to_string().contains("SHA-256 mismatch"),
            "{name} must pass the checksum first"
        );
    }
}

/// Run with `TABA_UPDATE_PY_FIXTURE=<archive> cargo test -- --ignored python_tarfile`
/// against an archive written by Python's `tarfile` exactly as `scripts/package-release.py` does.
#[test]
#[ignore = "needs an archive produced by Python's tarfile"]
fn python_tarfile_archive_verifies() {
    let path = std::env::var("TABA_UPDATE_PY_FIXTURE").expect("TABA_UPDATE_PY_FIXTURE");
    let archive = std::fs::read(path).unwrap();
    assert_eq!(verify(&archive).unwrap(), elf(62));
}

// ---- macOS targets and Mach-O executables -------------------------------------

const ARM64: u32 = 0x0100_000c;
const X86_64: u32 = 0x0100_0007;
const DARWIN: [(&str, u32); 2] = [
    ("aarch64-apple-darwin", ARM64),
    ("x86_64-apple-darwin", X86_64),
];

/// A minimal thin 64-bit Mach-O `MH_EXECUTE` header for `cpu`.
fn mach_o(cpu: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 256];
    bytes[..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
    bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
    bytes
}

/// Publish `archive` as `target`'s v0.3.0 asset with a matching `SHA256SUMS`.
fn verify_for(triple: &str, archive: &[u8]) -> Result<Vec<u8>> {
    let target = target(triple).unwrap();
    let name = target.asset("v0.3.0");
    let sums = format!("{}  {name}\n", sha(archive));
    let release = parse(release_json(
        "v0.3.0",
        false,
        false,
        &[(&name, archive.len()), ("SHA256SUMS", sums.len())],
    ));
    let mock = Mock::default()
        .with(asset_url("v0.3.0", &name), archive.to_vec())
        .with(asset_url("v0.3.0", "SHA256SUMS"), sums);
    verified_binary(&mock, &release, target)
}

#[test]
fn macos_targets_select_their_official_archives() {
    for (triple, _) in DARWIN {
        assert_eq!(
            target(triple).unwrap().asset("v0.3.0"),
            format!("taba-v0.3.0-{triple}.tar.gz")
        );
    }
    let mock = listing(&[release_json(
        "v0.3.0",
        false,
        false,
        &[
            ("taba-v0.3.0-aarch64-apple-darwin.tar.gz", 10),
            ("taba-v0.3.0-x86_64-apple-darwin.tar.gz", 10),
            ("taba-v0.3.0-x86_64-unknown-linux-gnu.tar.gz", 10),
        ],
    )]);
    for (triple, _) in DARWIN {
        let out = run(&mock, true, &v("0.2.0"), triple, never_install).unwrap();
        assert_eq!(
            out.body["asset"],
            format!("taba-v0.3.0-{triple}.tar.gz"),
            "{triple}"
        );
        let out = run(&mock, false, &v("0.2.0"), triple, |_, _, target| {
            assert_eq!(target.triple, triple);
            Ok(Output::new(
                json!({"status": "installed-by-test"}),
                String::new(),
            ))
        })
        .unwrap();
        assert_eq!(out.body["status"], "installed-by-test", "{triple}");
    }
}

#[test]
fn macos_archives_require_a_matching_thin_mach_o() {
    for (triple, cpu) in DARWIN {
        assert_eq!(
            verify_for(triple, &official(&mach_o(cpu))).unwrap(),
            mach_o(cpu)
        );
        let other = if cpu == ARM64 { X86_64 } else { ARM64 };
        let mut fat = mach_o(cpu);
        fat[..4].copy_from_slice(&0xcafe_babe_u32.to_be_bytes());
        let mut dylib = mach_o(cpu);
        dylib[12] = 6; // MH_DYLIB
        for (case, binary) in [
            ("other architecture", mach_o(other)),
            ("Linux ELF", elf(62)),
            ("universal binary", fat),
            ("not an executable", dylib),
            ("truncated header", mach_o(cpu)[..16].to_vec()),
        ] {
            assert!(
                verify_for(triple, &official(&binary)).is_err(),
                "{triple}: {case}"
            );
        }
    }
    // A Mach-O is never accepted for a Linux target.
    assert!(verify_for(LINUX, &official(&mach_o(X86_64))).is_err());
}

// ---- end-to-end Linux replacement in a disposable directory -------------------

#[cfg(target_os = "linux")]
mod linux_install {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn release_with(tag: &str, binary: &[u8]) -> (serde_json::Value, Vec<(Url, Vec<u8>)>) {
        let name = linux().asset(tag);
        let archive = official(binary);
        let sums = format!("{}  {name}\n", sha(&archive));
        let meta = release_json(
            tag,
            false,
            false,
            &[(&name, archive.len()), ("SHA256SUMS", sums.len())],
        );
        (
            meta,
            vec![
                (asset_url(tag, &name), archive),
                (asset_url(tag, "SHA256SUMS"), sums.into_bytes()),
            ],
        )
    }

    fn fixture(old: &[u8], new: &[u8]) -> (Mock, Release) {
        let (old_meta, old_assets) = release_with("v0.2.1", old);
        let (new_meta, new_assets) = release_with("v0.3.0", new);
        let mut mock =
            Mock::default().with(api_tag("v0.2.1"), serde_json::to_vec(&old_meta).unwrap());
        for (url, body) in old_assets.into_iter().chain(new_assets) {
            mock = mock.with(url, body);
        }
        (mock, parse(new_meta))
    }

    fn install_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let exe = dir.path().join("taba");
        (dir, exe)
    }

    fn run_update(mock: &Mock, exe: &std::path::Path, release: &Release) -> Result<Output> {
        super::super::install::update_at(
            mock,
            exe,
            &v("0.2.1"),
            release,
            &v("0.3.0"),
            linux(),
            None,
        )
    }

    fn is_root() -> bool {
        // SAFETY: no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn official_standalone_install_is_replaced() {
        if is_root() {
            return;
        }
        let (old, mut new) = (elf(62), elf(62));
        new[100] = 7;
        let (mock, release) = fixture(&old, &new);
        let (_dir, exe) = install_dir();
        fs::write(&exe, &old).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let out = run_update(&mock, &exe, &release).unwrap();
        assert_eq!(out.body["status"], "updated");
        assert_eq!(fs::read(&exe).unwrap(), new);
    }

    #[test]
    fn non_official_binary_is_left_alone() {
        if is_root() {
            return;
        }
        let (mock, release) = fixture(&elf(62), &elf(62));
        let (_dir, exe) = install_dir();
        let mut source_build = elf(62);
        source_build[200] = 1;
        fs::write(&exe, &source_build).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let out = run_update(&mock, &exe, &release).unwrap();
        assert_eq!(out.body["status"], "manual_update");
        assert_eq!(fs::read(&exe).unwrap(), source_build);
    }

    #[test]
    fn bad_new_release_preserves_original_bytes() {
        if is_root() {
            return;
        }
        let old = elf(62);
        let (mut mock, release) = fixture(&old, &elf(62));
        let name = linux().asset("v0.3.0");
        let mut tampered = mock.responses[asset_url("v0.3.0", &name).as_str()].clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        mock = mock.with(asset_url("v0.3.0", &name), tampered);
        let (_dir, exe) = install_dir();
        fs::write(&exe, &old).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(run_update(&mock, &exe, &release).is_err());
        assert_eq!(fs::read(&exe).unwrap(), old);
    }

    #[test]
    fn missing_current_release_needs_manual_path() {
        if is_root() {
            return;
        }
        let (_, release) = fixture(&elf(62), &elf(62));
        let (_dir, exe) = install_dir();
        fs::write(&exe, elf(62)).unwrap();
        // No metadata for the running version: the request fails, nothing is touched.
        assert!(run_update(&Mock::default(), &exe, &release).is_err());
        assert_eq!(fs::read(&exe).unwrap(), elf(62));
    }
}

// ---- end-to-end macOS replacement in a disposable directory -------------------

#[cfg(target_os = "macos")]
mod macos_install {
    use super::*;
    use std::{
        fs, io,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::Path,
    };

    fn release_with(
        triple: &str,
        tag: &str,
        binary: &[u8],
    ) -> (serde_json::Value, Vec<(Url, Vec<u8>)>) {
        let name = target(triple).unwrap().asset(tag);
        let archive = official(binary);
        let sums = format!("{}  {name}\n", sha(&archive));
        let meta = release_json(
            tag,
            false,
            false,
            &[(&name, archive.len()), ("SHA256SUMS", sums.len())],
        );
        (
            meta,
            vec![
                (asset_url(tag, &name), archive),
                (asset_url(tag, "SHA256SUMS"), sums.into_bytes()),
            ],
        )
    }

    fn fixture(triple: &str, old: &[u8], new: &[u8]) -> (Mock, Release) {
        let (old_meta, old_assets) = release_with(triple, "v0.2.1", old);
        let (new_meta, new_assets) = release_with(triple, "v0.3.0", new);
        let mut mock =
            Mock::default().with(api_tag("v0.2.1"), serde_json::to_vec(&old_meta).unwrap());
        for (url, body) in old_assets.into_iter().chain(new_assets) {
            mock = mock.with(url, body);
        }
        (mock, parse(new_meta))
    }

    /// An installed official binary in a private, canonical directory.
    fn installed(old: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let exe = fs::canonicalize(dir.path()).unwrap().join("taba");
        fs::write(&exe, old).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, exe)
    }

    fn run_update(
        mock: &Mock,
        triple: &str,
        exe: &Path,
        release: &Release,
        check: &dyn Fn(&Path) -> io::Result<()>,
    ) -> Result<Output> {
        super::super::install::update_at(
            mock,
            exe,
            &v("0.2.1"),
            release,
            &v("0.3.0"),
            target(triple).unwrap(),
            Some(check),
        )
    }

    fn leftovers(exe: &Path) -> Vec<String> {
        fs::read_dir(exe.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".taba-update-"))
            .collect()
    }

    fn is_root() -> bool {
        // SAFETY: no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    fn pair(cpu: u32) -> (Vec<u8>, Vec<u8>) {
        let (old, mut new) = (mach_o(cpu), mach_o(cpu));
        new[100] = 7;
        (old, new)
    }

    #[test]
    fn official_standalone_install_is_replaced_and_checked() {
        if is_root() {
            return;
        }
        for (triple, cpu) in DARWIN {
            let (old, new) = pair(cpu);
            let (mock, release) = fixture(triple, &old, &new);
            let (_dir, exe) = installed(&old);
            let checked = std::cell::Cell::new(false);
            let check = |path: &Path| {
                // The self-check sees the new bytes at the real path.
                assert_eq!(path, exe.as_path());
                assert_eq!(fs::read(path).unwrap(), new);
                checked.set(true);
                Ok(())
            };
            let out = run_update(&mock, triple, &exe, &release, &check).unwrap();
            assert_eq!(out.body["status"], "updated", "{triple}");
            assert!(checked.get(), "{triple}: self-check must run");
            assert_eq!(fs::read(&exe).unwrap(), new);
            assert_eq!(fs::metadata(&exe).unwrap().mode() & 0o777, 0o755);
            assert!(
                leftovers(&exe).is_empty(),
                "{triple}: {:?}",
                leftovers(&exe)
            );
        }
    }

    #[test]
    fn replacement_failure_keeps_original_bytes() {
        if is_root() {
            return;
        }
        for (triple, cpu) in DARWIN {
            let (old, new) = pair(cpu);
            let (mock, release) = fixture(triple, &old, &new);
            let (dir, exe) = installed(&old);
            // A read-only directory: the staged file cannot be created, nothing is replaced.
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
            let result = run_update(&mock, triple, &exe, &release, &|_| Ok(()));
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(result.is_err(), "{triple}");
            assert_eq!(fs::read(&exe).unwrap(), old);
            assert!(leftovers(&exe).is_empty());

            // A tampered new archive is rejected before anything is staged.
            let (mut mock, release) = fixture(triple, &old, &new);
            let name = target(triple).unwrap().asset("v0.3.0");
            let mut tampered = mock.responses[asset_url("v0.3.0", &name).as_str()].clone();
            let last = tampered.len() - 1;
            tampered[last] ^= 1;
            mock = mock.with(asset_url("v0.3.0", &name), tampered);
            assert!(run_update(&mock, triple, &exe, &release, &|_| Ok(())).is_err());
            assert_eq!(fs::read(&exe).unwrap(), old);
            assert!(leftovers(&exe).is_empty());
        }
    }

    #[test]
    fn failed_self_check_rolls_back_to_the_original_file() {
        if is_root() {
            return;
        }
        for (triple, cpu) in DARWIN {
            let (old, new) = pair(cpu);
            let (mock, release) = fixture(triple, &old, &new);
            let (_dir, exe) = installed(&old);
            let before = fs::metadata(&exe).unwrap();
            let check = |_: &Path| Err(io::Error::other("killed by signal 9"));
            let error = run_update(&mock, triple, &exe, &release, &check)
                .err()
                .unwrap();
            let text = error.to_json().to_string();
            assert!(text.contains("was restored"), "{text}");
            assert_eq!(fs::read(&exe).unwrap(), old);
            let after = fs::metadata(&exe).unwrap();
            assert_eq!(
                (after.ino(), after.mode()),
                (before.ino(), before.mode()),
                "{triple}: the original inode is back"
            );
            assert!(
                leftovers(&exe).is_empty(),
                "{triple}: {:?}",
                leftovers(&exe)
            );
        }
    }

    #[test]
    fn package_manager_and_unofficial_installs_need_manual_update() {
        if is_root() {
            return;
        }
        let (triple, cpu) = DARWIN[0];
        let (old, new) = pair(cpu);
        let (mock, release) = fixture(triple, &old, &new);
        let (_dir, exe) = installed(&new); // not the official v0.2.1 bytes
        let out = run_update(&mock, triple, &exe, &release, &|_| unreachable!()).unwrap();
        assert_eq!(out.body["status"], "manual_update");
        assert_eq!(fs::read(&exe).unwrap(), new);
        for path in [
            "/opt/homebrew/bin/taba",
            "/usr/local/Cellar/taba/0.2.1/bin/taba",
            "/Users/u/.cargo/bin/taba",
        ] {
            let out = run_update(
                &mock,
                triple,
                Path::new(path),
                &release,
                &|_| unreachable!(),
            )
            .unwrap();
            assert_eq!(out.body["status"], "manual_update", "{path}");
        }
    }
}

/// End to end on this Mac with real binaries. Run sequentially per architecture:
///
/// `TABA_UPDATE_E2E_DIR=<dir> cargo test [--target x86_64-apple-darwin] -- --ignored --test-threads=1 macos_end_to_end`
///
/// `<dir>` holds `v0.2.0.json` (GitHub's release metadata for v0.2.0),
/// `download/v0.2.0/{SHA256SUMS,taba-v0.2.0-<triple>.tar.gz}` (the official assets), and
/// `candidates/<case>/{v0.2.1.json,v0.2.1/<assets>}` for each local v0.2.1 candidate
/// (`good`, `wrong-version`, `bad-signature`). The fetcher maps the fixed official URLs
/// to these files, acting as the release server; nothing else is read.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs TABA_UPDATE_E2E_DIR with official v0.2.0 and local v0.2.1 archives"]
fn macos_end_to_end() {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

    struct Files(std::path::PathBuf);
    impl Fetch for Files {
        fn get(&self, _kind: Download, url: Url, limit: usize) -> Result<Vec<u8>> {
            let path = match url.as_str() {
                u if u == api_tag("v0.2.0").as_str() => self.0.join("v0.2.0.json"),
                u => {
                    let rest = u
                        .strip_prefix(&format!("{RELEASES}/download/"))
                        .unwrap_or_else(|| panic!("unexpected URL {u}"));
                    self.0.join("download").join(rest)
                }
            };
            read_bounded(fs::File::open(&path).unwrap(), limit)
        }
    }

    let root = std::path::PathBuf::from(std::env::var("TABA_UPDATE_E2E_DIR").unwrap());
    let triple = env!("TABA_BUILD_TARGET");
    let target = target(triple).unwrap();
    let official = root.join("download/v0.2.0").join(target.asset("v0.2.0"));
    let current = v("0.2.0");
    let version = |exe: &Path| {
        let out = Command::new(exe).arg("--version").output().unwrap();
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )
    };

    for (case, expect_updated) in [
        ("good", true),
        ("wrong-version", false),
        ("bad-signature", false),
    ] {
        let candidate = root.join("candidates").join(case);
        if !candidate.exists() {
            eprintln!("{triple} {case}: skipped (no candidate)");
            continue;
        }
        // A fresh standalone install: the official v0.2.0 binary, extracted as shipped.
        let dir = tempfile::tempdir_in(&root).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let bin = fs::canonicalize(dir.path()).unwrap();
        assert!(
            Command::new("tar")
                .arg("-xzf")
                .arg(&official)
                .arg("-C")
                .arg(&bin)
                .status()
                .unwrap()
                .success()
        );
        let exe = bin.join("taba");
        let original = fs::read(&exe).unwrap();
        assert_eq!(version(&exe).1, "taba 0.2.0");
        // Keep v0.2.0 running from the file being replaced (it waits on stdin).
        let mut running = Command::new(&exe)
            .args(["hook", "codex", "pre-tool-use"])
            .env("HOME", dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // The candidate's v0.2.1 metadata and assets, served at the official URLs.
        let mut fetch_root = root.join("download");
        fs::create_dir_all(&fetch_root).unwrap();
        let link = fetch_root.join("v0.2.1");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(candidate.join("v0.2.1"), &link).unwrap();
        fetch_root.pop();
        let release: Release =
            serde_json::from_slice(&fs::read(candidate.join("v0.2.1.json")).unwrap()).unwrap();
        let next = release.stable_version().unwrap();
        let check = |path: &Path| install::version_check(path, &next);
        let result = install::update_at(
            &Files(fetch_root),
            &exe,
            &current,
            &release,
            &next,
            target,
            Some(&check),
        );
        drop(running.stdin.take());
        let still_running = running.wait().unwrap();
        let after = version(&exe);
        eprintln!(
            "{triple} {case}: result={} | --version after: {:?} | running v0.2.0 exited with {still_running}",
            match &result {
                Ok(out) => serde_json::to_string(&out.body).unwrap(),
                Err(error) => error.to_json().to_string(),
            },
            after
        );
        if expect_updated {
            assert_eq!(result.unwrap().body["status"], "updated");
            assert_eq!(after.1, "taba 0.2.1");
            assert!(after.0.success());
        } else {
            let text = result.err().unwrap().to_json().to_string();
            assert!(text.contains("was restored"), "{text}");
            assert_eq!(fs::read(&exe).unwrap(), original, "v0.2.0 bytes intact");
            assert_eq!(after.1, "taba 0.2.0");
        }
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let leftovers: Vec<_> = fs::read_dir(&bin)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(".taba-update-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn https_client_builds_without_network() {
    assert!(GitHub::new().is_ok());
}
