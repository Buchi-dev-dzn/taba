//! Explicit updates from the official release repository. Never opens the taba state store.
use crate::error::{Output, Result, TabaError};
use reqwest::{Url, blocking::Client, redirect::Policy};
use semver::Version;
use serde::Deserialize;
use serde_json::json;
use std::{cmp::Ordering, io::Read, time::Duration};

#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod archive;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod install;
mod progress;
pub use progress::Progress;
#[cfg(test)]
mod tests;

const REPO: &str = "Buchi-dev-dzn/taba";
const RELEASES: &str = "https://github.com/Buchi-dev-dzn/taba/releases";
const METADATA_LIMIT: usize = 4 * 1024 * 1024;
const MAX_PAGES: usize = 5;
const PER_PAGE: usize = 100;

fn failure(message: impl Into<String>) -> TabaError {
    TabaError::new("E_UPDATE_FAILED", message)
}

/// How a request that GitHub answered with 404 Not Found fails.
const HTTP_NOT_FOUND: &str = "GitHub release request returned HTTP 404";

/// Whether `e` is a request failure because the resource does not exist.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn is_not_found(e: &TabaError) -> bool {
    e.code == "E_UPDATE_FAILED" && e.message.starts_with(HTTP_NOT_FOUND)
}

fn manual(version: &Version, reason: &str) -> Output {
    Output::new(
        json!({"status": "manual_update", "available": version.to_string(),
               "reason": reason, "releases": RELEASES}),
        format!(
            "taba v{version} is available. {reason}\nUse the original installation method, or follow the verified archive instructions at {RELEASES}."
        ),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    triple: &'static str,
    extension: &'static str,
}

fn target(triple: &str) -> Option<Target> {
    let (triple, extension) = match triple {
        "aarch64-apple-darwin" => ("aarch64-apple-darwin", "tar.gz"),
        "x86_64-apple-darwin" => ("x86_64-apple-darwin", "tar.gz"),
        "aarch64-unknown-linux-gnu" => ("aarch64-unknown-linux-gnu", "tar.gz"),
        "x86_64-unknown-linux-gnu" => ("x86_64-unknown-linux-gnu", "tar.gz"),
        "x86_64-pc-windows-msvc" => ("x86_64-pc-windows-msvc", "zip"),
        _ => return None,
    };
    Some(Target { triple, extension })
}

impl Target {
    fn asset(self, tag: &str) -> String {
        format!("taba-{tag}-{}.{}", self.triple, self.extension)
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    state: String,
    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    size: usize,
}

impl Release {
    fn stable_version(&self) -> Option<Version> {
        let version = Version::parse(self.tag_name.strip_prefix('v')?).ok()?;
        (!self.draft && !self.prerelease && version.pre.is_empty()).then_some(version)
    }

    #[cfg(any(target_os = "linux", target_os = "macos", test))]
    fn asset_size(&self, name: &str, limit: usize) -> Result<usize> {
        let mut entries = self.assets.iter().filter(|asset| asset.name == name);
        match (entries.next(), entries.next()) {
            (Some(asset), None)
                if asset.state == "uploaded" && (1..=limit).contains(&asset.size) =>
            {
                Ok(asset.size)
            }
            _ => Err(failure(format!(
                "missing, duplicate, or invalid release asset {name}"
            ))),
        }
    }
}

fn newest(releases: Vec<Release>, current: &Version) -> Result<Option<(Version, Release)>> {
    let mut best: Option<(Version, Release)> = None;
    let mut ambiguous = false;
    for release in releases {
        let Some(version) = release.stable_version() else {
            continue;
        };
        if version.cmp_precedence(current) != Ordering::Greater {
            continue;
        }
        match best.as_ref().map(|(best, _)| version.cmp_precedence(best)) {
            None | Some(Ordering::Greater) => {
                best = Some((version, release));
                ambiguous = false;
            }
            Some(Ordering::Equal) => {
                ambiguous = true;
            }
            Some(Ordering::Less) => {}
        }
    }
    if ambiguous {
        return Err(failure(
            "multiple stable releases have the highest SemVer precedence",
        ));
    }
    Ok(best)
}

#[derive(Clone, Copy)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", test)),
    allow(dead_code)
)]
enum Download {
    Api,
    Asset,
}

