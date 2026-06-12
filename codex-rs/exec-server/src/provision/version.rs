//! Version policy for remote codex provisioning.

use codex_client::build_reqwest_client_with_custom_ca;
use serde::Deserialize;

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
    /// Returns `true` when an already-installed remote binary reporting
    /// `existing` satisfies this policy *without* a network round-trip.
    ///
    /// This lets the reuse path avoid the GitHub API entirely (important when
    /// the API is rate-limited): an explicit `Exact` match is reused as-is, and
    /// a dev/placeholder host build reuses whatever codex is already present
    /// rather than forcing a `Latest` lookup it has no authority to demand.
    /// `Latest` always re-checks, since the whole point is to pull the newest.
    pub fn is_satisfied_by_existing(&self, existing: &str) -> bool {
        match self {
            VersionPolicy::Exact(v) => existing == v,
            VersionPolicy::HostVersion => {
                let host_version = env!("CARGO_PKG_VERSION");
                if is_dev_version(host_version) {
                    // A dev host has no authoritative version; reuse any
                    // existing remote codex instead of resolving Latest.
                    true
                } else {
                    existing == host_version
                }
            }
            VersionPolicy::Latest => false,
        }
    }

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

/// Minimal deserialization target for the GitHub releases/latest response.
///
/// Only `tag_name` is extracted; all other fields are ignored so the parser
/// is robust to schema additions and to `body` fields that happen to contain
/// the text `rust-v`.
#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

/// Fetches `https://api.github.com/repos/openai/codex/releases/latest` and
/// extracts the version from the `tag_name` field (`rust-vX.Y.Z`).
///
/// Uses the shared workspace HTTP client so `CODEX_CA_CERTIFICATE` and proxy
/// settings are inherited automatically.
pub(crate) async fn resolve_latest_version() -> Result<String, ProvisionError> {
    let client = build_reqwest_client_with_custom_ca(
        reqwest::Client::builder().user_agent("codex-exec-server"),
    )?;
    let mut request = client.get("https://api.github.com/repos/openai/codex/releases/latest");
    // Authenticate when a token is available so the unauthenticated 60 req/hr
    // limit (which breaks provisioning under repeated use) is lifted to the
    // authenticated 5000 req/hr limit.
    if let Some(token) = github_token() {
        request = request.bearer_auth(token);
    }
    let json = request.send().await?.error_for_status()?.text().await?;

    parse_latest_tag_name(&json)
}

/// Returns a GitHub API token from the standard environment variables, if set.
fn github_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN", "CODEX_GITHUB_TOKEN"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|token| !token.trim().is_empty())
}

/// Parses the `tag_name` from a GitHub releases/latest JSON response and
/// strips the `rust-v` prefix to return the bare version string.
///
/// Uses `serde_json` so the result is correct even when the `body` or `name`
/// fields of the response contain `rust-v` substrings.
pub(crate) fn parse_latest_tag_name(json: &str) -> Result<String, ProvisionError> {
    let release: GitHubRelease = serde_json::from_str(json).map_err(|e| {
        ProvisionError::LatestVersionResolve(format!("failed to parse GitHub releases JSON: {e}"))
    })?;

    release
        .tag_name
        .strip_prefix("rust-v")
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ProvisionError::LatestVersionResolve(format!(
                "tag_name {:?} does not have expected rust-v prefix",
                release.tag_name
            ))
        })
}

#[cfg(test)]
#[path = "version_tests.rs"]
mod tests;
