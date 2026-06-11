//! Version policy for remote codex provisioning.

use crate::provision::error::ProvisionError;

/// Determines which version of codex to provision on the remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionPolicy {
    /// Install exactly this version string (e.g. `"1.2.3"`).
    Exact(String),
    /// Match the version of the running host binary (`CARGO_PKG_VERSION`).
    /// Falls back to [`VersionPolicy::Latest`] when the host version is a dev
    /// build (`"0.0.0"` or contains `"-dev"`).
    HostVersion,
    /// Fetch the latest published release from the GitHub API.
    Latest,
}

impl VersionPolicy {
    /// Resolves this policy to a concrete version string, possibly hitting the
    /// GitHub API to discover the latest release.
    ///
    /// # Network
    /// Only [`VersionPolicy::Latest`] (and [`VersionPolicy::HostVersion`] when
    /// it falls back) performs a network request.
    pub async fn resolve(&self) -> Result<String, ProvisionError> {
        match self {
            VersionPolicy::Exact(v) => Ok(v.clone()),
            VersionPolicy::HostVersion => {
                let host_version = env!("CARGO_PKG_VERSION");
                if is_dev_version(host_version) {
                    resolve_latest_version().await
                } else {
                    Ok(host_version.to_string())
                }
            }
            VersionPolicy::Latest => resolve_latest_version().await,
        }
    }
}

/// Returns `true` when `v` is a development/placeholder version that should
/// not be published to the remote.
pub(crate) fn is_dev_version(v: &str) -> bool {
    v == "0.0.0" || v.contains("-dev")
}

/// Fetches `https://api.github.com/repos/openai/codex/releases/latest` and
/// extracts the version from the `tag_name` field (`rust-vX.Y.Z`).
pub(crate) async fn resolve_latest_version() -> Result<String, ProvisionError> {
    let client = reqwest::Client::builder()
        .user_agent("codex-exec-server")
        .build()?;
    let resp = client
        .get("https://api.github.com/repos/openai/codex/releases/latest")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    parse_latest_tag_name(&resp)
}

/// Parses the `tag_name` from a GitHub releases/latest JSON response.
///
/// Expected format: `"tag_name": "rust-vX.Y.Z"`.
pub(crate) fn parse_latest_tag_name(json: &str) -> Result<String, ProvisionError> {
    // Use a simple string search to avoid pulling in a JSON parser just for
    // this one field.
    for part in json.split('"') {
        if let Some(version) = part.strip_prefix("rust-v")
            && !version.is_empty()
        {
            return Ok(version.to_string());
        }
    }
    Err(ProvisionError::LatestVersionResolve(
        "tag_name with prefix rust-v not found in response".to_string(),
    ))
}

#[cfg(test)]
#[path = "version_tests.rs"]
mod tests;
