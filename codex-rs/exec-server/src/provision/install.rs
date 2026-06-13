//! Core provisioning logic: download, verify, stream and install codex remotely.

use std::fmt::Write as FmtWrite;
use std::process::Stdio;
use std::time::Duration;

use codex_client::build_reqwest_client_with_custom_ca;
use futures::StreamExt;
use reqwest::StatusCode;
use sha2::Digest;
use sha2::Sha256;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::provision::ProvisionError;
use crate::provision::RemoteLauncher;
use crate::provision::VersionPolicy;
use crate::provision::launcher::posix_single_quote;
use crate::provision::probe::probe;
use crate::provision::triple::resolve_triple;
use crate::provision::version::is_dev_version;

/// Timeout for the version-verification command (short – same as probe).
const VERIFY_TIMEOUT_SECS: u64 = 20;

/// Timeout for the full download + remote extraction pipeline.
///
/// The archive download can be large (tens of MB) and the remote `tar`
/// invocation adds time on top; 300 s is generous but bounded.
const INSTALL_TIMEOUT_SECS: u64 = 300;

/// The result of a successful provisioning operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedCodex {
    /// Absolute path to the codex binary on the remote host.
    pub codex_path: String,
    /// Installed version string (e.g. `"1.2.3"`).
    pub version: String,
    /// Optional warning produced when install failed but an existing binary
    /// could be used as a fallback.
    pub warning: Option<String>,
    /// Path to the preferred shell on the remote host, as detected by the
    /// probe script (`CODEX_SHELL:` line).  `None` when the remote probe did
    /// not emit a shell line (very old environments or exotic configurations).
    pub shell: Option<String>,
}

/// The standard installation symlink path relative to `$HOME`.
const CODEX_SYMLINK_RELATIVE: &str = ".codex/bin/codex";

/// Base URL for GitHub releases.
const RELEASES_BASE: &str = "https://github.com/openai/codex/releases/download";

/// Ensures the correct version of codex is available on the remote host,
/// installing it if necessary.
///
/// # Steps
/// 1. Probe the remote to find OS/arch and any existing binary.
/// 2. If the existing binary already satisfies `desired` *without* a network
///    round-trip, reuse it immediately (avoids the GitHub API, which matters
///    under rate limiting).
/// 3. Otherwise resolve the concrete target version (may hit the GitHub API),
///    and reuse again if it now matches.
/// 4. Otherwise download `codex-package-<triple>.tar.gz` and
///    `codex-package_SHA256SUMS` on the **host** into a temporary file,
///    verify the SHA-256, then stream the archive into the remote via the
///    launcher's stdin.
/// 5. Verify the installed binary reports the expected version.
/// 6. On download failure, fall back to the existing binary (if any) with a
///    warning.
pub async fn ensure_remote_codex(
    launcher: &RemoteLauncher,
    desired: &VersionPolicy,
) -> Result<ProvisionedCodex, ProvisionError> {
    let probe_result = probe(launcher).await?;
    let triple = resolve_triple(&probe_result.os, &probe_result.arch)?;
    let remote_shell = probe_result.shell.clone();

    // Fast path: reuse an existing remote binary when the policy can be
    // satisfied offline, so a routine switch to an already-provisioned remote
    // never touches the network.
    if let Some((existing_path, existing_version)) = &probe_result.existing
        && desired.is_satisfied_by_existing(existing_version)
    {
        // When the host is a dev build, the policy accepts any existing remote
        // binary.  Warn about potential protocol incompatibility so the caller
        // can surface this to the user.
        if matches!(desired, VersionPolicy::HostVersion)
            && is_dev_version(env!("CARGO_PKG_VERSION"))
        {
            tracing::warn!(
                existing_version,
                "reusing remote codex {existing_version} from a dev host build; \
                 protocol versions may be incompatible"
            );
        }
        return Ok(ProvisionedCodex {
            codex_path: existing_path.clone(),
            version: existing_version.clone(),
            warning: None,
            shell: remote_shell,
        });
    }

    // A download may be required: resolve the concrete version now.
    let version = desired.resolve().await?;

    // The resolved version may still match what is already installed.
    // Normalize both sides: trim whitespace and strip a leading 'v' so that
    // "v1.2.3" and "1.2.3" are treated as equal (avoids unnecessary re-downloads).
    if let Some((existing_path, existing_version)) = &probe_result.existing
        && normalize_version(existing_version) == normalize_version(&version)
    {
        return Ok(ProvisionedCodex {
            codex_path: existing_path.clone(),
            version: existing_version.clone(),
            warning: None,
            shell: remote_shell,
        });
    }

    let codex_path = format!("{}/{CODEX_SYMLINK_RELATIVE}", probe_result.home);

    // Attempt to download, verify, and stream the archive.
    match install_remote_codex(launcher, &triple, &version, &probe_result.home).await {
        Ok(()) => {}
        Err(install_err) => {
            // Fall back to the existing binary when one is available.
            if let Some((existing_path, existing_version)) = &probe_result.existing {
                return Ok(ProvisionedCodex {
                    codex_path: existing_path.clone(),
                    version: existing_version.clone(),
                    warning: Some(format!(
                        "install failed ({install_err}); using existing codex {existing_version}"
                    )),
                    shell: remote_shell,
                });
            }
            return Err(install_err);
        }
    }

    // Verify the installed binary.
    let installed_version = verify_remote_version(launcher, &codex_path).await?;
    if installed_version != version {
        return Err(ProvisionError::VersionCheckFailed {
            expected: version,
            actual: installed_version,
        });
    }

    Ok(ProvisionedCodex {
        codex_path,
        version,
        warning: None,
        shell: remote_shell,
    })
}

