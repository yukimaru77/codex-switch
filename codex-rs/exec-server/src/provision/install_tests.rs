use pretty_assertions::assert_eq;

use super::parse_sha256sums;
use super::parse_version_output;

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
