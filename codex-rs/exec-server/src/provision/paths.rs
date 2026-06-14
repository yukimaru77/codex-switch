//! Managed remote paths for env_switch provisioned codex binaries.

/// Root directory for codex binaries managed by env_switch on the remote host.
///
/// This is intentionally separate from `~/.codex/bin` so env_switch does not
/// overwrite or shadow a user's normal codex installation.
pub(crate) const MANAGED_CODEX_ROOT_RELATIVE: &str = ".codex-server/env-switch";

/// Stable symlink path to the currently provisioned env_switch codex binary,
/// relative to the remote `$HOME`.
pub(crate) const MANAGED_CODEX_SYMLINK_RELATIVE: &str = ".codex-server/env-switch/current/codex";

/// Absolute path to the stable env_switch codex symlink.
pub(crate) fn managed_codex_path(remote_home: &str) -> String {
    format!("{remote_home}/{MANAGED_CODEX_SYMLINK_RELATIVE}")
}

/// Absolute path to the directory that contains the stable env_switch symlink.
pub(crate) fn managed_current_dir(remote_home: &str) -> String {
    format!("{remote_home}/{MANAGED_CODEX_ROOT_RELATIVE}/current")
}

/// Absolute path to a versioned env_switch codex release directory.
pub(crate) fn managed_release_dir(remote_home: &str, version: &str) -> String {
    format!("{remote_home}/{MANAGED_CODEX_ROOT_RELATIVE}/releases/{version}")
}
