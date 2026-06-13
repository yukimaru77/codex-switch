//! Remote provisioning of the codex binary via Docker or SSH.
//!
//! This module provides utilities to detect and install a specific version of
//! the codex binary on a remote host without requiring the remote side to have
//! network access.  The tar archive is fetched on the host side, verified with
//! SHA-256, and then streamed into the remote via the launcher's stdin so the
//! remote only needs `sh`, `tar`, and `gzip`.

mod error;
mod install;
mod launcher;
mod probe;
mod triple;
mod version;

pub use error::ProvisionError;
pub use install::ProvisionedCodex;
pub use install::ensure_remote_codex;
pub use launcher::Hop;
pub use launcher::RemoteLauncher;
pub use launcher::posix_single_quote;
pub use launcher::shell_join;
pub use launcher::validate_hop_value;
pub use probe::RemoteProbe;
pub use probe::probe;
pub use version::VersionPolicy;
