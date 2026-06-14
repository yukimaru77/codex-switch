use super::DOCKER_RUN_ADVISORY;
use super::RAW_REMOTE_ADVISORY;
use super::RemoteCommandAdvisoryOptions;
use super::remote_command_advisory;
use pretty_assertions::assert_eq;

#[test]
fn no_advisory_when_env_switch_is_disabled() {
    assert_eq!(
        remote_command_advisory(
            "ssh example-host hostname",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: false,
                explicit_environment_id: None,
            }
        ),
        None
    );
}

#[test]
fn no_advisory_when_environment_id_is_explicit() {
    assert_eq!(
        remote_command_advisory(
            "ssh example-host hostname",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: Some("ssh:example-host"),
            }
        ),
        None
    );
}

#[test]
fn detects_raw_ssh_command() {
    let advisory = remote_command_advisory(
        "ssh example-host hostname",
        RemoteCommandAdvisoryOptions {
            env_switch_enabled: true,
            explicit_environment_id: None,
        },
    );
    assert_eq!(advisory, Some(RAW_REMOTE_ADVISORY));
    assert!(advisory.expect("advisory").contains("compatible tools"));
}

#[test]
fn detects_absolute_ssh_command() {
    assert_eq!(
        remote_command_advisory(
            "/usr/bin/ssh example-host hostname",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: None,
            }
        ),
        Some(RAW_REMOTE_ADVISORY)
    );
}

#[test]
fn detects_raw_ssh_later_in_plain_command_sequence() {
    assert_eq!(
        remote_command_advisory(
            "printf local && ssh example-host hostname",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: None,
            }
        ),
        Some(RAW_REMOTE_ADVISORY)
    );
}

#[test]
fn detects_docker_exec_command() {
    assert_eq!(
        remote_command_advisory(
            "docker exec example-container hostname",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: None,
            }
        ),
        Some(RAW_REMOTE_ADVISORY)
    );
}

#[test]
fn detects_docker_run_command_with_lifecycle_advisory() {
    assert_eq!(
        remote_command_advisory(
            "docker run --name example-container image:tag",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: None,
            }
        ),
        Some(DOCKER_RUN_ADVISORY)
    );
}

#[test]
fn ignores_other_docker_lifecycle_commands() {
    assert_eq!(
        remote_command_advisory(
            "docker ps --filter name=example",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: None,
            }
        ),
        None
    );
}

#[test]
fn detects_command_after_environment_assignments() {
    assert_eq!(
        remote_command_advisory(
            "DOCKER_HOST=unix:///tmp/docker.sock docker exec c true",
            RemoteCommandAdvisoryOptions {
                env_switch_enabled: true,
                explicit_environment_id: None,
            }
        ),
        Some(RAW_REMOTE_ADVISORY)
    );
}
