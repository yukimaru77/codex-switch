use pretty_assertions::assert_eq;

use super::Hop;
use super::RemoteLauncher;
use super::SSH_HARDENING_FLAGS;
use super::posix_single_quote;
use super::shell_join;
use super::validate_hop_value;

// --- posix_single_quote --------------------------------------------------

#[test]
fn posix_quote_plain_string() {
    assert_eq!(posix_single_quote("hello world"), "'hello world'");
}

#[test]
fn posix_quote_contains_single_quote() {
    // "it's" → 'it'\''s'
    assert_eq!(posix_single_quote("it's"), r"'it'\''s'");
}

#[test]
fn posix_quote_double_quote_and_dollar() {
    // Double-quotes and $ must NOT be interpreted inside single-quoted strings,
    // so the output should still just wrap in single quotes.
    assert_eq!(posix_single_quote(r#"echo "$VAR""#), r#"'echo "$VAR"'"#);
}

#[test]
fn posix_quote_ampersand_and_pipe() {
    assert_eq!(
        posix_single_quote("mkdir -p /a && tar -xzf - -C /a"),
        "'mkdir -p /a && tar -xzf - -C /a'"
    );
}

#[test]
fn posix_quote_empty_string() {
    assert_eq!(posix_single_quote(""), "''");
}

// --- shell_join ----------------------------------------------------------

#[test]
fn shell_join_single_token() {
    let tokens = vec!["uname".to_string()];
    assert_eq!(shell_join(&tokens), "'uname'");
}

#[test]
fn shell_join_multiple_tokens() {
    let tokens = vec![
        "docker".to_string(),
        "exec".to_string(),
        "-i".to_string(),
        "c1".to_string(),
    ];
    assert_eq!(shell_join(&tokens), "'docker' 'exec' '-i' 'c1'");
}

#[test]
fn shell_join_token_with_metachar() {
    let tokens = vec!["sh".to_string(), "-c".to_string(), "echo 'hi'".to_string()];
    assert_eq!(shell_join(&tokens), r"'sh' '-c' 'echo '\''hi'\'''");
}

// --- validate_hop_value --------------------------------------------------

#[test]
fn validate_hop_value_ok_normal_host() {
    assert!(validate_hop_value("user@host.example.com").is_ok());
}

#[test]
fn validate_hop_value_ok_colon_in_value() {
    // Values may contain `:` (e.g. IPv6 addresses or user:host forms).
    assert!(validate_hop_value("user:host").is_ok());
}

#[test]
fn validate_hop_value_err_empty() {
    let err = validate_hop_value("").unwrap_err();
    assert!(err.contains("empty"), "unexpected error: {err}");
}

#[test]
fn validate_hop_value_err_leading_dash() {
    // A leading `-` would be interpreted as an SSH or docker flag.
    let err = validate_hop_value("-oProxyCommand=touch /tmp/pwned").unwrap_err();
    assert!(err.contains("start with `-`"), "unexpected error: {err}");
}

#[test]
fn validate_hop_value_err_gt_separator() {
    // `>` is the id segment separator; allowing it would corrupt round-trip.
    let err = validate_hop_value("host>evil").unwrap_err();
    assert!(err.contains("`>`"), "unexpected error: {err}");
}

#[test]
fn validate_hop_value_err_whitespace() {
    let err = validate_hop_value("host name").unwrap_err();
    assert!(
        err.contains("whitespace or control characters"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_hop_value_err_too_long() {
    let value = "a".repeat(257);
    let err = validate_hop_value(&value).unwrap_err();
    assert!(
        err.contains("exceeds maximum 256"),
        "unexpected error: {err}"
    );
}

// --- id ------------------------------------------------------------------

#[test]
fn id_single_docker() {
    let launcher = RemoteLauncher::docker("my-container");
    assert_eq!(launcher.id(), "docker:my-container");
}

#[test]
fn id_single_ssh() {
    let launcher = RemoteLauncher::ssh("user@host");
    assert_eq!(launcher.id(), "ssh:user@host");
}

#[test]
fn id_ssh_then_docker() {
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    assert_eq!(launcher.id(), "ssh:hostname>docker:c");
}

#[test]
fn id_three_hops() {
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "bastion".to_string(),
        },
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    assert_eq!(launcher.id(), "ssh:bastion>ssh:hostname>docker:c");
}

// --- helper: build the expected SSH prefix for a given host --------------

/// Returns the expected SSH argv prefix including all hardening flags and `--`,
/// but *without* the final shell-word argument.
fn ssh_prefix(host: &str) -> Vec<String> {
    let mut v = vec!["ssh".to_string()];
    v.extend(
        SSH_HARDENING_FLAGS
            .iter()
            .map(std::string::ToString::to_string),
    );
    v.push("--".to_string());
    v.push(host.to_string());
    v
}

// --- shell_argv (single-layer, backward-compat) --------------------------

#[test]
fn docker_shell_argv_passes_script_as_separate_args() {
    let launcher = RemoteLauncher::docker("c1");
    let script = "mkdir -p /tmp/x && echo ok";
    // Docker argv now includes `--` before the container name.
    assert_eq!(
        launcher.shell_argv(script),
        vec!["docker", "exec", "-i", "--", "c1", "sh", "-c", script]
    );
}

#[test]
fn ssh_shell_argv_collapses_to_single_quoted_arg() {
    let launcher = RemoteLauncher::ssh("remote.example");
    let script = "mkdir -p /tmp/x && tar -xzf - -C /tmp/x";
    let argv = launcher.shell_argv(script);

    // Expected length: "ssh" + hardening flags + "--" + host + 1 shell-word.
    // SSH_HARDENING_FLAGS has 8 elements → total = 1 + 8 + 1 + 1 + 1 = 12.
    let prefix = ssh_prefix("remote.example");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());

    // The last element must be the safely-quoted shell command.
    let remote_arg = argv.last().unwrap();
    assert_eq!(
        remote_arg,
        &format!("'sh' '-c' '{script}'"),
        "script contains no single-quotes so wrapping in single-quotes is sufficient"
    );
}

#[test]
fn ssh_shell_argv_quotes_script_with_single_quote() {
    let launcher = RemoteLauncher::ssh("h");
    // Script containing a single-quote must be properly escaped.
    let script = "echo 'hello world'";
    let argv = launcher.shell_argv(script);

    let prefix = ssh_prefix("h");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());

    // Verify the remote arg contains the escaped form.
    let remote_arg = argv.last().unwrap();
    assert!(
        !remote_arg.contains("echo 'hello"),
        "raw single-quote must be escaped, got: {remote_arg}"
    );
    // Expected: 'sh' '-c' 'echo '\''hello world'\'''
    assert_eq!(remote_arg, r"'sh' '-c' 'echo '\''hello world'\'''");
}

// --- exec_argv (single-layer) -------------------------------------------

#[test]
fn docker_exec_argv_no_quoting() {
    let launcher = RemoteLauncher::docker("c1");
    let inner = vec![
        "codex".to_string(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
    ];
    // Docker argv now includes `--` before the container name.
    assert_eq!(
        launcher.exec_argv(inner),
        vec![
            "docker",
            "exec",
            "-i",
            "--",
            "c1",
            "codex",
            "exec-server",
            "--listen",
            "stdio"
        ]
    );
}

#[test]
fn ssh_exec_argv_collapses_to_single_quoted_arg() {
    let launcher = RemoteLauncher::ssh("h");
    let inner = vec![
        "codex".to_string(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
    ];
    let argv = launcher.exec_argv(inner);

    let prefix = ssh_prefix("h");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());
    assert_eq!(
        argv.last().unwrap(),
        "'codex' 'exec-server' '--listen' 'stdio'"
    );
}

// --- multi-hop: ssh→docker ----------------------------------------------

#[test]
fn ssh_docker_exec_argv() {
    // hops = [Ssh{hostname}, Docker{c}], inner = ["codex", "exec-server", "--listen", "stdio"]
    // Expected fold (inner→outer):
    //   Docker step: ["docker","exec","-i","--","c","codex","exec-server","--listen","stdio"]
    //   Ssh step:    ["ssh", <hardening>, "--", "hostname", shell_join(above)]
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let inner = vec![
        "codex".to_string(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
    ];
    let argv = launcher.exec_argv(inner);

    let prefix = ssh_prefix("hostname");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());
    assert_eq!(
        argv.last().unwrap(),
        "'docker' 'exec' '-i' '--' 'c' 'codex' 'exec-server' '--listen' 'stdio'"
    );
}

#[test]
fn ssh_docker_shell_argv() {
    // hops = [Ssh{hostname}, Docker{c}], script = "uname -m"
    // inner = ["sh","-c","uname -m"]
    // Docker step: ["docker","exec","-i","--","c","sh","-c","uname -m"]
    // Ssh step: ["ssh", <hardening>, "--", "hostname", shell_join(docker_tokens)]
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let argv = launcher.shell_argv("uname -m");

    let prefix = ssh_prefix("hostname");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());
    assert_eq!(
        argv.last().unwrap(),
        "'docker' 'exec' '-i' '--' 'c' 'sh' '-c' 'uname -m'"
    );
}

// --- multi-hop: metacharacter neutralization across layers --------------

#[test]
fn ssh_docker_shell_argv_script_with_metacharacters() {
    // A script containing shell metacharacters must be properly escaped so
    // that the innermost sh -c receives the exact script string.
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "h".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    // Script with single-quote, dollar, semicolon.
    let script = "echo 'hi'; echo $USER";
    let argv = launcher.shell_argv(script);

    let prefix = ssh_prefix("h");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());

    // The entire payload for ssh must be a single token.
    // Docker tokens are individually quoted by shell_join at the Ssh layer.
    // The script token itself is quoted by posix_single_quote, so ' → '\''
    let remote_arg = argv.last().unwrap();
    // script = "echo 'hi'; echo $USER"
    // posix_single_quote(script) = "'echo '\''hi'\''; echo $USER'"
    // inner = ["sh", "-c", script]
    // after Docker prepend: ["docker","exec","-i","--","c","sh","-c",script]
    // shell_join of all those:
    //   'docker' 'exec' '-i' '--' 'c' 'sh' '-c' '<quoted_script>'
    let expected_script_quoted = posix_single_quote(script);
    let expected = format!("'docker' 'exec' '-i' '--' 'c' 'sh' '-c' {expected_script_quoted}");
    assert_eq!(remote_arg, &expected);

    // The remote_arg must be a single token (no unquoted space at the top
    // level that would split it into multiple shell words when ssh forwards
    // it to the remote login shell).  Verify that it starts with `'docker'`
    // (the outer-most docker token is quoted by shell_join) and that the
    // whole thing ends with a closing single-quote (matching the script
    // token's posix_single_quote wrapping).
    assert!(
        remote_arg.starts_with("'docker'"),
        "remote arg must start with quoted 'docker' token: {remote_arg}"
    );
    assert!(
        remote_arg.ends_with('\''),
        "remote arg must end with closing single quote of the script token: {remote_arg}"
    );
}

// --- from_id (round-trip) ------------------------------------------------

#[test]
fn from_id_roundtrip_single_docker() {
    let original = RemoteLauncher::docker("my-container");
    let parsed = RemoteLauncher::from_id(&original.id()).expect("parse failed");
    assert_eq!(parsed, original);
}

#[test]
fn from_id_roundtrip_single_ssh() {
    let original = RemoteLauncher::ssh("user@host");
    let parsed = RemoteLauncher::from_id(&original.id()).expect("parse failed");
    assert_eq!(parsed, original);
}

#[test]
fn from_id_roundtrip_ssh_then_docker() {
    let original = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let parsed = RemoteLauncher::from_id(&original.id()).expect("parse failed");
    assert_eq!(parsed, original);
    assert_eq!(parsed.id(), "ssh:hostname>docker:c");
}

#[test]
fn from_id_roundtrip_three_hops() {
    let original = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "bastion".to_string(),
        },
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let parsed = RemoteLauncher::from_id(&original.id()).expect("parse failed");
    assert_eq!(parsed, original);
    assert_eq!(parsed.id(), "ssh:bastion>ssh:hostname>docker:c");
}

