use codex_protocol::custom_commands::CustomCommand;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;

pub struct CommandLoadError {
    pub path: PathBuf,
    pub message: String,
}

/// Validate that all agent and profile references in commands exist.
pub fn validate_commands(
    commands: &[CustomCommand],
    agent_slugs: &HashSet<String>,
    profile_names: &HashSet<String>,
) -> Vec<CommandLoadError> {
    let mut errors = Vec::new();

    for c in commands {
        if c.agent.is_some() && c.profile.is_some() {
            errors.push(CommandLoadError {
                path: c.path.clone(),
                message: "Cannot specify both 'agent' and 'profile' in frontmatter. Use one or the other.".to_string(),
            });
            continue;
        }

        if let Some(ref agent) = c.agent
            && !agent_slugs.contains(agent)
        {
            errors.push(CommandLoadError {
                path: c.path.clone(),
                message: format!("Agent '{agent}' not found. Available agents: {:?}", {
                    let mut slugs: Vec<_> = agent_slugs.iter().collect();
                    slugs.sort();
                    slugs
                }),
            });
        }

        if let Some(ref profile) = c.profile
            && !profile_names.contains(profile)
        {
            errors.push(CommandLoadError {
                path: c.path.clone(),
                message: format!("Profile '{profile}' not found. Available profiles: {:?}", {
                    let mut names: Vec<_> = profile_names.iter().collect();
                    names.sort();
                    names
                }),
            });
        }
    }

    errors
}

/// Return the default commands directory: `$CODEX_HOME/commands`.
/// If `CODEX_HOME` cannot be resolved, returns `None`.
pub fn default_commands_dir() -> Option<PathBuf> {
    crate::config::find_codex_home()
        .ok()
        .map(|home| home.join("commands"))
}

/// Discover command files in the given directory, returning entries sorted by name.
/// Non-files are ignored. If the directory does not exist or cannot be read, returns empty.
pub async fn discover_commands_in(dir: &Path) -> Vec<CustomCommand> {
    discover_commands_in_excluding(dir, &HashSet::new()).await
}

/// Discover command files in the given directory, excluding any with names in `exclude`.
/// Returns entries sorted by name. Non-files are ignored. Missing/unreadable dir yields empty.
pub async fn discover_commands_in_excluding(
    dir: &Path,
    exclude: &HashSet<String>,
) -> Vec<CustomCommand> {
    let mut out: Vec<CustomCommand> = Vec::new();
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return out,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_file_like = fs::metadata(&path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !is_file_like {
            continue;
        }

        let is_md = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }

        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if exclude.contains(&name) {
            continue;
        }

        let content = match fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (description, argument_hint, agent, profile, body) = parse_frontmatter(&content);
        out.push(CustomCommand {
            name,
            path,
            content: body,
            description,
            argument_hint,
            agent,
            profile,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse optional YAML-like frontmatter at the beginning of `content`.
/// Supported keys:
/// - `description`: short description shown in the slash popup
/// - `argument-hint` or `argument_hint`: brief hint string shown after the description
/// - `agent`: subagent slug to run this command under
/// - `profile`: profile name to run this command under
/// Returns (description, argument_hint, agent, profile, body_without_frontmatter).
fn parse_frontmatter(
    content: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
) {
    let mut segments = content.split_inclusive('\n');
    let Some(first_segment) = segments.next() else {
        return (None, None, None, None, String::new());
    };
    let first_line = first_segment.trim_end_matches(['\r', '\n']);
    if first_line.trim() != "---" {
        return (None, None, None, None, content.to_string());
    }

    let mut desc: Option<String> = None;
    let mut hint: Option<String> = None;
    let mut agent: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut frontmatter_closed = false;
    let mut consumed = first_segment.len();

    for segment in segments {
        let line = segment.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();

        if trimmed == "---" {
            frontmatter_closed = true;
            consumed += segment.len();
            break;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            consumed += segment.len();
            continue;
        }

        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let mut val = v.trim().to_string();
            if val.len() >= 2 {
                let bytes = val.as_bytes();
                let first = bytes[0];
                let last = bytes[bytes.len() - 1];
                if (first == b'\"' && last == b'\"') || (first == b'\'' && last == b'\'') {
                    val = val[1..val.len().saturating_sub(1)].to_string();
                }
            }
            match key.as_str() {
                "description" => desc = Some(val),
                "argument-hint" | "argument_hint" => hint = Some(val),
                "agent" => agent = Some(val),
                "profile" => profile = Some(val),
                _ => {}
            }
        }

        consumed += segment.len();
    }

    if !frontmatter_closed {
        return (None, None, None, None, content.to_string());
    }

    let body = if consumed >= content.len() {
        String::new()
    } else {
        content[consumed..].to_string()
    };
    (desc, hint, agent, profile, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn empty_when_dir_missing() {
        let tmp = tempdir().expect("create TempDir");
        let missing = tmp.path().join("nope");
        let found = discover_commands_in(&missing).await;
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn discovers_and_sorts_files() {
        let tmp = tempdir().expect("create TempDir");
        let dir = tmp.path();
        fs::write(dir.join("b.md"), b"b").unwrap();
        fs::write(dir.join("a.md"), b"a").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();
        let found = discover_commands_in(dir).await;
        let names: Vec<String> = found.into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn parses_frontmatter_and_strips_from_body() {
        let tmp = tempdir().expect("create TempDir");
        let dir = tmp.path();
        let file = dir.join("withmeta.md");
        let text = "---\ndescription: \"Quick review command\"\nargument-hint: \"[file] [priority]\"\nagent: explorer\n---\nActual body with $1 and $ARGUMENTS";
        fs::write(&file, text).unwrap();

        let found = discover_commands_in(dir).await;
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert_eq!(c.name, "withmeta");
        assert_eq!(c.description.as_deref(), Some("Quick review command"));
        assert_eq!(c.argument_hint.as_deref(), Some("[file] [priority]"));
        assert_eq!(c.agent.as_deref(), Some("explorer"));
        assert_eq!(c.profile, None);
        assert_eq!(c.content, "Actual body with $1 and $ARGUMENTS");
    }

    #[test]
    fn validate_commands_rejects_both_agent_and_profile() {
        use std::path::PathBuf;

        let commands = vec![CustomCommand {
            name: "test".to_string(),
            path: PathBuf::from("/commands/test.md"),
            content: "body".to_string(),
            description: None,
            argument_hint: None,
            agent: Some("explorer".to_string()),
            profile: Some("fast".to_string()),
        }];

        let agent_slugs = HashSet::from(["explorer".to_string()]);
        let profile_names = HashSet::from(["fast".to_string()]);
        let errors = validate_commands(&commands, &agent_slugs, &profile_names);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Cannot specify both"));
    }

    #[test]
    fn validate_commands_detects_invalid_profile() {
        use std::path::PathBuf;

        let commands = vec![CustomCommand {
            name: "test".to_string(),
            path: PathBuf::from("/commands/test.md"),
            content: "body".to_string(),
            description: None,
            argument_hint: None,
            agent: None,
            profile: Some("nonexistent".to_string()),
        }];

        let agent_slugs = HashSet::new();
        let profile_names = HashSet::from(["fast".to_string(), "slow".to_string()]);
        let errors = validate_commands(&commands, &agent_slugs, &profile_names);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .message
                .contains("Profile 'nonexistent' not found")
        );
    }
}
