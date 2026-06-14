use codex_shell_command::bash::try_parse_shell;
use codex_shell_command::bash::try_parse_word_only_commands_sequence;

const RAW_REMOTE_ADVISORY: &str = "Advisory: this command used raw SSH/Docker to enter another execution target. If you will continue running commands, editing files, or inspecting images there, consider env_switch so compatible tools can use that target as the default; pass environment_id explicitly when clarity or an override is useful.";
const DOCKER_RUN_ADVISORY: &str = "Advisory: docker run is appropriate for creating a container. If you will continue working inside the created container, consider env_switch so compatible tools can use it as the default execution environment; pass environment_id explicitly when clarity or an override is useful.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteCommandKind {
    RawRemote,
    DockerRun,
}

pub(crate) struct RemoteCommandAdvisoryOptions<'a> {
    pub(crate) env_switch_enabled: bool,
    pub(crate) explicit_environment_id: Option<&'a str>,
}

pub(crate) fn remote_command_advisory(
    command: &str,
    options: RemoteCommandAdvisoryOptions<'_>,
) -> Option<&'static str> {
    if !options.env_switch_enabled || options.explicit_environment_id.is_some() {
        return None;
    }

    match detect_remote_command(command)? {
        RemoteCommandKind::RawRemote => Some(RAW_REMOTE_ADVISORY),
        RemoteCommandKind::DockerRun => Some(DOCKER_RUN_ADVISORY),
    }
}

fn detect_remote_command(command: &str) -> Option<RemoteCommandKind> {
    shell_words(command)
        .iter()
        .filter_map(|words| detect_remote_command_words(words))
        .next()
}

fn shell_words(command: &str) -> Vec<Vec<String>> {
    if let Some(tree) = try_parse_shell(command)
        && let Some(commands) = try_parse_word_only_commands_sequence(&tree, command)
    {
        return commands;
    }

    shlex::split(command)
        .map(|tokens| split_words_on_connectors(&tokens))
        .unwrap_or_default()
}

fn split_words_on_connectors(tokens: &[String]) -> Vec<Vec<String>> {
    tokens
        .split(|token| matches!(token.as_str(), "&&" | "||" | ";" | "|"))
        .filter(|words| !words.is_empty())
        .map(<[std::string::String]>::to_vec)
        .collect()
}

fn detect_remote_command_words(words: &[String]) -> Option<RemoteCommandKind> {
    let command_index = command_index(words)?;
    let command_name = command_basename(words[command_index].as_str());

    if command_name == "ssh" {
        return Some(RemoteCommandKind::RawRemote);
    }

    if command_name == "docker" {
        return match words.get(command_index + 1).map(String::as_str) {
            Some("exec") => Some(RemoteCommandKind::RawRemote),
            Some("run") => Some(RemoteCommandKind::DockerRun),
            _ => None,
        };
    }

    None
}

fn command_index(words: &[String]) -> Option<usize> {
    let mut index = 0;
    while words.get(index).is_some_and(|word| is_env_assignment(word)) {
        index += 1;
    }

    Some(index).filter(|index| *index < words.len())
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _value)) = word.split_once('=') else {
        return false;
    };
    let Some(first) = name.chars().next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn command_basename(command: &str) -> &str {
    command
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(command)
}

#[cfg(test)]
#[path = "remote_command_advisory_tests.rs"]
mod tests;