#[test]
fn from_id_error_empty_string() {
    let result = RemoteLauncher::from_id("");
    assert!(result.is_err(), "expected error for empty id");
}

#[test]
fn from_id_error_unknown_type() {
    let result = RemoteLauncher::from_id("ftp:somehost");
    assert!(
        result.is_err(),
        "expected error for unknown type, got: {result:?}"
    );
}

#[test]
fn from_id_error_missing_colon() {
    let result = RemoteLauncher::from_id("sshhostname");
    assert!(
        result.is_err(),
        "expected error for missing colon separator, got: {result:?}"
    );
}

#[test]
fn from_id_error_empty_value() {
    let result = RemoteLauncher::from_id("ssh:");
    assert!(
        result.is_err(),
        "expected error for empty value after colon, got: {result:?}"
    );
}

// --- from_id: dangerous / malformed values are rejected ------------------

#[test]
fn from_id_rejects_leading_dash_in_host() {
    // A host value starting with `-` would be a flag-injection vector.
    let result = RemoteLauncher::from_id("ssh:-oProxyCommand=touch /tmp/pwned");
    assert!(
        result.is_err(),
        "expected error for leading-dash host, got: {result:?}"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("start with `-`"),
        "error should mention leading dash: {err}"
    );
}

#[test]
fn from_id_rejects_leading_dash_in_container() {
    // A container value starting with `-` would be a flag-injection vector.
    let result = RemoteLauncher::from_id("docker:-evil-container");
    assert!(
        result.is_err(),
        "expected error for leading-dash container, got: {result:?}"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("start with `-`"),
        "error should mention leading dash: {err}"
    );
}