fn api_list(page: usize) -> Url {
    Url::parse(&format!(
        "https://api.github.com/repos/{REPO}/releases?per_page={PER_PAGE}&page={page}"
    ))
    .expect("fixed API base")
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn api_tag(tag: &str) -> Url {
    let mut url = Url::parse(&format!(
        "https://api.github.com/repos/{REPO}/releases/tags"
    ))
    .expect("fixed API base");
    url.path_segments_mut().unwrap().push(tag);
    url
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn asset_url(tag: &str, name: &str) -> Url {
    let mut url = Url::parse(&format!("{RELEASES}/download")).expect("fixed release base");
    url.path_segments_mut().unwrap().push(tag).push(name);
    url
}

fn asset_redirect(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
        && (url.path().starts_with("/github-production-release-asset/")
            || url.path().starts_with("/github-production-release-asset-"))
}

trait Fetch {
    fn get(&self, kind: Download, url: Url, limit: usize) -> Result<Vec<u8>>;

    /// Where download and verification progress is shown, if anywhere.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", test)),
        allow(dead_code)
    )]
    fn progress(&self) -> Option<&Progress> {
        None
    }
}

struct GitHub(Client, Progress);

impl GitHub {
    fn new(progress: Progress) -> Result<Self> {
        Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("taba/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(|client| Self(client, progress))
            .map_err(|_| failure("could not initialize GitHub HTTPS client"))
    }
}

impl Fetch for GitHub {
    fn progress(&self) -> Option<&Progress> {
        Some(&self.1)
    }

    fn get(&self, kind: Download, mut url: Url, limit: usize) -> Result<Vec<u8>> {
        let bar_name = bar_name(kind, &url);
        // Initial URLs are constructed above; no metadata, proxies, or user config select an origin.
        for hop in 0..=1 {
            let mut request = self
                .0
                .get(url.clone())
                .header("Accept-Encoding", "identity");
            if matches!(kind, Download::Api) {
                request = request
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28");
            }
            let response = request.send().map_err(|error| {
                failure(format!(
                    "GitHub release request failed: {}",
                    error.without_url()
                ))
            })?;
            if response.status().is_redirection() {
                if matches!(kind, Download::Asset) && hop == 0 {
                    let redirect = response
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| Url::parse(s).ok());
                    if let Some(next) = redirect.filter(asset_redirect) {
                        url = next;
                        continue;
                    }
                }
                return Err(failure("refused unexpected GitHub release redirect"));
            }
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(failure(format!("{HTTP_NOT_FOUND} Not Found")));
            }
            if response.status() != reqwest::StatusCode::OK {
                return Err(failure(format!(
                    "GitHub release request returned HTTP {}",
                    response.status()
                )));
            }
            if response.content_length().is_some_and(|n| n > limit as u64) {
                return Err(failure("GitHub release response exceeded its size limit"));
            }
            let total = response.content_length();
            return match bar_name {
                Some(name) if self.1.is_enabled() => {
                    read_with_progress(response, limit, total, &self.1, &name)
                }
                _ => read_bounded(response, limit),
            };
        }
        unreachable!("each request either returns or takes one redirect")
    }
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure(format!("could not read update data: {e}")))?;
    if bytes.len() > limit {
        return Err(failure("update data exceeded its size limit"));
    }
    Ok(bytes)
}

/// The asset name to draw a download bar for: archives get one; metadata
/// and SHA256SUMS are small.
fn bar_name(kind: Download, url: &Url) -> Option<String> {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|n| matches!(kind, Download::Asset) && *n != "SHA256SUMS")
        .map(str::to_string)
}

