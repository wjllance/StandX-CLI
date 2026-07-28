//! Self-update: check GitHub releases and replace the running binary.
//!
//! ## Trust model (read before extending this)
//!
//! The tarball is fetched over TLS from the project's own GitHub release and its
//! SHA-256 is verified against the `checksums.txt` published beside it. That
//! protects against a truncated or corrupted download — **it does not protect
//! against a compromised release**, because the checksum comes from the same
//! place as the archive. Real supply-chain protection needs a detached signature
//! (minisign / cosign) verified against a key shipped in the binary; that is not
//! implemented, and this comment exists so nobody mistakes checksum verification
//! for provenance verification.
//!
//! ## What this deliberately refuses to do
//!
//! - It never escalates privileges. An unwritable install path is an error with
//!   instructions, not a `sudo` invocation.
//! - It never fights a package manager: a Homebrew-managed binary is left alone
//!   with a pointer to `brew upgrade`.
//! - It never acts on a version string it cannot parse. Comparison failure
//!   aborts rather than guessing which side is newer.

use crate::cli::OutputFormat;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::IsTerminal;
use std::path::Path;

const REPO: &str = "wjllance/standx-cli";
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/wjllance/standx-cli/releases";
const USER_AGENT: &str = concat!("standx-cli/", env!("CARGO_PKG_VERSION"));

/// A parsed `MAJOR.MINOR.PATCH[-PRE]` version.
///
/// Hand-rolled rather than pulling in `semver`: the only versions this compares
/// are this project's own release tags, and the parser is strict — anything it
/// cannot read becomes an error instead of a silent mis-ordering. Pre-release
/// ordering follows semver's rule that a pre-release sorts *before* its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
}

impl Version {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let text = raw.trim().trim_start_matches('v');
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
            Some((_, _)) => anyhow::bail!("version '{raw}' has an empty pre-release part"),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut next = |field: &str| -> Result<u64> {
            parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("version '{raw}' is missing its {field} component"))?
                .parse::<u64>()
                .with_context(|| format!("version '{raw}' has a non-numeric {field} component"))
        };
        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if parts.next().is_some() {
            anyhow::bail!("version '{raw}' has more than three numeric components");
        }
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    pub(crate) fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                // A release outranks any pre-release of the same core version.
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                // Dotted identifiers, numeric ones compared numerically.
                (Some(left), Some(right)) => compare_pre(left, right),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_pre(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            // Fewer identifiers sorts lower ("rc.1" < "rc.1.2").
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => {
                let ordering = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    // Numeric identifiers sort below alphanumeric ones.
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => a.cmp(b),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

/// The release-asset target triple for the running platform.
///
/// Only the three targets the release pipeline actually publishes are mapped;
/// anything else is an explicit error rather than a 404 later.
pub(crate) fn current_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => anyhow::bail!(
            "no published release asset for {os}/{arch}; build from source instead \
             (published targets: aarch64-apple-darwin, x86_64-unknown-linux-gnu, \
             aarch64-unknown-linux-gnu)"
        ),
    }
}

/// How the running binary was installed, as far as we can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallKind {
    /// A plain file we may replace.
    SelfManaged,
    /// Inside a Homebrew Cellar: the package manager owns it.
    Homebrew,
}

/// Classify an executable path. Homebrew installs live under
/// `<prefix>/Cellar/<formula>/<version>/bin`, and `<prefix>/bin/standx` is a
/// symlink into it — so this is checked on the *canonical* path.
pub(crate) fn classify_install(canonical_exe: &Path) -> InstallKind {
    let looks_like_cellar = canonical_exe
        .components()
        .any(|component| component.as_os_str() == "Cellar");
    if looks_like_cellar {
        InstallKind::Homebrew
    } else {
        InstallKind::SelfManaged
    }
}

/// One release as the update flow needs it.
#[derive(Debug, Clone)]
pub(crate) struct Release {
    pub(crate) version: Version,
    pub(crate) tag: String,
    pub(crate) url: String,
}

