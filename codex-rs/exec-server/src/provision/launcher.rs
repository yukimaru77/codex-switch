//! Remote launcher abstraction.
//!
//! A [`RemoteLauncher`] describes how to route a command through an ordered
//! sequence of transport hops so it runs inside the innermost environment.
//! Hops are stored outer-to-inner; argv synthesis folds inner-to-outer so each
//! outer hop wraps the already-composed inner tokens.

/// A single transport layer in a multi-hop chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hop {
    /// Run via `docker exec -i -- <container>`.
    ///
    /// Docker passes argv elements directly to `execve`, so no extra quoting
    /// is needed for this hop itself.  If an SSH hop sits outside this one,
    /// the SSH layer's [`shell_join`] will quote the docker tokens.
    Docker { container: String },
    /// Run via `ssh -T -q -o BatchMode=yes -o StrictHostKeyChecking=accept-new
    /// -o LogLevel=ERROR -- <host>`.
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

/// Validates a hop value (host name or container name) and returns an error if
/// the value is unsafe or would corrupt the id round-trip.
///
/// Rules:
/// - Must not be empty.
/// - Must not start with `-` (would be interpreted as a flag by ssh/docker).
/// - Must not contain `>` (used as the id segment separator in
///   [`RemoteLauncher::id`]; would corrupt round-trip parsing).
///
/// This function is intentionally `pub` so that call sites that accept
/// user-supplied hop values (e.g. `env_switch`'s `hop_from_arg`) can validate
/// values at intake before constructing a [`RemoteLauncher`].
pub fn validate_hop_value(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("hop value must not be empty".to_string());
    }
    if s.starts_with('-') {
        return Err(format!(
            "hop value `{s}` must not start with `-` (would be interpreted as a flag)"
        ));
    }
    if s.contains('>') {
        return Err(format!(
            "hop value `{s}` must not contain `>` (reserved as the id segment separator)"
        ));
    }
    Ok(())
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

    /// Parses a launcher id string (as produced by [`RemoteLauncher::id`]) back
    /// into a [`RemoteLauncher`].
    ///
    /// Format: `<type>:<value>` segments separated by `>`, e.g.
    /// `"ssh:dgx>docker:c"`.  The first `:` is used as the type/value separator
    /// so that the value may itself contain `:` characters (e.g. `user@host`).
    ///
    /// Returns `Err` if the string is empty, any segment is missing a `:`
    /// separator, any segment has an empty value, any segment has an unrecognised
    /// type, or any hop value fails [`validate_hop_value`].
    pub fn from_id(id: &str) -> Result<Self, String> {
        if id.is_empty() {
            return Err("launcher id must not be empty".to_string());
        }
        let hops = id
            .split('>')
            .map(|seg| {
                let colon = seg.find(':').ok_or_else(|| {
                    format!("launcher id segment `{seg}` is missing a `:` separator")
                })?;
                let (kind, value) = (&seg[..colon], &seg[colon + 1..]);
                if value.is_empty() {
                    return Err(format!(
                        "launcher id segment `{seg}` has an empty value after `:`"
                    ));
                }
                // Validate the value even when parsing from an id string — a
                // persisted id containing a dangerous value must not silently
                // produce a broken launcher.
                validate_hop_value(value)?;
                match kind {
                    "ssh" => Ok(Hop::Ssh {
                        host: value.to_string(),
                    }),
                    "docker" => Ok(Hop::Docker {
                        container: value.to_string(),
                    }),
                    other => Err(format!(
                        "unknown hop type `{other}` in launcher id `{id}`; \
                         valid types are `ssh`, `docker`"
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { hops })
    }

    /// Returns a new launcher with `hop` appended to the end of the hop list
    /// (i.e. one layer deeper / more inner).
    pub fn with_appended_hop(&self, hop: Hop) -> Self {
        let mut hops = self.hops.clone();
        hops.push(hop);
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
    /// - `Docker{c}`:  prepend `["docker", "exec", "-i", "--", c]` — no quoting.
    ///   The `--` separator prevents container names starting with `-` from
    ///   being misinterpreted as flags.
    /// - `Ssh{h}`:     wrap current tokens with [`shell_join`] into a single
    ///   argument, then prepend
    ///   `["ssh", "-T", "-q", "-o", "BatchMode=yes", "-o",
    ///    "StrictHostKeyChecking=accept-new", "-o", "LogLevel=ERROR", "--", h]`.
    ///   The SSH hardening options prevent password prompts (BatchMode),
    ///   suppress MOTD/banners that would corrupt JSON-RPC stdout (LogLevel,
    ///   -q), and make the host-key policy explicit (StrictHostKeyChecking).
    ///   The `--` separator prevents host names starting with `-` from being
    ///   misinterpreted as flags.
    pub fn exec_argv(&self, inner: Vec<String>) -> Vec<String> {
        fold_hops(&self.hops, inner)
    }

    /// Builds an argv that runs `sh -c <script>` safely inside the target
    /// environment, composing each hop from inner to outer.
    ///
    /// Use this for the **provision / probe** step.
    ///
    /// # Single-hop examples
    /// - **Docker**: `["docker", "exec", "-i", "--", "<container>", "sh", "-c",
    ///   script]` — Docker passes argv to `execve`; no extra quoting needed.
    /// - **SSH**: `["ssh", "-T", "-q", "-o", "BatchMode=yes", "-o",
    ///   "StrictHostKeyChecking=accept-new", "-o", "LogLevel=ERROR", "--",
    ///   "<host>", "sh -c '<quoted_script>'"]` — SSH hands all trailing
    ///   arguments to the remote login shell as-is; the script is wrapped in
    ///   POSIX single-quote escaping.
    ///
    /// # Multi-hop example
    /// `[Ssh{dgx}, Docker{c}]` with `script = "uname -m"` yields:
    /// `["ssh", "-T", "-q", "-o", "BatchMode=yes", "-o",
    ///  "StrictHostKeyChecking=accept-new", "-o", "LogLevel=ERROR", "--",
    ///  "dgx",
    ///  "'docker' 'exec' '-i' '--' 'c' 'sh' '-c' 'uname -m'"]`
    pub fn shell_argv(&self, script: &str) -> Vec<String> {
        let inner = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        fold_hops(&self.hops, inner)
    }
}

/// SSH hardening flags inserted before `--` in every SSH argv.
///
/// - `-T`: disable pseudo-TTY allocation (we only need a transport channel).
/// - `-q`: quiet mode — suppresses most warnings/diagnostic messages.
/// - `-o BatchMode=yes`: never prompt for passwords; fail fast instead.
/// - `-o StrictHostKeyChecking=accept-new`: accept unknown host keys on first
///   connection but reject changed keys (prevents MITM on already-known hosts).
/// - `-o LogLevel=ERROR`: suppress MOTD, banners, and info messages that would
///   corrupt the JSON-RPC stdout stream.
const SSH_HARDENING_FLAGS: &[&str] = &[
    "-T",
    "-q",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "LogLevel=ERROR",
];

/// Folds `hops` (outer-to-inner) over `inner_tokens` by processing from
/// innermost to outermost, so each outer hop wraps the already-composed result.
fn fold_hops(hops: &[Hop], inner_tokens: Vec<String>) -> Vec<String> {
    hops.iter()
        .rev()
        .fold(inner_tokens, |tokens, hop| match hop {
            Hop::Docker { container } => {
                // "docker exec -i -- <container> ..." — the `--` end-of-options
                // separator ensures a container name that starts with `-` is
                // never misinterpreted as a docker flag.
                let mut argv = vec![
                    "docker".to_string(),
                    "exec".to_string(),
                    "-i".to_string(),
                    "--".to_string(),
                    container.clone(),
                ];
                argv.extend(tokens);
                argv
            }
            Hop::Ssh { host } => {
                // "ssh <hardening> -- <host> <shell-word>" — the `--`
                // end-of-options separator ensures a host name that starts with
                // `-` is never misinterpreted as an SSH flag.
                let mut argv: Vec<String> = std::iter::once("ssh".to_string())
                    .chain(SSH_HARDENING_FLAGS.iter().map(|s| s.to_string()))
                    .chain(["--".to_string(), host.clone()])
                    .collect();
                argv.push(shell_join(&tokens));
                argv
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
///
/// # Why not `shlex::try_quote`?
///
/// `shlex::try_quote` uses a mixed single/double-quote strategy and has
/// special handling for `^` characters that produces output incompatible with
/// the existing tests and with the pure-single-quote contract expected by
/// [`shell_join`].  The self-contained implementation below is simpler,
/// deterministic, and has been verified against all relevant test cases.
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
