//! Remote launcher abstraction.
//!
//! A [`RemoteLauncher`] describes how to prefix a command so it runs inside a
//! Docker container or via SSH.  All other provisioning code is generic over
//! this enum.

/// Describes how to reach the remote execution environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteLauncher {
    /// Run commands inside a Docker container via `docker exec -i`.
    Docker { container: String },
    /// Run commands on a remote host via `ssh -T` (key authentication required).
    Ssh { host: String },
}

impl RemoteLauncher {
    /// Builds an argv that runs `sh -c <script>` safely inside the target
    /// environment, regardless of transport.
    ///
    /// - **Docker**: `["docker", "exec", "-i", "<container>", "sh", "-c", script]`
    ///   Docker passes argv elements directly to `execve`, so no extra quoting
    ///   is needed.
    /// - **SSH**: `["ssh", "-T", "<host>", "sh -c '<quoted_script>'"]`
    ///   SSH concatenates all trailing arguments with spaces and sends the
    ///   result to the remote login shell for parsing.  The script is wrapped
    ///   in POSIX single-quote escaping so it arrives as a single token.
    pub fn shell_argv(&self, script: &str) -> Vec<String> {
        match self {
            RemoteLauncher::Docker { container } => vec![
                "docker".to_string(),
                "exec".to_string(),
                "-i".to_string(),
                container.clone(),
                "sh".to_string(),
                "-c".to_string(),
                script.to_string(),
            ],
            RemoteLauncher::Ssh { host } => vec![
                "ssh".to_string(),
                "-T".to_string(),
                host.clone(),
                format!("sh -c {}", posix_single_quote(script)),
            ],
        }
    }
}

/// Wraps `s` in POSIX single quotes, escaping any embedded single-quote
/// characters using the `'\''` sequence.
///
/// The result is always a valid POSIX shell word that expands to exactly `s`,
/// regardless of what characters `s` contains.
pub fn posix_single_quote(s: &str) -> String {
    // Surround the whole string with single quotes, replacing every `'` with
    // `'\''` (end the current single-quoted segment, insert a literal `'` as a
    // double-quoted or backslash-escaped fragment, then start a new
    // single-quoted segment).
    let escaped = s.replace('\'', r"'\''");
    format!("'{escaped}'")
}

#[cfg(test)]
#[path = "launcher_tests.rs"]
mod tests;
