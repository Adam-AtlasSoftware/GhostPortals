//! Finding, downloading and unpacking releases from GitHub.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const REPO: &str = "Adam-AtlasSoftware/GhostPortals";
pub const RELEASES_PAGE: &str = "https://github.com/Adam-AtlasSoftware/GhostPortals/releases";

/// The Windows bundle every release carries.
pub const WINDOWS_ASSET: &str = "ghostportals-windows.zip";

/// This build's version.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo versions are semver")
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub html_url: String,
    pub draft: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
    /// "sha256:<hex>", which GitHub computes for every upload.
    #[serde(default)]
    pub digest: Option<String>,
}

impl Release {
    pub fn version(&self) -> Option<Version> {
        Version::parse(self.tag_name.trim_start_matches('v')).ok()
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

fn agent() -> ureq::Agent {
    let builder = ureq::Agent::config_builder();
    #[cfg(windows)]
    let builder = builder.tls_config(
        ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).build(),
    );
    builder
        .user_agent(concat!("ghostportals/", env!("CARGO_PKG_VERSION")))
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

/// The newest published release, pre-releases included: every release so far
/// is one.
pub fn latest() -> Result<Release> {
    let releases: Vec<Release> = agent()
        .get(format!("https://api.github.com/repos/{REPO}/releases?per_page=20"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("asking GitHub for releases")?
        .body_mut()
        .read_json()
        .context("reading GitHub's answer")?;
    newest(releases).ok_or_else(|| anyhow!("no releases published yet"))
}

fn newest(releases: Vec<Release>) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| Some((r.version()?, r)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, r)| r)
}

/// Downloads `asset` to `dest`, counting bytes into `progress`, and checks
/// GitHub's SHA-256 digest when it has one.
pub fn download(asset: &Asset, dest: &Path, progress: &Arc<AtomicU64>) -> Result<()> {
    let mut response = agent()
        .get(&asset.browser_download_url)
        .call()
        .with_context(|| format!("downloading {}", asset.name))?;
    let mut reader = response.body_mut().with_config().limit(2 << 30).reader();
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.sync_all()?;
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    match asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
        Some(want) if !want.eq_ignore_ascii_case(&got) => {
            let _ = std::fs::remove_file(dest);
            bail!("{} is corrupt (SHA-256 {got}, expected {want})", asset.name)
        }
        _ => Ok(()),
    }
}

/// Unpacks the Windows bundle into `dir`, dropping the zip's top-level
/// folder. Returns the files written.
pub fn unpack(zip: &Path, dir: &Path) -> Result<Vec<PathBuf>> {
    let file = std::fs::File::open(zip)?;
    let mut archive = zip::ZipArchive::new(file).context("not a zip file")?;
    let mut written = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name() else { continue };
        // Bundles are "ghostportals-windows/<files>"; strip that folder.
        let rel: PathBuf = name.components().skip(1).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dir.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f =
            std::fs::File::create(&out).with_context(|| format!("writing {}", out.display()))?;
        std::io::copy(&mut entry, &mut f)?;
        written.push(out);
    }
    if written.is_empty() {
        bail!("the bundle is empty");
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, draft: bool) -> Release {
        Release { tag_name: tag.into(), html_url: String::new(), draft, assets: vec![] }
    }

    #[test]
    fn newest_orders_by_semver_and_skips_drafts() {
        let r = newest(vec![
            release("v0.1.0-alpha.9", false),
            release("v0.1.0-alpha.10", false),
            release("v0.2.0", true),
            release("nightly", false),
        ])
        .unwrap();
        assert_eq!(r.tag_name, "v0.1.0-alpha.10");
        assert!(r.version().unwrap() > Version::parse("0.1.0-alpha.9").unwrap());
        assert!(Version::parse("0.1.0").unwrap() > Version::parse("0.1.0-alpha.10").unwrap());
    }

    #[test]
    fn unpack_strips_the_top_folder() {
        let dir = std::env::temp_dir().join(format!("gp-unpack-{}", std::process::id()));
        let zip_path = dir.join("b.zip");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("ghostportals-windows/ghostportals.exe", opts).unwrap();
            w.write_all(b"exe").unwrap();
            w.start_file("ghostportals-windows/gstreamer/plugins/gstapp.dll", opts).unwrap();
            w.write_all(b"dll").unwrap();
            w.finish().unwrap();
        }
        let out = dir.join("install");
        let files = unpack(&zip_path, &out).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(std::fs::read(out.join("ghostportals.exe")).unwrap(), b"exe");
        assert!(out.join("gstreamer/plugins/gstapp.dll").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
