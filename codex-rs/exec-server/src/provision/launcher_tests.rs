use pretty_assertions::assert_eq;

use super::Hop;
use super::RemoteLauncher;
use super::posix_single_quote;
use super::shell_join;

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
            host: "dgx".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    assert_eq!(launcher.id(), "ssh:dgx>docker:c");
}

#[test]
fn id_three_hops() {
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "bastion".to_string(),
        },
        Hop::Ssh {
            host: "dgx".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    assert_eq!(launcher.id(), "ssh:bastion>ssh:dgx>docker:c");
}

// --- shell_argv (single-layer, backward-compat) --------------------------

#[test]
fn docker_shell_argv_passes_script_as_separate_args() {
    let launcher = RemoteLauncher::docker("c1");
    let script = "mkdir -p /tmp/x && echo ok";
    assert_eq!(
        launcher.shell_argv(script),
        vec!["docker", "exec", "-i", "c1", "sh", "-c", script]
    );
}

#[test]
fn ssh_shell_argv_collapses_to_single_quoted_arg() {
    let launcher = RemoteLauncher::ssh("remote.example");
    let script = "mkdir -p /tmp/x && tar -xzf - -C /tmp/x";
    let argv = launcher.shell_argv(script);
    // Must be exactly 4 elements: ssh -T <host> <single-shell-word>
    assert_eq!(argv.len(), 4);
    assert_eq!(&argv[0], "ssh");
    assert_eq!(&argv[1], "-T");
    assert_eq!(&argv[2], "remote.example");
    // The fourth element must start the remote shell command; the script
    // must be safely wrapped so no re-splitting occurs.
    let remote_arg = &argv[3];
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
    assert_eq!(argv.len(), 4);
    // Verify the remote arg contains the escaped form.
    let remote_arg = &argv[3];
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
    assert_eq!(
        launcher.exec_argv(inner),
        vec![
            "docker",
            "exec",
            "-i",
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
    assert_eq!(argv.len(), 4);
    assert_eq!(&argv[0], "ssh");
    assert_eq!(&argv[1], "-T");
    assert_eq!(&argv[2], "h");
    assert_eq!(&argv[3], "'codex' 'exec-server' '--listen' 'stdio'");
}

// --- multi-hop: ssh→docker ----------------------------------------------

#[test]
fn ssh_docker_exec_argv() {
    // hops = [Ssh{dgx}, Docker{c}], inner = ["codex", "exec-server", "--listen", "stdio"]
    // Expected fold (inner→outer):
    //   Docker step: ["docker","exec","-i","c","codex","exec-server","--listen","stdio"]
    //   Ssh step:    ["ssh","-T","dgx", shell_join(above)]
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "dgx".to_string(),
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
    assert_eq!(argv.len(), 4);
    assert_eq!(&argv[0], "ssh");
    assert_eq!(&argv[1], "-T");
    assert_eq!(&argv[2], "dgx");
    assert_eq!(
        &argv[3],
        "'docker' 'exec' '-i' 'c' 'codex' 'exec-server' '--listen' 'stdio'"
    );
}

#[test]
fn ssh_docker_shell_argv() {
    // hops = [Ssh{dgx}, Docker{c}], script = "uname -m"
    // inner = ["sh","-c","uname -m"]
    // Docker step: ["docker","exec","-i","c","sh","-c","uname -m"]
    // Ssh step: ["ssh","-T","dgx", shell_join(docker_tokens)]
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "dgx".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let argv = launcher.shell_argv("uname -m");
    assert_eq!(argv.len(), 4);
    assert_eq!(&argv[0], "ssh");
    assert_eq!(&argv[1], "-T");
    assert_eq!(&argv[2], "dgx");
    assert_eq!(&argv[3], "'docker' 'exec' '-i' 'c' 'sh' '-c' 'uname -m'");
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
    assert_eq!(argv.len(), 4);
    // The entire payload for ssh must be a single token.
    // Docker tokens are individually quoted by shell_join at the Ssh layer.
    // The script token itself is quoted by posix_single_quote, so ' → '\''
    let remote_arg = &argv[3];
    // script = "echo 'hi'; echo $USER"
    // posix_single_quote(script) = "'echo '\''hi'\''; echo $USER'"
    // inner = ["sh", "-c", script]
    // after Docker prepend: ["docker","exec","-i","c","sh","-c",script]
    // shell_join of all those:
    //   'docker' 'exec' '-i' 'c' 'sh' '-c' '<quoted_script>'
    let expected_script_quoted = posix_single_quote(script);
    let expected = format!("'docker' 'exec' '-i' 'c' 'sh' '-c' {expected_script_quoted}");
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
            host: "dgx".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let parsed = RemoteLauncher::from_id(&original.id()).expect("parse failed");
    assert_eq!(parsed, original);
    assert_eq!(parsed.id(), "ssh:dgx>docker:c");
}

#[test]
fn from_id_roundtrip_three_hops() {
    let original = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "bastion".to_string(),
        },
        Hop::Ssh {
            host: "dgx".to_string(),
        },
        Hop::Docker {
            container: "c".to_string(),
        },
    ]);
    let parsed = RemoteLauncher::from_id(&original.id()).expect("parse failed");
    assert_eq!(parsed, original);
    assert_eq!(parsed.id(), "ssh:bastion>ssh:dgx>docker:c");
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
    let result = RemoteLauncher::from_id("sshdgx");
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

