//! Check GitHub (via rootmode.ai, then the API) for a newer desktop tag.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const SITE: &str = "https://rootmode.ai/version";
const GITHUB: &str = "https://api.github.com/repos/rootmodeai/rootmode/releases/latest";
const DOWNLOAD: &str = "https://rootmode.ai/download";
pub const SETTING_SKIPPED: &str = "skipped_update";

/// What this install says about itself when it asks for the newest version,
/// so installs can be counted: a random id it made up, and its platform.
/// Sent as headers, never in the URL, so it lands in no access log. Nothing
/// about what the app was used for is ever part of it.
#[derive(Debug, Clone)]
pub struct Hello {
    pub install: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: Option<String>,
    pub available: bool,
    pub url: String,
}

#[derive(Deserialize)]
struct VersionBody {
    #[serde(default)]
    version: String,
    #[serde(default)]
    tag: String,
    #[serde(default, alias = "tag_name")]
    tag_name: String,
    #[serde(default)]
    url: String,
}

pub fn newer(current: &str, latest: &str) -> bool {
    match (parse(current), parse(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

fn parse(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut p = s.split('.');
    let major = p.next()?.parse().ok()?;
    let minor = p.next()?.parse().ok()?;
    let patch = p.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

fn tag_to_version(body: &VersionBody) -> Option<String> {
    let raw = if !body.version.is_empty() {
        body.version.as_str()
    } else if !body.tag.is_empty() {
        body.tag.as_str()
    } else {
        body.tag_name.as_str()
    };
    let v = raw.trim().trim_start_matches('v');
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

async fn fetch_json(url: &str, hello: Option<&Hello>) -> Result<VersionBody> {
    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| AppError::Net(e.to_string()))?
        .get(url)
        .header("User-Agent", "rootmode-desktop")
        .header("Accept", "application/json");
    if let Some(h) = hello {
        req = req
            .header("X-Rootmode-Install", h.install.as_str())
            .header("X-Rootmode-Version", CURRENT)
            .header("X-Rootmode-OS", h.os.as_str())
            .header("X-Rootmode-Arch", h.arch.as_str());
    }
    let text = req
        .send()
        .await
        .map_err(|e| AppError::Net(e.to_string()))?
        .error_for_status()
        .map_err(|e| AppError::Net(e.to_string()))?
        .text()
        .await
        .map_err(|e| AppError::Net(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| AppError::Net(e.to_string()))
}

pub async fn lookup(hello: Option<&Hello>) -> Result<UpdateInfo> {
    // The hello goes to the site only; GitHub is a fallback for the version
    // and learns nothing about the install.
    let body = match fetch_json(SITE, hello).await {
        Ok(b) => b,
        Err(_) => fetch_json(GITHUB, None).await?,
    };
    let latest = tag_to_version(&body);
    let url = if body.url.is_empty() {
        DOWNLOAD.to_string()
    } else {
        body.url
    };
    let available = latest.as_deref().is_some_and(|l| newer(CURRENT, l));
    Ok(UpdateInfo {
        current: CURRENT.to_string(),
        latest,
        available,
        url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_patch_is_newer() {
        assert!(newer("0.1.4", "0.1.5"));
        assert!(!newer("0.1.5", "0.1.5"));
        assert!(!newer("0.1.5", "0.1.4"));
        assert!(newer("0.1.9", "0.2.0"));
        assert!(newer("v0.1.4", "v0.1.10"));
    }
}