#[test]
fn from_id_rejects_gt_in_value() {
    // A `>` inside a value would corrupt split('>')-based parsing, making it
    // impossible to safely round-trip.  from_id must reject such strings.
    // Note: `ssh:host>evil` is actually parsed as two segments by split('>'),
    // so the value seen for the first segment is just "host" — which is fine.
    // The dangerous case is a value that itself embeds `>` *after* the colon
    // without another `>` acting as a separator, e.g. the second segment has
    // an empty type.  We test validate_hop_value directly for this.
    let result = validate_hop_value("host>evil");
    assert!(
        result.is_err(),
        "validate_hop_value must reject values containing `>`"
    );
    let err = result.unwrap_err();
    assert!(err.contains("`>`"), "error should mention `>`: {err}");
}

// --- with_appended_hop ---------------------------------------------------

#[test]
fn with_appended_hop_adds_inner_layer() {
    let base = RemoteLauncher::ssh("hostname");
    let extended = base.with_appended_hop(Hop::Docker {
        container: "c".to_string(),
    });
    assert_eq!(extended.id(), "ssh:hostname>docker:c");
    assert_eq!(extended.hops.len(), 2);
}

#[test]
fn with_appended_hop_does_not_mutate_base() {
    let base = RemoteLauncher::ssh("hostname");
    let _extended = base.with_appended_hop(Hop::Docker {
        container: "c".to_string(),
    });
    // base is unchanged
    assert_eq!(base.id(), "ssh:hostname");
    assert_eq!(base.hops.len(), 1);
}

