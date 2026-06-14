//! Version policy for remote codex provisioning.
//!
//! # Follow-up: deduplication of GitHub release fetching
//!
//! The `resolve_latest_version` function here duplicates the GitHub
//! `releases/latest` fetch logic found in `codex-rs/cli/src/doctor/updates.rs`
//! and `codex-rs/tui/src/updates.rs`.  Unifying these into a shared crate
//! (e.g. `codex-updates`) is left as a follow-up to keep this PR's diff small.

use codex_client::build_reqwest_client_with_custom_ca;
use reqwest::StatusCode;
use serde::Deserialize;

use crate::provision::error::ProvisionError;
use crate::provision::install::normalize_version;

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
    /// This lets release builds avoid the GitHub API entirely when the managed
    /// binary already matches the desired version. `Latest` and dev/placeholder
    /// host builds must re-check, since their concrete target is the newest
    /// published release.
    pub fn is_satisfied_by_existing(&self, existing: &str) -> bool {
        match self {
            VersionPolicy::Exact(v) => canonicalize_exact_version(v)
                .is_ok_and(|version| normalize_version(existing) == version.as_str()),
            VersionPolicy::HostVersion => {
                let host_version = env!("CARGO_PKG_VERSION");
                if is_dev_version(host_version) {
                    // A dev host has no authoritative version; resolve Latest
                    // before deciding whether an existing managed binary is
                    // current enough to reuse.
                    false
                } else {
                    normalize_version(existing) == normalize_version(host_version)
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
            VersionPolicy::Exact(v) => canonicalize_exact_version(v),
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

pub(crate) fn canonicalize_exact_version(version: &str) -> Result<String, ProvisionError> {
    let trimmed = version.trim();
    let version = trimmed
        .strip_prefix("rust-v")
        .or_else(|| trimmed.strip_prefix('v'))
        .unwrap_or(trimmed);

    if version.is_empty() {
        return Err(ProvisionError::InvalidVersion(
            "version must not be empty".to_string(),
        ));
    }
    if version
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '/' | '\\'))
    {
        return Err(ProvisionError::InvalidVersion(format!(
            "version `{trimmed}` contains an invalid character"
        )));
    }
    Ok(version.to_string())
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
///
/// HTTP 403/429 (rate limit) and 404 (release not found) are returned as
/// dedicated [`ProvisionError`] variants with actionable messages.
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
    let response = request.send().await?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProvisionError::GitHubRateLimit {
            status: status.as_u16(),
        });
    }
    let json = response.error_for_status()?.text().await?;

    parse_latest_tag_name(&json)
}

/// Returns a GitHub API token from the standard environment variables, if set.
pub(crate) fn github_token() -> Option<String> {
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
