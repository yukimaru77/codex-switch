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

    /// The remote codex binary reported an unexpected version after install.
    #[error("post-install version check failed: expected {expected}, got {actual}")]
    VersionCheckFailed { expected: String, actual: String },

    /// A command exceeded its allowed time limit.
    #[error("command timed out after {secs}s: {context}")]
    Timeout { secs: u64, context: String },
}
