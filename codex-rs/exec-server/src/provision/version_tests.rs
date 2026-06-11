use pretty_assertions::assert_eq;

use super::is_dev_version;
use super::parse_latest_tag_name;
use crate::provision::VersionPolicy;

#[test]
fn dev_version_zero() {
    assert!(is_dev_version("0.0.0"));
}

#[test]
fn dev_version_suffix() {
    assert!(is_dev_version("1.2.3-dev"));
    assert!(is_dev_version("0.0.0-dev.1"));
}

#[test]
fn not_dev_version() {
    assert!(!is_dev_version("1.2.3"));
    assert!(!is_dev_version("0.1.0"));
}

#[test]
fn parse_tag_name_standard() {
    let json = r#"{"tag_name":"rust-v1.2.3","name":"Codex 1.2.3"}"#;
    assert_eq!(parse_latest_tag_name(json).unwrap(), "1.2.3");
}

#[test]
fn parse_tag_name_with_prerelease() {
    let json = r#"{"tag_name":"rust-v1.2.3-beta.1","name":"Codex 1.2.3-beta.1"}"#;
    assert_eq!(parse_latest_tag_name(json).unwrap(), "1.2.3-beta.1");
}

#[test]
fn parse_tag_name_missing() {
    let json = r#"{"tag_name":"v1.2.3"}"#;
    assert!(parse_latest_tag_name(json).is_err());
}

#[test]
fn exact_policy_resolves_synchronously() {
    // VersionPolicy::Exact does not hit the network; verify it round-trips.
    let policy = VersionPolicy::Exact("3.0.0".to_string());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let v = rt.block_on(policy.resolve()).expect("resolve");
    assert_eq!(v, "3.0.0");
}