// --- with_appended_hop ---------------------------------------------------

#[test]
fn with_appended_hop_adds_inner_layer() {
    let base = RemoteLauncher::ssh("dgx");
    let extended = base.with_appended_hop(Hop::Docker {
        container: "c".to_string(),
    });
    assert_eq!(extended.id(), "ssh:dgx>docker:c");
    assert_eq!(extended.hops.len(), 2);
}

#[test]
fn with_appended_hop_does_not_mutate_base() {
    let base = RemoteLauncher::ssh("dgx");
    let _extended = base.with_appended_hop(Hop::Docker {
        container: "c".to_string(),
    });
    // base is unchanged
    assert_eq!(base.id(), "ssh:dgx");
    assert_eq!(base.hops.len(), 1);
}

// --- 3-hop: ssh→ssh→docker ---------------------------------------------

#[test]
fn three_hop_ssh_ssh_docker_exec_argv() {
    // hops = [Ssh{bastion}, Ssh{dgx}, Docker{c}]
    // inner = ["codex", "exec-server", "--listen", "stdio"]
    //
    // Fold innermost-first:
    //   1. Docker{c}: ["docker","exec","-i","c","codex","exec-server","--listen","stdio"]
    //   2. Ssh{dgx}:  ["ssh","-T","dgx", shell_join(step1)]
    //      step1_joined = "'docker' 'exec' '-i' 'c' 'codex' 'exec-server' '--listen' 'stdio'"
    //      step2 = ["ssh","-T","dgx", step1_joined]
    //   3. Ssh{bastion}: ["ssh","-T","bastion", shell_join(step2)]
    //      step2 as tokens: ["ssh","-T","dgx", step1_joined]
    //      shell_join(step2) = "'ssh' '-T' 'dgx' '<posix_single_quote(step1_joined)>'"
    let launcher = RemoteLauncher::layered(vec![
        Hop::Ssh {
            host: "bastion".to_string(),
        },
        Hop::Ssh {
            host: "dgx".to_string(),
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
    assert_eq!(argv.len(), 4);
    assert_eq!(&argv[0], "ssh");
    assert_eq!(&argv[1], "-T");
    assert_eq!(&argv[2], "bastion");

    // Compute expected step by step to avoid hard-coding fragile strings.
    let step1_tokens: Vec<String> = vec![
        "docker".to_string(),
        "exec".to_string(),
        "-i".to_string(),
        "c".to_string(),
        "codex".to_string(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
    ];
    let step1_joined = shell_join(&step1_tokens);
    let step2_tokens = vec![
        "ssh".to_string(),
        "-T".to_string(),
        "dgx".to_string(),
        step1_joined,
    ];
    let expected = shell_join(&step2_tokens);
    assert_eq!(&argv[3], &expected);

    // Sanity: the expected string must contain double-layer quoting (the
    // step1_joined string itself gets posix_single_quote'd inside step2).
    assert!(
        argv[3].contains("'ssh'"),
        "outer ssh layer must quote inner 'ssh' token: {}",
        argv[3]
    );
}
