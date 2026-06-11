use pretty_assertions::assert_eq;

use super::resolve_triple;

#[test]
fn linux_x86_64() {
    assert_eq!(
        resolve_triple("Linux", "x86_64").unwrap(),
        "x86_64-unknown-linux-musl"
    );
}

#[test]
fn linux_aarch64() {
    assert_eq!(
        resolve_triple("Linux", "aarch64").unwrap(),
        "aarch64-unknown-linux-musl"
    );
}

#[test]
fn linux_arm64() {
    assert_eq!(
        resolve_triple("Linux", "arm64").unwrap(),
        "aarch64-unknown-linux-musl"
    );
}

#[test]
fn darwin_x86_64() {
    assert_eq!(
        resolve_triple("Darwin", "x86_64").unwrap(),
        "x86_64-apple-darwin"
    );
}

#[test]
fn darwin_aarch64() {
    assert_eq!(
        resolve_triple("Darwin", "aarch64").unwrap(),
        "aarch64-apple-darwin"
    );
}

#[test]
fn darwin_arm64() {
    assert_eq!(
        resolve_triple("Darwin", "arm64").unwrap(),
        "aarch64-apple-darwin"
    );
}

#[test]
fn unsupported_os() {
    let err = resolve_triple("Windows", "x86_64").unwrap_err();
    assert!(err.to_string().contains("unsupported remote platform"));
}

#[test]
fn unsupported_arch() {
    let err = resolve_triple("Linux", "riscv64").unwrap_err();
    assert!(err.to_string().contains("unsupported remote platform"));
}
