//! Error type for remote provisioning operations.

use codex_client::BuildCustomCaTransportError;

/// Errors that can occur while probing or provisioning a remote codex binary.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The remote platform or architecture is not supported.
    #[error("unsupported remote platform: os={os}, arch={arch}")]
    UnsupportedPlatform { os: String, arch: String },

    /// The probe command returned unexpected output.
    #[error("failed to parse probe output: {0}")]
    ProbeOutputParse(String),

    /// The launcher command failed to spawn or run.
    #[error("launcher command failed: {0}")]
    LauncherIo(#[source] std::io::Error),

    /// The launcher command exited with a non-zero status.
    #[error("launcher command exited with status {status}: {stderr}")]
    LauncherNonZero { status: i32, stderr: String },

    /// A network request to GitHub failed.
    #[error("failed to fetch release from GitHub: {0}")]
    Http(#[from] reqwest::Error),

    /// Building the HTTP client failed (e.g. bad CA certificate path).
    #[error("failed to build HTTP client: {0}")]
    HttpClientBuild(#[from] BuildCustomCaTransportError),

    /// GitHub API rate limit hit (HTTP 403 or 429).
    ///
    /// Set GITHUB_TOKEN (or GH_TOKEN / CODEX_GITHUB_TOKEN) to raise the limit,
    /// or retry after the rate-limit window resets.
    #[error("GitHub API rate limit exceeded (HTTP {status}): set GITHUB_TOKEN or retry later")]
    GitHubRateLimit { status: u16 },

    /// The requested release asset was not found on GitHub (HTTP 404).
    #[error(
        "release asset not found on GitHub for triple '{triple}' version '{version}' (HTTP 404)"
    )]
    AssetNotFound { triple: String, version: String },

    /// The downloaded archive digest did not match the expected value.
    #[error("SHA-256 mismatch for {asset}: expected {expected}, got {actual}")]
    DigestMismatch {
        asset: String,
        expected: String,
        actual: String,
    },

    /// The expected asset was not found in the SHA256SUMS file.
    #[error("asset {asset} not found in SHA256SUMS")]
    AssetNotInSums { asset: String },

    /// Failed to resolve the latest release version from GitHub.
    #[error("failed to resolve latest release version: {0}")]
    LatestVersionResolve(String),

    /// A requested exact version string is invalid.
    #[error("invalid codex version: {0}")]
    InvalidVersion(String),

    /// The remote codex binary reported an unexpected version after install.
    #[error("post-install version check failed: expected {expected}, got {actual}")]
    VersionCheckFailed { expected: String, actual: String },

    /// Installing the concrete required version failed.
    #[error("failed to install required codex version {version}: {source}")]
    InstallRequiredVersionFailed {
        version: String,
        #[source]
        source: Box<ProvisionError>,
    },

    /// A command exceeded its allowed time limit.
    #[error("command timed out after {secs}s: {context}")]
    Timeout { secs: u64, context: String },

    /// An I/O error occurred when writing to a temporary file during install.
    #[error("temporary file I/O error during install: {0}")]
    TempFileIo(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::ProvisionError;

    #[test]
    fn install_required_version_failed_names_requested_version() {
        let err = ProvisionError::InstallRequiredVersionFailed {
            version: "1.2.3".to_string(),
            source: Box::new(ProvisionError::AssetNotInSums {
                asset: "codex-package-x86_64-unknown-linux-musl.tar.gz".to_string(),
            }),
        };

        assert!(
            err.to_string()
                .contains("failed to install required codex version 1.2.3"),
            "{err}"
        );
    }
}
