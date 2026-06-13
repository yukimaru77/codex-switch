use pretty_assertions::assert_eq;

use super::parse_probe_output;
use crate::provision::RemoteProbe;

#[test]
fn parse_probe_with_existing_codex() {
    let output =
        "Linux\nx86_64\n/home/user\nCODEX_PATH:/home/user/.codex/bin/codex\nCODEX_VERSION:1.2.3\n";
    assert_eq!(
        parse_probe_output(output).unwrap(),
        RemoteProbe {
            os: "Linux".to_string(),
            arch: "x86_64".to_string(),
            home: "/home/user".to_string(),
            existing: Some((
                "/home/user/.codex/bin/codex".to_string(),
                "1.2.3".to_string(),
            )),
            shell: None,
        }
    );
}

#[test]
fn parse_probe_without_existing_codex() {
    let output = "Linux\naarch64\n/root\n";
    assert_eq!(
        parse_probe_output(output).unwrap(),
        RemoteProbe {
            os: "Linux".to_string(),
            arch: "aarch64".to_string(),
            home: "/root".to_string(),
            existing: None,
            shell: None,
        }
    );
}

#[test]
fn parse_probe_darwin() {
    let output = "Darwin\narm64\n/Users/alice\nCODEX_PATH:/Users/alice/.codex/bin/codex\nCODEX_VERSION:0.9.1\n";
    assert_eq!(
        parse_probe_output(output).unwrap(),
        RemoteProbe {
            os: "Darwin".to_string(),
            arch: "arm64".to_string(),
            home: "/Users/alice".to_string(),
            existing: Some((
                "/Users/alice/.codex/bin/codex".to_string(),
                "0.9.1".to_string(),
            )),
            shell: None,
        }
    );
}

#[test]
fn parse_probe_missing_os() {
    let err = parse_probe_output("").unwrap_err();
    assert!(err.to_string().contains("missing uname -s"));
}

#[test]
fn parse_probe_missing_arch() {
    let err = parse_probe_output("Linux\n").unwrap_err();
    assert!(err.to_string().contains("missing uname -m"));
}

#[test]
fn parse_probe_missing_home() {
    let err = parse_probe_output("Linux\nx86_64\n").unwrap_err();
    assert!(err.to_string().contains("missing $HOME"));
}

#[test]
fn parse_probe_path_without_version_yields_no_existing() {
    // If the version line is missing (e.g. the binary exists but --version
    // fails), we treat it as though no existing codex is present.
    let output = "Linux\nx86_64\n/home/ci\nCODEX_PATH:/usr/local/bin/codex\n";
    let probe = parse_probe_output(output).unwrap();
    assert!(probe.existing.is_none());
}

#[test]
fn parse_probe_with_codex_shell() {
    let output = "Linux\nx86_64\n/home/user\nCODEX_PATH:/home/user/.codex/bin/codex\nCODEX_VERSION:1.2.3\nCODEX_SHELL:/bin/bash\n";
    let probe = parse_probe_output(output).unwrap();
    assert_eq!(probe.shell, Some("/bin/bash".to_string()));
    assert_eq!(probe.os, "Linux");
}

#[test]
fn parse_probe_with_sh_fallback_shell() {
    let output = "Linux\nx86_64\n/root\nCODEX_SHELL:/bin/sh\n";
    let probe = parse_probe_output(output).unwrap();
    assert_eq!(probe.shell, Some("/bin/sh".to_string()));
    assert!(probe.existing.is_none());
}

#[test]
fn parse_probe_without_shell_line() {
    // Older probe output without CODEX_SHELL: shell field is None.
    let output = "Linux\naarch64\n/root\n";
    let probe = parse_probe_output(output).unwrap();
    assert_eq!(probe.shell, None);
}