/// Pick the newest release we are willing to install.
///
/// Pre-releases are skipped unless `allow_prerelease`. Entries whose tag cannot
/// be parsed are skipped rather than failing the whole check: one malformed tag
/// in release history must not break `update` forever.
pub(crate) fn select_release(
    releases: &[(String, String, bool)],
    allow_prerelease: bool,
) -> Option<Release> {
    releases
        .iter()
        .filter_map(|(tag, url, flagged_prerelease)| {
            let version = Version::parse(tag).ok()?;
            // Trust either signal: a hyphenated tag or GitHub's own flag.
            if (*flagged_prerelease || version.is_prerelease()) && !allow_prerelease {
                return None;
            }
            Some(Release {
                version,
                tag: tag.clone(),
                url: url.clone(),
            })
        })
        .max_by(|left, right| left.version.cmp(&right.version))
}

/// Locate the sha256 for `asset` in a `sha256sum`-style checksums file.
pub(crate) fn checksum_for(checksums: &str, asset: &str) -> Result<String> {
    for line in checksums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        // sha256sum writes "<digest>  <name>"; the name may carry a `*` marker.
        if name.trim_start_matches('*') == asset {
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                anyhow::bail!("checksum entry for {asset} is not a sha256 digest: '{digest}'");
            }
            return Ok(digest.to_ascii_lowercase());
        }
    }
    anyhow::bail!("checksums.txt has no entry for {asset}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Extract the tag from a `.../releases/tag/<tag>` URL.
pub(crate) fn tag_from_release_url(url: &str) -> Option<String> {
    let (_, tag) = url.trim_end_matches('/').rsplit_once("/tag/")?;
    (!tag.is_empty()).then(|| tag.to_string())
}

/// Resolve the latest **stable** release without touching the REST API.
///
/// `github.com/<repo>/releases/latest` redirects to the tagged release page, and
/// that redirect is not subject to the API's 60-requests-per-hour unauthenticated
/// limit. Users behind a shared or NAT'd IP would otherwise meet a bare HTTP 403
/// on a perfectly healthy install.
async fn resolve_latest_stable(client: &reqwest::Client) -> Result<Release> {
    let response = client
        .get(format!("https://github.com/{REPO}/releases/latest"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .context("could not reach github.com to resolve the latest release")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "resolving the latest release returned HTTP {}",
            response.status().as_u16()
        );
    }
    let final_url = response.url().to_string();
    let tag = tag_from_release_url(&final_url)
        .ok_or_else(|| anyhow::anyhow!("could not read a release tag out of '{final_url}'"))?;
    let version = Version::parse(&tag).with_context(|| {
        format!("latest release tag '{tag}' is not a version this build can compare")
    })?;
    Ok(Release {
        version,
        tag,
        url: final_url,
    })
}

async fn fetch_releases(client: &reqwest::Client) -> Result<Vec<(String, String, bool)>> {
    let mut request = client
        .get(format!("{LATEST_RELEASE_API}?per_page=30"))
        .header(reqwest::header::USER_AGENT, USER_AGENT);
    // An ambient token only raises the rate limit; the endpoint is public.
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|token| !token.is_empty())
    {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .context("could not reach the GitHub releases API")?;
    let status = response.status();
    if !status.is_success() {
        // 403/429 here is almost always the unauthenticated hourly limit, which
        // is worth naming: "HTTP 403" alone reads like a permissions problem.
        if matches!(status.as_u16(), 403 | 429) {
            anyhow::bail!(
                "GitHub API rate limit reached (HTTP {}). Retry later, set GITHUB_TOKEN to raise                  the limit, or drop --pre so the check uses the rate-limit-free redirect instead.",
                status.as_u16()
            );
        }
        anyhow::bail!("GitHub releases API returned HTTP {}", status.as_u16());
    }
    let payload: serde_json::Value = response
        .json()
        .await
        .context("GitHub releases API returned a body that is not JSON")?;
    let entries = payload
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("GitHub releases API did not return a list"))?;
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let tag = entry.get("tag_name")?.as_str()?.to_string();
            let url = entry
                .get("html_url")
                .and_then(|url| url.as_str())
                .unwrap_or_default()
                .to_string();
            let prerelease = entry
                .get("prerelease")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Some((tag, url, prerelease))
        })
        .collect())
}