/// `read_bounded` that also draws a download bar for `name`.
fn read_with_progress(
    mut reader: impl Read,
    limit: usize,
    total: Option<u64>,
    progress: &Progress,
    name: &str,
) -> Result<Vec<u8>> {
    progress.start(name, total);
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                if bytes.len() > limit {
                    progress.abandon();
                    return Err(failure("update data exceeded its size limit"));
                }
                progress.advance(bytes.len() as u64);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                progress.abandon();
                return Err(failure(format!("could not read update data: {e}")));
            }
        }
    }
    progress.done();
    Ok(bytes)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn download_asset(
    fetch: &impl Fetch,
    release: &Release,
    name: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    let size = release.asset_size(name, limit)?;
    let bytes = fetch.get(Download::Asset, asset_url(&release.tag_name, name), size)?;
    if bytes.len() != size {
        return Err(failure(format!(
            "release asset length does not match metadata: {name}"
        )));
    }
    Ok(bytes)
}

fn discover(fetch: &impl Fetch, current: &Version) -> Result<Option<(Version, Release)>> {
    let mut releases = Vec::new();
    for page in 1..=MAX_PAGES {
        let bytes = fetch.get(Download::Api, api_list(page), METADATA_LIMIT)?;
        let mut entries: Vec<Release> = serde_json::from_slice(&bytes)
            .map_err(|_| failure("invalid GitHub release metadata"))?;
        let count = entries.len();
        if count > PER_PAGE {
            return Err(failure("unexpected GitHub release page size"));
        }
        releases.append(&mut entries);
        if count < PER_PAGE {
            return newest(releases, current);
        }
    }
    Err(failure(
        "release listing exceeded its page limit; use GitHub Releases manually",
    ))
}

fn run(
    fetch: &impl Fetch,
    check: bool,
    current: &Version,
    build_target: &str,
    install: impl FnOnce(&Release, &Version, Target) -> Result<Output>,
) -> Result<Output> {
    let Some((version, release)) = discover(fetch, current)? else {
        return Ok(Output::new(
            json!({"status": "up_to_date", "current": current.to_string()}),
            format!("No newer stable taba release (current: v{current})."),
        ));
    };
    let asset = target(build_target)
        .map(|target| target.asset(&release.tag_name))
        .filter(|name| {
            release
                .assets
                .iter()
                .filter(|asset| asset.name == *name)
                .count()
                == 1
        });
    if check {
        return Ok(Output::new(
            json!({"status": "update_available", "current": current.to_string(),
                "available": version.to_string(), "asset": asset, "releases": RELEASES}),
            format!(
                "taba v{version} is available (current: v{current}).\n{}\n{RELEASES}",
                asset.map_or_else(
                    || "No matching archive listed for this build.".into(),
                    |name| format!("Archive: {name}")
                )
            ),
        ));
    }
    match target(build_target) {
        Some(target) if asset.is_some() => install(&release, &version, target),
        _ => Ok(manual(
            &version,
            "No unique official archive matches this build.",
        )),
    }
}

pub(super) fn command(check: bool, json: bool) -> Result<Output> {
    let current =
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version is SemVer");
    // `--check` downloads nothing, so it shows no progress.
    let progress = if check {
        Progress::disabled()
    } else {
        Progress::for_terminal(json)
    };
    let fetch = GitHub::new(progress)?;
    run(
        &fetch,
        check,
        &current,
        env!("TABA_BUILD_TARGET"),
        |release, version, target| {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                install::update(&fetch, &current, release, version, target)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (release, target);
                let reason = if cfg!(windows) {
                    "Windows is check-only: taba will not replace its loaded .exe. Close taba processes before manually replacing it."
                } else {
                    "Automatic replacement is supported only for eligible standalone Linux and macOS installations."
                };
                Ok(manual(version, reason))
            }
        },
    )
}