/// Downloads the package archive and SHA256SUMS on the host into a temporary
/// file, verifies the SHA-256 digest, then streams the archive into the remote
/// via stdin.
///
/// Using a temporary file keeps memory usage O(1) (no full-buffer in RAM) and
/// is safer than piping an unverified stream directly into `tar`: the archive
/// is only forwarded to the remote *after* the digest check passes.
///
/// The entire operation (download + remote extraction) is bounded by
/// [`INSTALL_TIMEOUT_SECS`].
async fn install_remote_codex(
    launcher: &RemoteLauncher,
    triple: &str,
    version: &str,
    remote_home: &str,
) -> Result<(), ProvisionError> {
    timeout(
        Duration::from_secs(INSTALL_TIMEOUT_SECS),
        install_remote_codex_inner(launcher, triple, version, remote_home),
    )
    .await
    .map_err(|_| ProvisionError::Timeout {
        secs: INSTALL_TIMEOUT_SECS,
        context: "install (download + remote extraction)".to_string(),
    })?
}

/// Inner (non-timeout-wrapped) implementation of the install step.
///
/// Downloads the tar.gz into a temp file while streaming the bytes through a
/// SHA-256 hasher.  After the download finishes the digest is checked, and only
/// if it matches is the temp file streamed into the remote child process.
async fn install_remote_codex_inner(
    launcher: &RemoteLauncher,
    triple: &str,
    version: &str,
    remote_home: &str,
) -> Result<(), ProvisionError> {
    let asset_name = format!("codex-package-{triple}.tar.gz");
    let archive_url = format!("{RELEASES_BASE}/rust-v{version}/{asset_name}");
    let sums_url = format!("{RELEASES_BASE}/rust-v{version}/codex-package_SHA256SUMS");

    // Use the shared workspace HTTP client so CODEX_CA_CERTIFICATE and
    // proxy settings are inherited automatically.
    let client = build_reqwest_client_with_custom_ca(
        reqwest::Client::builder().user_agent("codex-exec-server"),
    )?;

    // Download SHA256SUMS first (small file; full buffer is fine).
    let sums_response = client.get(&sums_url).send().await?;
    check_github_response_status(&sums_response, triple, version)?;
    let sums_bytes = sums_response.error_for_status()?.bytes().await?;

    // Parse expected digest.
    let expected_hex = parse_sha256sums(&sums_bytes, &asset_name)?;

    // Download the archive into a temporary file while computing the digest.
    // This keeps RAM usage O(chunk size) regardless of archive size.
    // NamedTempFile is deleted automatically when `_tmp` is dropped.
    let _tmp = tempfile::NamedTempFile::new().map_err(ProvisionError::TempFileIo)?;
    let tmp_path = _tmp.path().to_owned();
    let mut tmp_file = File::create(&tmp_path)
        .await
        .map_err(ProvisionError::TempFileIo)?;

    let archive_response = client.get(&archive_url).send().await?;
    check_github_response_status(&archive_response, triple, version)?;
    let mut stream = archive_response.error_for_status()?.bytes_stream();

    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        tmp_file
            .write_all(&chunk)
            .await
            .map_err(ProvisionError::TempFileIo)?;
    }
    tmp_file.flush().await.map_err(ProvisionError::TempFileIo)?;
    drop(tmp_file);

    // Verify digest now that the full archive is written.
    let actual_hex = finalize_sha256_hex(hasher);
    if actual_hex != expected_hex {
        return Err(ProvisionError::DigestMismatch {
            asset: asset_name,
            expected: expected_hex,
            actual: actual_hex,
        });
    }

    // Stream the verified archive to the remote for extraction.
    //
    // The codex-package tar.gz layout (from install.sh) places the binary at
    // `bin/codex` relative to the archive root.  We extract into a versioned
    // directory and create a stable symlink.
    //
    // All values interpolated into the shell script are wrapped with
    // `posix_single_quote` to prevent injection via paths containing shell
    // metacharacters (spaces, quotes, dollar signs, etc.).
    let release_dir = format!("{remote_home}/.codex/bin/releases/{version}");
    let install_sh = format!(
        "mkdir -p {release_dir_q} && \
         tar -xzf - -C {release_dir_q} && \
         chmod 0755 {codex_bin_q} && \
         mkdir -p {bin_dir_q} && \
         ln -sf {codex_bin_q} {symlink_q}",
        release_dir_q = posix_single_quote(&release_dir),
        codex_bin_q = posix_single_quote(&format!("{release_dir}/bin/codex")),
        bin_dir_q = posix_single_quote(&format!("{remote_home}/.codex/bin")),
        symlink_q = posix_single_quote(&format!("{remote_home}/.codex/bin/codex")),
    );

    let argv = launcher.shell_argv(&install_sh);
    let (program, prefix_args) = argv
        .split_first()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("empty launcher argv".to_string()))?;

    let mut cmd = Command::new(program);
    cmd.args(prefix_args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(ProvisionError::LauncherIo)?;

    let mut child_stdin = child.stdin.take().ok_or_else(|| {
        ProvisionError::ProbeOutputParse("install command has no stdin".to_string())
    })?;

    // Stream the verified temp file into the child's stdin chunk by chunk.
    let mut read_file = File::open(&tmp_path)
        .await
        .map_err(ProvisionError::TempFileIo)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = read_file
            .read(&mut buf)
            .await
            .map_err(ProvisionError::TempFileIo)?;
        if n == 0 {
            break;
        }
        child_stdin
            .write_all(&buf[..n])
            .await
            .map_err(ProvisionError::LauncherIo)?;
    }
    drop(child_stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(ProvisionError::LauncherIo)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(ProvisionError::LauncherNonZero {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    Ok(())
}

/// Inspects the HTTP response status before consuming the body and maps
/// rate-limit (403/429) and not-found (404) responses to specific errors.
///
/// Note: this consumes a shared reference; call it before `.error_for_status()`
/// or `.bytes()`.
fn check_github_response_status(
    response: &reqwest::Response,
    triple: &str,
    version: &str,
) -> Result<(), ProvisionError> {
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(ProvisionError::AssetNotFound {
            triple: triple.to_string(),
            version: version.to_string(),
        });
    }
    if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ProvisionError::GitHubRateLimit {
            status: status.as_u16(),
        });
    }
    Ok(())
}

