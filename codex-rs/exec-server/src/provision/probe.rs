//! Remote environment probe.
//!
//! Sends a single `sh -c` invocation via the launcher to collect OS/arch/HOME
//! and existing codex location + version.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::provision::ProvisionError;
use crate::provision::RemoteLauncher;
use crate::provision::paths::MANAGED_CODEX_SYMLINK_RELATIVE;
use crate::provision::paths::managed_codex_path;

/// Timeout for the initial remote probe. This must be bounded because it is
/// the first SSH/Docker subprocess an env_switch call performs.
const PROBE_TIMEOUT_SECS: u64 = 20;

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
    /// The probe checks only the env_switch-managed codex symlink under
    /// `~/.codex-server/env-switch`, so unrelated user installations on PATH
    /// cannot satisfy or interfere with provisioning.
    pub existing: Option<(String, String)>,
    /// Path to the preferred shell on the remote host.
    ///
    /// Determined by: `$SHELL` (if the binary exists), then `bash`, then `sh`.
    /// `None` only if the probe output did not include a `CODEX_SHELL:` line
    /// (e.g. output from an older probe script version).
    pub shell: Option<String>,
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
_codex_path="$HOME/__MANAGED_CODEX_SYMLINK_RELATIVE__"
if [ ! -x "$_codex_path" ]; then
  _codex_path=""
fi
if [ -n "$_codex_path" ]; then
  echo "CODEX_PATH:$_codex_path"
  _ver="$("$_codex_path" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1)"
  if [ -n "$_ver" ]; then
    echo "CODEX_VERSION:$_ver"
  fi
fi
_sh="$( { [ -n "$SHELL" ] && command -v "$SHELL"; } 2>/dev/null || command -v bash 2>/dev/null || command -v sh 2>/dev/null )"
[ -n "$_sh" ] && echo "CODEX_SHELL:$_sh"
"#;

fn probe_script() -> String {
    PROBE_SCRIPT.replace(
        "__MANAGED_CODEX_SYMLINK_RELATIVE__",
        MANAGED_CODEX_SYMLINK_RELATIVE,
    )
}

/// Runs the probe script on the remote via `launcher` and returns the parsed
/// result.
///
/// This performs exactly one subprocess invocation.
pub async fn probe(launcher: &RemoteLauncher) -> Result<RemoteProbe, ProvisionError> {
    let script = probe_script();
    let argv = launcher.shell_argv(&script);
    let (program, prefix_args) = argv
        .split_first()
        .ok_or_else(|| ProvisionError::ProbeOutputParse("empty launcher argv".to_string()))?;

    let mut cmd = Command::new(program);
    cmd.args(prefix_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let output = timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| ProvisionError::Timeout {
            secs: PROBE_TIMEOUT_SECS,
            context: "remote probe".to_string(),
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
    let mut shell: Option<String> = None;

    for line in lines {
        if let Some(path) = line.strip_prefix("CODEX_PATH:") {
            codex_path = Some(path.trim().to_string());
        } else if let Some(version) = line.strip_prefix("CODEX_VERSION:") {
            codex_version = Some(version.trim().to_string());
        } else if let Some(sh) = line.strip_prefix("CODEX_SHELL:") {
            shell = Some(sh.trim().to_string());
        }
    }

    let expected_managed_path = managed_codex_path(&home);
    let existing = match (codex_path, codex_version) {
        (Some(path), Some(version)) if path == expected_managed_path => Some((path, version)),
        _ => None,
    };

    Ok(RemoteProbe {
        os,
        arch,
        home,
        existing,
        shell,
    })
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