async fn download(client: &reqwest::Client, url: &str, what: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not download {what}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "{what} download returned HTTP {}",
            response.status().as_u16()
        );
    }
    Ok(response
        .bytes()
        .await
        .with_context(|| format!("could not read the {what} body"))?
        .to_vec())
}

fn asset_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{asset}")
}

/// `standx update`.
pub async fn handle_update(
    check_only: bool,
    assume_yes: bool,
    allow_prerelease: bool,
    force: bool,
    output: OutputFormat,
) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("this build's own version string is unparseable")?;
    let target = current_target()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("could not build an HTTP client")?;

    // Stable checks avoid the REST API entirely (see resolve_latest_stable);
    // only --pre needs the release list, and only that path can be rate-limited.
    let latest = if allow_prerelease {
        let releases = fetch_releases(&client).await?;
        select_release(&releases, true)
            .ok_or_else(|| anyhow::anyhow!("no installable release found"))?
    } else {
        resolve_latest_stable(&client).await?
    };

    let newer = latest.version > current;
    if output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "action": "update_check",
                "current": current.to_string(),
                "latest": latest.version.to_string(),
                "latest_tag": latest.tag,
                "update_available": newer,
                "target": target,
                "release_url": latest.url,
            })
        );
    } else {
        println!("current: {current}");
        println!("latest:  {} ({})", latest.version, latest.tag);
        if !latest.url.is_empty() {
            println!("notes:   {}", latest.url);
        }
    }

    if check_only {
        if output != OutputFormat::Json {
            println!(
                "{}",
                if newer {
                    "→ an update is available; run `standx update` to install it"
                } else {
                    "→ already up to date"
                }
            );
        }
        return Ok(());
    }
    if !newer && !force {
        if output != OutputFormat::Json {
            println!("→ already up to date (use --force to reinstall {current})");
        }
        return Ok(());
    }

    // --- everything below replaces a file on disk ---

    let exe = std::env::current_exe().context("could not locate the running executable")?;
    let exe = exe
        .canonicalize()
        .unwrap_or_else(|_| exe.clone())
        .to_path_buf();
    if classify_install(&exe) == InstallKind::Homebrew {
        anyhow::bail!(
            "this binary is Homebrew-managed ({}); update it with `brew upgrade standx-cli` \
             so the formula and the binary stay in agreement",
            exe.display()
        );
    }
    let parent = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable path {} has no parent", exe.display()))?;
    // Fail before downloading anything if we could not install it anyway.
    ensure_writable(parent, &exe)?;

    if !assume_yes && !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to replace {} without confirmation; pass --yes for non-interactive use",
            exe.display()
        );
    }
    if !assume_yes {
        eprint!("Replace {} with {}? [y/N] ", exe.display(), latest.version);
        use std::io::Write;
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("could not read confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted; nothing was changed");
            return Ok(());
        }
    }

    let asset = format!("standx-{}-{}.tar.gz", latest.tag, target);
    if output != OutputFormat::Json {
        eprintln!("downloading {asset}…");
    }
    let archive = download(&client, &asset_url(&latest.tag, &asset), &asset).await?;
    let checksums = download(
        &client,
        &asset_url(&latest.tag, "checksums.txt"),
        "checksums.txt",
    )
    .await?;
    let checksums = String::from_utf8(checksums).context("checksums.txt is not UTF-8")?;

    let expected = checksum_for(&checksums, &asset)?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {asset}: expected {expected}, got {actual}. \
             Nothing was installed."
        );
    }

    // Stage inside the install directory so the final rename is atomic (a
    // cross-filesystem rename is not).
    let staging = parent.join(format!(".standx-update-{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("could not create staging dir {}", staging.display()))?;
    let result = install_from_archive(&archive, &staging, &exe, &latest.version);
    // Best-effort cleanup either way; a leftover staging dir must not mask the
    // real error.
    let _ = std::fs::remove_dir_all(&staging);
    result?;

    if output == OutputFormat::Json {
        println!(
            "{}",
            serde_json::json!({
                "action": "update_applied",
                "from": current.to_string(),
                "to": latest.version.to_string(),
                "path": exe.display().to_string(),
            })
        );
    } else {
        println!(
            "✅ updated {current} → {} ({})",
            latest.version,
            exe.display()
        );
    }
    Ok(())
}

/// Verify the install directory and the target file are writable by us.
fn ensure_writable(parent: &Path, exe: &Path) -> Result<()> {
    let probe = parent.join(format!(".standx-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
        }
        Err(error) => {
            anyhow::bail!(
                "cannot write to {} ({error}); re-run from a writable install or reinstall with \
                 install.sh into a directory you own. This command deliberately does not \
                 elevate privileges.",
                parent.display()
            );
        }
    }
    let metadata =
        std::fs::metadata(exe).with_context(|| format!("could not stat {}", exe.display()))?;
    if metadata.permissions().readonly() {
        anyhow::bail!("{} is read-only; refusing to replace it", exe.display());
    }
    Ok(())
}

/// Extract, sanity-check and atomically move the new binary into place.
fn install_from_archive(
    archive: &[u8],
    staging: &Path,
    exe: &Path,
    expected: &Version,
) -> Result<()> {
    let tarball = staging.join("standx.tar.gz");
    std::fs::write(&tarball, archive)
        .with_context(|| format!("could not write {}", tarball.display()))?;
    // System tar rather than two extra crates; both published targets are Unix.
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(staging)
        .status()
        .context("could not run `tar` to unpack the release")?;
    if !status.success() {
        anyhow::bail!("`tar -xzf` failed with {status}; nothing was installed");
    }
    let unpacked = staging.join("standx");
    if !unpacked.is_file() {
        anyhow::bail!("release archive did not contain a `standx` binary; nothing was installed");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unpacked, std::fs::Permissions::from_mode(0o755))
            .context("could not mark the new binary executable")?;
    }
    // Confirm the thing we are about to install is the version we asked for.
    // The archive already matched its published checksum; this catches a
    // mislabeled release before it replaces a working binary.
    let reported = std::process::Command::new(&unpacked)
        .arg("--version")
        .output()
        .context("could not run the downloaded binary to confirm its version")?;
    let reported = String::from_utf8_lossy(&reported.stdout);
    let reported = reported
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_string();
    match Version::parse(&reported) {
        Ok(found) if &found == expected => {}
        Ok(found) => anyhow::bail!(
            "downloaded binary reports {found} but the release is {expected}; \
             nothing was installed"
        ),
        Err(error) => anyhow::bail!(
            "downloaded binary reported an unusable version ('{reported}': {error}); \
             nothing was installed"
        ),
    }
    // Replacing a running executable by rename is fine on Unix: the old inode
    // stays alive for this process, and the new name is visible atomically.
    std::fs::rename(&unpacked, exe)
        .with_context(|| format!("could not move the new binary onto {}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_and_orders_releases_above_prereleases() {
        assert!(Version::parse("1.1.0").unwrap() > Version::parse("1.0.9").unwrap());
        assert!(Version::parse("v1.1.0").unwrap() > Version::parse("v1.0.9").unwrap());
        assert!(Version::parse("1.2.0").unwrap() > Version::parse("1.1.9").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.99.99").unwrap());
        // A release outranks its own pre-releases; that ordering decides whether
        // an rc build sees the final as an update.
        assert!(Version::parse("1.1.0").unwrap() > Version::parse("1.1.0-rc.1").unwrap());
        assert!(Version::parse("1.1.0-rc.2").unwrap() > Version::parse("1.1.0-rc.1").unwrap());
        assert!(Version::parse("1.1.0-rc.1.1").unwrap() > Version::parse("1.1.0-rc.1").unwrap());
        // Numeric identifiers compare numerically, not lexically.
        assert!(Version::parse("1.1.0-rc.10").unwrap() > Version::parse("1.1.0-rc.9").unwrap());
        assert_eq!(
            Version::parse("1.1.0").unwrap(),
            Version::parse("v1.1.0").unwrap()
        );
    }

    /// Unparseable versions must never be guessed at: this comparison gates
    /// overwriting a binary.
    #[test]
    fn version_parsing_is_strict() {
        for bad in ["", "1.1", "1.1.0.0", "1.1.x", "latest", "v", "1.1.0-"] {
            assert!(Version::parse(bad).is_err(), "should reject '{bad}'");
        }
    }

    #[test]
    fn select_release_skips_prereleases_and_unparseable_tags() {
        let releases = vec![
            ("v1.1.0".to_string(), "u1".to_string(), false),
            ("v1.2.0-rc.1".to_string(), "u2".to_string(), true),
            ("nightly".to_string(), "u3".to_string(), false),
            ("v1.0.0".to_string(), "u4".to_string(), false),
        ];
        let stable = select_release(&releases, false).expect("a stable release exists");
        assert_eq!(stable.tag, "v1.1.0");
        let with_pre = select_release(&releases, true).expect("a prerelease exists");
        assert_eq!(with_pre.tag, "v1.2.0-rc.1");

        // A hyphenated tag counts as a pre-release even if GitHub's flag says no.
        let mislabeled = vec![("v2.0.0-beta".to_string(), "u".to_string(), false)];
        assert!(select_release(&mislabeled, false).is_none());
        assert!(select_release(&mislabeled, true).is_some());
        assert!(select_release(&[], false).is_none());
    }

    #[test]
    fn checksum_lookup_matches_the_published_format() {
        let checksums = "\
abc  standx-v1.1.0-aarch64-apple-darwin.tar.gz
0000000000000000000000000000000000000000000000000000000000000000 *standx-v1.1.0-x86_64-unknown-linux-gnu.tar.gz
1111111111111111111111111111111111111111111111111111111111111111  standx-v1.1.0-aarch64-unknown-linux-gnu.tar.gz
";
        assert_eq!(
            checksum_for(checksums, "standx-v1.1.0-aarch64-unknown-linux-gnu.tar.gz").unwrap(),
            "1".repeat(64)
        );
        // sha256sum's binary-mode `*` prefix must not hide the entry.
        assert_eq!(
            checksum_for(checksums, "standx-v1.1.0-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "0".repeat(64)
        );
        // A truncated digest is rejected rather than compared loosely.
        assert!(checksum_for(checksums, "standx-v1.1.0-aarch64-apple-darwin.tar.gz").is_err());
        assert!(checksum_for(checksums, "standx-v9.9.9-unknown.tar.gz").is_err());
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A Homebrew-managed binary must be left to `brew`, or the formula and the
    /// installed binary silently disagree.
    #[test]
    fn homebrew_installs_are_detected_on_the_canonical_path() {
        assert_eq!(
            classify_install(Path::new(
                "/opt/homebrew/Cellar/standx-cli/1.1.0/bin/standx"
            )),
            InstallKind::Homebrew
        );
        assert_eq!(
            classify_install(Path::new("/usr/local/Cellar/standx-cli/1.0.0/bin/standx")),
            InstallKind::Homebrew
        );
        assert_eq!(
            classify_install(Path::new("/usr/local/bin/standx")),
            InstallKind::SelfManaged
        );
        assert_eq!(
            classify_install(Path::new("/home/me/.local/bin/standx")),
            InstallKind::SelfManaged
        );
    }

    /// The stable path reads the tag out of the redirect target, so this parse
    /// is what keeps `update` working when the REST API is rate-limited.
    #[test]
    fn tag_is_read_from_the_release_redirect_url() {
        assert_eq!(
            tag_from_release_url("https://github.com/wjllance/standx-cli/releases/tag/v1.1.0")
                .as_deref(),
            Some("v1.1.0")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/wjllance/standx-cli/releases/tag/v1.1.0/")
                .as_deref(),
            Some("v1.1.0")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases/tag/v2.0.0-rc.1").as_deref(),
            Some("v2.0.0-rc.1")
        );
        // No tag segment (e.g. a repo with zero releases) must not be guessed.
        assert!(tag_from_release_url("https://github.com/o/r/releases").is_none());
        assert!(tag_from_release_url("https://github.com/o/r/releases/tag/").is_none());
    }

    #[test]
    fn asset_url_matches_the_release_layout() {
        assert_eq!(
            asset_url("v1.1.0", "standx-v1.1.0-aarch64-apple-darwin.tar.gz"),
            "https://github.com/wjllance/standx-cli/releases/download/v1.1.0/standx-v1.1.0-aarch64-apple-darwin.tar.gz"
        );
    }
}