/// Runs `<codex_path> --version` via the launcher and parses the version
/// string from the output.
///
/// The path is wrapped with [`posix_single_quote`] before embedding it in the
/// shell script so paths containing spaces or shell metacharacters cannot cause
/// injection.  A [`VERIFY_TIMEOUT_SECS`]-second timeout prevents a hung
/// remote from blocking indefinitely.
async fn verify_remote_version(
    launcher: &RemoteLauncher,
    codex_path: &str,
) -> Result<String, ProvisionError> {
    let version_cmd = format!("{} --version", posix_single_quote(codex_path));
    let argv = launcher.shell_argv(&version_cmd);
    let (program, prefix_args) = argv
        .split_first()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("empty launcher argv".to_string()))?;

    let mut cmd = Command::new(program);
    cmd.args(prefix_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = timeout(Duration::from_secs(VERIFY_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| ProvisionError::Timeout {
            secs: VERIFY_TIMEOUT_SECS,
            context: format!("verify {codex_path} --version"),
        })?
        .map_err(ProvisionError::LauncherIo)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(ProvisionError::LauncherNonZero {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_version_output(&stdout)
}

/// Parses a version string from `codex --version` output.
///
/// Expects a line of the form `codex <version>` and extracts `<version>`.
pub(crate) fn parse_version_output(output: &str) -> Result<String, ProvisionError> {
    for line in output.lines() {
        // Match the trailing version token: anything after the last space that
        // starts with a digit.
        if let Some(ver) = line
            .split_whitespace()
            .last()
            .filter(|s| s.starts_with(|c: char| c.is_ascii_digit()))
        {
            return Ok(ver.to_string());
        }
    }
    Err(ProvisionError::ProbeOutputParse(format!(
        "could not parse version from: {output}"
    )))
}

/// Parses the expected SHA-256 hex digest for `asset_name` from the contents
/// of a `SHA256SUMS`-style file.
///
/// The file format is:
/// ```text
/// <64-char hex>  <filename>
/// ```
/// (two spaces between hash and name, matching the output of `sha256sum`).
pub(crate) fn parse_sha256sums(sums: &[u8], asset_name: &str) -> Result<String, ProvisionError> {
    let content = std::str::from_utf8(sums)
        .map_err(|e| ProvisionError::ProbeOutputParse(format!("SHA256SUMS is not UTF-8: {e}")))?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format: "<hex>  <name>" or "<hex> <name>"
        let Some((hex, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if hex.len() == 64
            && hex.chars().all(|c| c.is_ascii_hexdigit())
            && name.trim() == asset_name
        {
            return Ok(hex.to_lowercase());
        }
    }

    Err(ProvisionError::AssetNotInSums {
        asset: asset_name.to_string(),
    })
}

/// Finalizes a running SHA-256 hasher and returns the lowercase hex string.
fn finalize_sha256_hex(hasher: Sha256) -> String {
    let hash = hasher.finalize();
    hash.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Normalizes a version string for comparison: trims whitespace and strips a
/// leading `v` character.
///
/// This prevents false "version mismatch" results when one side uses `"v1.2.3"`
/// and the other uses `"1.2.3"`.
pub(crate) fn normalize_version(v: &str) -> &str {
    v.trim().strip_prefix('v').unwrap_or(v.trim())
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