// --- 3-hop: ssh→ssh→docker ---------------------------------------------

#[test]
fn three_hop_ssh_ssh_docker_exec_argv() {
    // hops = [Ssh{bastion}, Ssh{hostname}, Docker{c}]
    // inner = ["codex", "exec-server", "--listen", "stdio"]
    //
    // Fold innermost-first:
    //   1. Docker{c}: ["docker","exec","-i","--","c","codex","exec-server","--listen","stdio"]
    //   2. Ssh{hostname}:  ["ssh", <hardening>, "--", "hostname", shell_join(step1)]
    //      step1_joined = "'docker' 'exec' '-i' '--' 'c' 'codex' 'exec-server' '--listen' 'stdio'"
    //      step2 = ["ssh", <hardening>, "--", "hostname", step1_joined]
    //   3. Ssh{bastion}: ["ssh", <hardening>, "--", "bastion", shell_join(step2)]
    //      shell_join(step2) = "'ssh' <quoted hardening flags> '--' 'hostname' '<posix_single_quote(step1_joined)>'"
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "bastion".to_string(),
        },
        Hop::Ssh {
            host: "hostname".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let inner = vec![
        "codex".to_string(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
    ];
    let argv = launcher.exec_argv(inner);

    let prefix = ssh_prefix("bastion");
    assert_eq!(argv.len(), prefix.len() + 1);
    assert_eq!(&argv[..prefix.len()], prefix.as_slice());

    // Compute expected step by step to avoid hard-coding fragile strings.
    let step1_tokens: Vec<String> = vec![
        "docker".to_string(),
        "exec".to_string(),
        "-i".to_string(),
        "--".to_string(),
        "c".to_string(),
        "codex".to_string(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
    ];
    let step1_joined = shell_join(&step1_tokens);

    // step2 tokens = ssh_prefix("hostname") + [step1_joined]
    let mut step2_tokens = ssh_prefix("hostname");
    step2_tokens.push(step1_joined);
    let expected = shell_join(&step2_tokens);
    assert_eq!(argv.last().unwrap(), &expected);

    // Sanity: the expected string must contain double-layer quoting (the
    // step1_joined string itself gets posix_single_quote'd inside step2).
    assert!(
        argv.last().unwrap().contains("'ssh'"),
        "outer ssh layer must quote inner 'ssh' token: {}",
        argv.last().unwrap()
    );
}
