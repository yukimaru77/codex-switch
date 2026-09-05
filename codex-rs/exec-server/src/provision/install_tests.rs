use pretty_assertions::assert_eq;

use super::normalize_version;
use super::parse_sha256sums;
use super::parse_version_output;
use crate::provision::RemoteLauncher;
use crate::provision::launcher::posix_single_quote;

#[test]
fn parse_sha256sums_finds_matching_asset() {
    let sums = b"abc123def456abc123def456abc123def456abc123def456abc123def456abc1  codex-package-x86_64-unknown-linux-musl.tar.gz\n\
                 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  codex-package-aarch64-unknown-linux-musl.tar.gz\n";
    let result = parse_sha256sums(sums, "codex-package-x86_64-unknown-linux-musl.tar.gz").unwrap();
    assert_eq!(
        result,
        "abc123def456abc123def456abc123def456abc123def456abc123def456abc1"
    );
}

#[test]
fn parse_sha256sums_returns_lowercase() {
    let sums = b"ABC123DEF456ABC123DEF456ABC123DEF456ABC123DEF456ABC123DEF456ABC1  asset.tar.gz\n";
    let result = parse_sha256sums(sums, "asset.tar.gz").unwrap();
    assert_eq!(
        result,
        "abc123def456abc123def456abc123def456abc123def456abc123def456abc1"
    );
}

#[test]
fn parse_sha256sums_asset_not_found() {
    let sums =
        b"abc123def456abc123def456abc123def456abc123def456abc123def456abc1  other-asset.tar.gz\n";
    let err = parse_sha256sums(sums, "missing-asset.tar.gz").unwrap_err();
    assert!(err.to_string().contains("missing-asset.tar.gz"));
}

#[test]
fn parse_sha256sums_empty_file() {
    let err = parse_sha256sums(b"", "asset.tar.gz").unwrap_err();
    assert!(err.to_string().contains("asset.tar.gz"));
}

#[test]
fn parse_version_output_standard() {
    let output = "codex 1.2.3\n";
    assert_eq!(parse_version_output(output).unwrap(), "1.2.3");
}

#[test]
fn parse_version_output_prerelease() {
    let output = "codex 1.2.3-beta.1\n";
    assert_eq!(parse_version_output(output).unwrap(), "1.2.3-beta.1");
}

#[test]
fn parse_version_output_empty() {
    let err = parse_version_output("").unwrap_err();
    assert!(err.to_string().contains("could not parse version"));
}

// ---------------------------------------------------------------------------
// Injection-safety tests for install script quoting
// ---------------------------------------------------------------------------

/// Verify that a codex_path containing spaces and shell metacharacters is
/// quoted correctly and does not spill into the surrounding script.
#[test]
fn verify_remote_version_cmd_quotes_path_with_spaces() {
    let path = "/home/user name/.codex-server/env-switch/current/codex";
    let quoted = posix_single_quote(path);
    // The quoted form should wrap the whole path in single quotes.
    assert_eq!(
        quoted,
        "'/home/user name/.codex-server/env-switch/current/codex'"
    );

    // Simulate building the version command the same way verify_remote_version
    // does, and confirm the shell word is safe.
    let version_cmd = format!("{quoted} --version");
    let launcher = RemoteLauncher::docker("c");
    let argv = launcher.shell_argv(&version_cmd);
    // The script arg (last element for Docker) must contain the quoted path.
    let script = argv.last().unwrap();
    assert!(script.contains("'/home/user name/.codex-server/env-switch/current/codex'"));
    assert!(!script.contains("/home/user name/.codex-server/env-switch/current/codex --version"));
}

/// A codex_path with a single-quote character must not break out of quoting.
#[test]
fn verify_remote_version_cmd_escapes_single_quote_in_path() {
    let path = "/home/user's/.codex-server/env-switch/current/codex";
    let quoted = posix_single_quote(path);
    // Must use the '\'' escape sequence, not a raw single-quote.
    assert!(
        quoted.contains(r"'\''"),
        "expected '\\'' escape, got: {quoted}"
    );
    assert!(
        !quoted.contains("user's"),
        "raw quote must not appear in output"
    );
}

// ---------------------------------------------------------------------------
// normalize_version tests
// ---------------------------------------------------------------------------

#[test]
fn normalize_version_strips_leading_v() {
    assert_eq!(normalize_version("v1.2.3"), "1.2.3");
}

#[test]
fn normalize_version_no_leading_v() {
    assert_eq!(normalize_version("1.2.3"), "1.2.3");
}

#[test]
fn normalize_version_trims_whitespace() {
    assert_eq!(normalize_version("  1.2.3  "), "1.2.3");
    assert_eq!(normalize_version("  v1.2.3  "), "1.2.3");
}

#[test]
fn normalize_version_equal_with_and_without_v() {
    assert_eq!(normalize_version("v1.2.3"), normalize_version("1.2.3"));
}

// ---------------------------------------------------------------------------
// Streaming SHA-256 digest test
// ---------------------------------------------------------------------------

/// Verifies that the streaming digest path (chunk-by-chunk update) produces
/// the same result as a one-shot digest over the same bytes.
#[test]
fn streaming_sha256_matches_oneshot() {
    use sha2::Digest;
    use sha2::Sha256;
    use std::fmt::Write;

    let data: Vec<u8> = (0u8..=255).cycle().take(1_000_000).collect();

    // One-shot reference.
    let oneshot = {
        let hash = Sha256::digest(&data);
        hash.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    };

    // Streaming in 64 KiB chunks (mirrors install logic).
    let streaming = {
        let mut hasher = Sha256::new();
        for chunk in data.chunks(64 * 1024) {
            hasher.update(chunk);
        }
        let hash = hasher.finalize();
        hash.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    };

    assert_eq!(oneshot, streaming);
}

/// Verify that `release_dir` and `remote_home` values containing spaces
/// are properly quoted in the install script.
#[test]
fn install_script_quoting_covers_paths_with_spaces() {
    let remote_home = "/home/user name";
    let version = "1.2.3";
    let release_dir = format!("{remote_home}/.codex-server/env-switch/releases/{version}");
    let current_dir = format!("{remote_home}/.codex-server/env-switch/current");
    let symlink_path = format!("{current_dir}/codex");

    let install_sh = format!(
        "mkdir -p {release_dir_q} && \
         tar -xzf - -C {release_dir_q} && \
         chmod 0755 {codex_bin_q} && \
         mkdir -p {current_dir_q} && \
         ln -sf {codex_bin_q} {symlink_q}",
        release_dir_q = posix_single_quote(&release_dir),
        codex_bin_q = posix_single_quote(&format!("{release_dir}/bin/codex")),
        current_dir_q = posix_single_quote(&current_dir),
        symlink_q = posix_single_quote(&symlink_path),
    );

    // The script must not contain any unquoted space in the paths.
    assert!(
        install_sh.contains("'/home/user name/"),
        "path with space must be quoted, script: {install_sh}"
    );
    // The script must not expose a raw unquoted space after the first mkdir -p.
    assert!(
        !install_sh.contains("mkdir -p /home/user"),
        "unquoted path must not appear in script"
    );
}
