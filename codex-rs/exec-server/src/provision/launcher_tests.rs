use pretty_assertions::assert_eq;

use super::RemoteLauncher;
use super::posix_single_quote;

#[test]
fn docker_argv_prefix() {
    let launcher = RemoteLauncher::Docker {
        container: "my-container".to_string(),
    };
    assert_eq!(
        launcher.argv_prefix(),
        vec!["docker", "exec", "-i", "my-container"]
    );
}

#[test]
fn ssh_argv_prefix() {
    let launcher = RemoteLauncher::Ssh {
        host: "user@host".to_string(),
    };
    assert_eq!(launcher.argv_prefix(), vec!["ssh", "-T", "user@host"]);
}

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

// --- shell_argv ----------------------------------------------------------

#[test]
fn docker_shell_argv_passes_script_as_separate_args() {
    let launcher = RemoteLauncher::Docker {
        container: "c1".to_string(),
    };
    let script = "mkdir -p /tmp/x && echo ok";
    assert_eq!(
        launcher.shell_argv(script),
        vec!["docker", "exec", "-i", "c1", "sh", "-c", script]
    );
}

#[test]
fn ssh_shell_argv_collapses_to_single_quoted_arg() {
    let launcher = RemoteLauncher::Ssh {
        host: "remote.example".to_string(),
    };
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
        &format!("sh -c '{script}'"),
        "script contains no single-quotes so wrapping in single-quotes is sufficient"
    );
}

#[test]
fn ssh_shell_argv_quotes_script_with_single_quote() {
    let launcher = RemoteLauncher::Ssh {
        host: "h".to_string(),
    };
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
    // Expected: sh -c 'echo '\''hello world'\'''
    assert_eq!(remote_arg, r"sh -c 'echo '\''hello world'\'''");
}
