//! Remote launcher abstraction.
//!
//! A [`RemoteLauncher`] describes how to route a command through an ordered
//! sequence of transport hops so it runs inside the innermost environment.
//! Hops are stored outer-to-inner; argv synthesis folds inner-to-outer so each
//! outer hop wraps the already-composed inner tokens.

/// A single transport layer in a multi-hop chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hop {
    /// Run via `docker exec -i <container>`.
    ///
    /// Docker passes argv elements directly to `execve`, so no extra quoting
    /// is needed for this hop itself.  If an SSH hop sits outside this one,
    /// the SSH layer's [`shell_join`] will quote the docker tokens.
    Docker { container: String },
    /// Run via `ssh -T <host>`.
    ///
    /// SSH concatenates all trailing argv elements with spaces and hands them
    /// to the remote login shell for parsing.  Therefore the tokens that follow
    /// are joined with [`shell_join`] into a single, fully-quoted argument.
    Ssh { host: String },
}

impl Hop {
    /// Returns a short string that identifies this hop for use in
    /// [`RemoteLauncher::id`].
    fn id_segment(&self) -> String {
        match self {
            Hop::Docker { container } => format!("docker:{container}"),
            Hop::Ssh { host } => format!("ssh:{host}"),
        }
    }
}

/// Describes how to reach the remote execution environment.
///
/// The `hops` field lists transport layers from **outermost to innermost**.
/// For example, `[Ssh { host: "dgx" }, Docker { container: "c" }]` means: SSH
/// into `dgx`, then run `docker exec` inside that host.
///
/// An empty `hops` list is not a valid `RemoteLauncher`; use `local` execution
/// instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLauncher {
    /// Transport hops, outer-to-inner.
    pub hops: Vec<Hop>,
}

impl RemoteLauncher {
    /// Creates a single-hop Docker launcher.
    pub fn docker(container: impl Into<String>) -> Self {
        Self {
            hops: vec![Hop::Docker {
                container: container.into(),
            }],
        }
    }

    /// Creates a single-hop SSH launcher.
    pub fn ssh(host: impl Into<String>) -> Self {
        Self {
            hops: vec![Hop::Ssh { host: host.into() }],
        }
    }

    /// Creates a multi-hop launcher from an explicit hop list (outer-to-inner).
    pub fn layered(hops: Vec<Hop>) -> Self {
        Self { hops }
    }

    /// Returns a stable identifier for this launcher, suitable for use as an
    /// `environment_id`.
    ///
    /// Examples:
    /// - single-hop Docker: `"docker:my-container"`
    /// - single-hop SSH: `"ssh:user@host"`
    /// - SSH-then-Docker: `"ssh:dgx>docker:c"`
    pub fn id(&self) -> String {
        self.hops
            .iter()
            .map(Hop::id_segment)
            .collect::<Vec<_>>()
            .join(">")
    }

    /// Builds an argv that runs `inner` tokens inside the outermost hop,
    /// composing each layer from inner to outer.
    ///
    /// Use this for the **exec-server run** step where `inner` is the
    /// exec-server command tokens (e.g. `["codex", "exec-server", "--listen",
    /// "stdio", ...]`).
    ///
    /// Folding rules (applied innermost-first):
    /// - `Docker{c}`:  prepend `["docker", "exec", "-i", c]` — no quoting.
    /// - `Ssh{h}`:     wrap current tokens with [`shell_join`] into a single
    ///   argument, then prepend `["ssh", "-T", h]`.
    pub fn exec_argv(&self, inner: Vec<String>) -> Vec<String> {
        fold_hops(&self.hops, inner)
    }

    /// Builds an argv that runs `sh -c <script>` safely inside the target
    /// environment, composing each hop from inner to outer.
    ///
    /// Use this for the **provision / probe** step.
    ///
    /// # Single-hop examples
    /// - **Docker**: `["docker", "exec", "-i", "<container>", "sh", "-c",
    ///   script]` — Docker passes argv to `execve`; no extra quoting needed.
    /// - **SSH**: `["ssh", "-T", "<host>", "sh -c '<quoted_script>'"]` — SSH
    ///   hands all trailing arguments to the remote login shell as-is; the
    ///   script is wrapped in POSIX single-quote escaping.
    ///
    /// # Multi-hop example
    /// `[Ssh{dgx}, Docker{c}]` with `script = "uname -m"` yields:
    /// `["ssh", "-T", "dgx", "'docker' 'exec' '-i' 'c' 'sh' '-c' 'uname -m'"]`
    pub fn shell_argv(&self, script: &str) -> Vec<String> {
        let inner = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        fold_hops(&self.hops, inner)
    }
}

/// Folds `hops` (outer-to-inner) over `inner_tokens` by processing from
/// innermost to outermost, so each outer hop wraps the already-composed result.
fn fold_hops(hops: &[Hop], inner_tokens: Vec<String>) -> Vec<String> {
    hops.iter()
        .rev()
        .fold(inner_tokens, |tokens, hop| match hop {
            Hop::Docker { container } => {
                let mut argv = vec![
                    "docker".to_string(),
                    "exec".to_string(),
                    "-i".to_string(),
                    container.clone(),
                ];
                argv.extend(tokens);
                argv
            }
            Hop::Ssh { host } => {
                vec![
                    "ssh".to_string(),
                    "-T".to_string(),
                    host.clone(),
                    shell_join(&tokens),
                ]
            }
        })
}

/// Joins `tokens` into a single shell word by POSIX-single-quoting each token
/// and separating them with spaces.
///
/// The result, when passed as a single argument to `ssh`, causes the remote
/// login shell to re-parse it into the original token sequence.
pub fn shell_join(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| posix_single_quote(t))
        .collect::<Vec<_>>()
        .join(" ")
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
