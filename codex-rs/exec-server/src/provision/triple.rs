//! Target triple resolution from a probe result.

use crate::provision::error::ProvisionError;

/// Resolves the Rust target triple for the remote host based on the OS and
/// architecture strings returned by the probe.
///
/// Supported mappings:
/// - Linux + x86_64        → `x86_64-unknown-linux-musl`
/// - Linux + aarch64/arm64 → `aarch64-unknown-linux-musl`
/// - Darwin + x86_64       → `x86_64-apple-darwin`
/// - Darwin + aarch64/arm64→ `aarch64-apple-darwin`
pub fn resolve_triple(os: &str, arch: &str) -> Result<String, ProvisionError> {
    let os_lower = os.to_lowercase();
    let arch_lower = arch.to_lowercase();

    let canonical_arch = match arch_lower.as_str() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => {
            return Err(ProvisionError::UnsupportedPlatform {
                os: os.to_string(),
                arch: arch.to_string(),
            });
        }
    };

    let triple = match os_lower.as_str() {
        "linux" => format!("{canonical_arch}-unknown-linux-musl"),
        "darwin" => format!("{canonical_arch}-apple-darwin"),
        _ => {
            return Err(ProvisionError::UnsupportedPlatform {
                os: os.to_string(),
                arch: arch.to_string(),
            });
        }
    };

    Ok(triple)
}

#[cfg(test)]
#[path = "triple_tests.rs"]
mod tests;
