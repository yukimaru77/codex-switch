//! Remote environment probe.
//!
//! Sends a single `sh -c` invocation via the launcher to collect OS/arch/HOME
//! and existing codex location + version.

use std::process::Stdio;

use tokio::process::Command;

use crate::provision::ProvisionError;
use crate::provision::RemoteLauncher;

/// Information collected from the remote host in a single probe invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProbe {
    /// Output of `uname -s` (e.g. `"Linux"` or `"Darwin"`).
    pub os: String,
    /// Output of `uname -m` (e.g. `"x86_64"` or `"aarch64"`).
    pub arch: String,
    /// Value of `$HOME` on the remote.
    pub home: String,
    /// Path and version of an existing codex binary, if found.
    ///
    /// The probe checks both `command -v codex` (PATH-based) and
    /// `~/.codex/bin/codex` (the standard installation symlink).
    pub existing: Option<(String, String)>,
}

/// Shell script that collects all probe data in one round-trip.
///
/// Output format (one field per line):
/// ```text
/// <uname -s>
/// <uname -m>
/// <$HOME>
/// CODEX_PATH:<path>   (optional, omitted if not found)
/// CODEX_VERSION:<ver> (optional, omitted if not found)
/// ```
const PROBE_SCRIPT: &str = r#"
set -e
uname -s
uname -m
echo "$HOME"
_codex_path=""
if command -v codex >/dev/null 2>&1; then
  _codex_path="$(command -v codex)"
elif [ -x "$HOME/.codex/bin/codex" ]; then
  _codex_path="$HOME/.codex/bin/codex"
fi
if [ -n "$_codex_path" ]; then
  echo "CODEX_PATH:$_codex_path"
  _ver="$("$_codex_path" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1)"
  if [ -n "$_ver" ]; then
    echo "CODEX_VERSION:$_ver"
  fi
fi
"#;

/// Runs the probe script on the remote via `launcher` and returns the parsed
/// result.
///
/// This performs exactly one subprocess invocation.
pub async fn probe(launcher: &RemoteLauncher) -> Result<RemoteProbe, ProvisionError> {
    let argv = launcher.shell_argv(PROBE_SCRIPT);
    let (program, prefix_args) = argv
        .split_first()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("empty launcher argv".to_string()))?;

    let mut cmd = Command::new(program);
    cmd.args(prefix_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd.output().await.map_err(ProvisionError::LauncherIo)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(ProvisionError::LauncherNonZero {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_probe_output(&stdout)
}

/// Parses the output of the probe script into a [`RemoteProbe`].
pub(crate) fn parse_probe_output(output: &str) -> Result<RemoteProbe, ProvisionError> {
    let mut lines = output.lines().filter(|l| !l.trim().is_empty());

    let os = lines
        .next()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("missing uname -s line".to_string()))?
        .trim()
        .to_string();

    let arch = lines
        .next()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("missing uname -m line".to_string()))?
        .trim()
        .to_string();

    let home = lines
        .next()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("missing $HOME line".to_string()))?
        .trim()
        .to_string();

    let mut codex_path: Option<String> = None;
    let mut codex_version: Option<String> = None;

    for line in lines {
        if let Some(path) = line.strip_prefix("CODEX_PATH:") {
            codex_path = Some(path.trim().to_string());
        } else if let Some(version) = line.strip_prefix("CODEX_VERSION:") {
            codex_version = Some(version.trim().to_string());
        }
    }

    let existing = match (codex_path, codex_version) {
        (Some(path), Some(version)) => Some((path, version)),
        _ => None,
    };

    Ok(RemoteProbe {
        os,
        arch,
        home,
        existing,
    })
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
