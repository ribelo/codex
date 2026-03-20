use codex_git::merge_base_with_head;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedReviewRequest {
    pub target: ReviewTarget,
    pub prompt: String,
    pub user_facing_hint: String,
}

const UNCOMMITTED_PROMPT: &str = "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings.";

const BASE_BRANCH_PROMPT_BACKUP: &str = "Review the code changes against the base branch '{branch}'. Start by finding the merge diff between the current branch and {branch}'s upstream e.g. (`git merge-base HEAD \"$(git rev-parse --abbrev-ref \"{branch}@{upstream}\")\"`), then run `git diff` against that SHA to see what changes we would merge into the {branch} branch. Provide prioritized, actionable findings.";
const BASE_BRANCH_PROMPT: &str = "Review the code changes against the base branch '{baseBranch}'. The merge base commit for this comparison is {mergeBaseSha}. Run `git diff {mergeBaseSha}` to inspect the changes relative to {baseBranch}. Provide prioritized, actionable findings.";

const COMMIT_PROMPT_WITH_TITLE: &str = "Review the code changes introduced by commit {sha} (\"{title}\"). Provide prioritized, actionable findings.";
const COMMIT_PROMPT: &str =
    "Review the code changes introduced by commit {sha}. Provide prioritized, actionable findings.";

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReviewRequestArgs {
    UncommittedChanges,
    BaseBranch { branch: String },
    Commit { sha: String, title: Option<String> },
    Custom { instructions: String },
}

impl From<ReviewRequestArgs> for ReviewTarget {
    fn from(args: ReviewRequestArgs) -> Self {
        match args {
            ReviewRequestArgs::UncommittedChanges => ReviewTarget::UncommittedChanges,
            ReviewRequestArgs::BaseBranch { branch } => ReviewTarget::BaseBranch { branch },
            ReviewRequestArgs::Commit { sha, title } => ReviewTarget::Commit { sha, title },
            ReviewRequestArgs::Custom { instructions } => ReviewTarget::Custom { instructions },
        }
    }
}

pub fn resolve_review_request(
    request: ReviewRequest,
    cwd: &Path,
) -> anyhow::Result<ResolvedReviewRequest> {
    let target = normalize_review_target(request.target)?;
    let prompt = review_prompt(&target, cwd)?;
    let user_facing_hint = request
        .user_facing_hint
        .unwrap_or_else(|| user_facing_hint(&target));

    Ok(ResolvedReviewRequest {
        target,
        prompt,
        user_facing_hint,
    })
}

pub fn resolve_review_request_from_message(
    message: &str,
    cwd: &Path,
) -> anyhow::Result<ResolvedReviewRequest> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        anyhow::bail!("instructions must not be empty");
    }

    let request = match serde_json::from_str::<ReviewRequestArgs>(trimmed) {
        Ok(args) => ReviewRequest {
            target: ReviewTarget::from(args),
            user_facing_hint: None,
        },
        Err(_) => ReviewRequest {
            target: ReviewTarget::Custom {
                instructions: trimmed.to_string(),
            },
            user_facing_hint: None,
        },
    };

    resolve_review_request(request, cwd)
}

fn normalize_review_target(target: ReviewTarget) -> anyhow::Result<ReviewTarget> {
    match target {
        ReviewTarget::UncommittedChanges => Ok(ReviewTarget::UncommittedChanges),
        ReviewTarget::BaseBranch { branch } => {
            let branch = branch.trim().to_string();
            if branch.is_empty() {
                anyhow::bail!("branch must not be empty");
            }
            Ok(ReviewTarget::BaseBranch { branch })
        }
        ReviewTarget::Commit { sha, title } => {
            let sha = sha.trim().to_string();
            if sha.is_empty() {
                anyhow::bail!("sha must not be empty");
            }
            let title = title
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            Ok(ReviewTarget::Commit { sha, title })
        }
        ReviewTarget::Custom { instructions } => {
            let instructions = instructions.trim().to_string();
            if instructions.is_empty() {
                anyhow::bail!("instructions must not be empty");
            }
            Ok(ReviewTarget::Custom { instructions })
        }
    }
}

pub fn review_prompt(target: &ReviewTarget, cwd: &Path) -> anyhow::Result<String> {
    match target {
        ReviewTarget::UncommittedChanges => Ok(UNCOMMITTED_PROMPT.to_string()),
        ReviewTarget::BaseBranch { branch } => {
            if let Some(commit) = merge_base_with_head(cwd, branch)? {
                Ok(BASE_BRANCH_PROMPT
                    .replace("{baseBranch}", branch)
                    .replace("{mergeBaseSha}", &commit))
            } else {
                Ok(BASE_BRANCH_PROMPT_BACKUP.replace("{branch}", branch))
            }
        }
        ReviewTarget::Commit { sha, title } => {
            if let Some(title) = title {
                Ok(COMMIT_PROMPT_WITH_TITLE
                    .replace("{sha}", sha)
                    .replace("{title}", title))
            } else {
                Ok(COMMIT_PROMPT.replace("{sha}", sha))
            }
        }
        ReviewTarget::Custom { instructions } => Ok(instructions.to_string()),
    }
}

pub fn user_facing_hint(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::UncommittedChanges => "current changes".to_string(),
        ReviewTarget::BaseBranch { branch } => format!("changes against '{branch}'"),
        ReviewTarget::Commit { sha, title } => {
            let short_sha: String = sha.chars().take(7).collect();
            if let Some(title) = title {
                format!("commit {short_sha}: {title}")
            } else {
                format!("commit {short_sha}")
            }
        }
        ReviewTarget::Custom { instructions } => instructions.trim().to_string(),
    }
}

pub fn tool_invocation_summary(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::UncommittedChanges => "review(current changes)".to_string(),
        ReviewTarget::BaseBranch { branch } => format!("review(base branch '{branch}')"),
        ReviewTarget::Commit { sha, title } => {
            let short_sha: String = sha.chars().take(7).collect();
            if let Some(title) = title {
                format!("review(commit {short_sha}: {title})")
            } else {
                format!("review(commit {short_sha})")
            }
        }
        ReviewTarget::Custom { .. } => "review(custom instructions)".to_string(),
    }
}

pub fn tool_invocation_summary_from_tool_arguments(
    arguments: &str,
) -> Result<String, serde_json::Error> {
    let args: ReviewRequestArgs = serde_json::from_str(arguments)?;
    Ok(tool_invocation_summary(&ReviewTarget::from(args)))
}

impl From<ResolvedReviewRequest> for ReviewRequest {
    fn from(resolved: ResolvedReviewRequest) -> Self {
        ReviewRequest {
            target: resolved.target,
            user_facing_hint: Some(resolved.user_facing_hint),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn resolve_review_request_trims_commit_fields() {
        let resolved = resolve_review_request(
            ReviewRequest {
                target: ReviewTarget::Commit {
                    sha: "  abc123  ".to_string(),
                    title: Some("  tighten tests  ".to_string()),
                },
                user_facing_hint: None,
            },
            Path::new("."),
        )
        .expect("resolve review request");

        assert_eq!(
            resolved.target,
            ReviewTarget::Commit {
                sha: "abc123".to_string(),
                title: Some("tighten tests".to_string()),
            }
        );
        assert_eq!(resolved.user_facing_hint, "commit abc123: tighten tests");
    }

    #[test]
    fn resolve_review_request_rejects_empty_base_branch() {
        let err = resolve_review_request(
            ReviewRequest {
                target: ReviewTarget::BaseBranch {
                    branch: "   ".to_string(),
                },
                user_facing_hint: None,
            },
            Path::new("."),
        )
        .expect_err("empty branch should fail");

        assert_eq!(err.to_string(), "branch must not be empty");
    }

    #[test]
    fn resolve_review_request_rejects_empty_commit_sha() {
        let err = resolve_review_request(
            ReviewRequest {
                target: ReviewTarget::Commit {
                    sha: "\n\t".to_string(),
                    title: Some("title".to_string()),
                },
                user_facing_hint: None,
            },
            Path::new("."),
        )
        .expect_err("empty sha should fail");

        assert_eq!(err.to_string(), "sha must not be empty");
    }

    #[test]
    fn resolve_review_request_rejects_empty_custom_instructions() {
        let err = resolve_review_request(
            ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "  ".to_string(),
                },
                user_facing_hint: None,
            },
            Path::new("."),
        )
        .expect_err("empty instructions should fail");

        assert_eq!(err.to_string(), "instructions must not be empty");
    }

    #[test]
    fn review_tool_arguments_commit_summary_matches_tool_invocation_summary() {
        let summary = tool_invocation_summary_from_tool_arguments(
            r#"{"type":"commit","sha":"abc123def","title":"Tighten tests"}"#,
        )
        .expect("parse review tool arguments");

        assert_eq!(summary, "review(commit abc123d: Tighten tests)");
    }

    #[test]
    fn review_tool_arguments_custom_summary_uses_compact_label() {
        let summary = tool_invocation_summary_from_tool_arguments(
            r#"{"type":"custom","instructions":"Just say hi back"}"#,
        )
        .expect("parse review tool arguments");

        assert_eq!(summary, "review(custom instructions)");
    }

    #[test]
    fn resolve_review_request_from_message_uses_structured_json_targets() {
        let resolved = resolve_review_request_from_message(
            r#"{"type":"commit","sha":"abc123def","title":"Tighten tests"}"#,
            Path::new("."),
        )
        .expect("resolve structured review request");

        assert_eq!(
            resolved.target,
            ReviewTarget::Commit {
                sha: "abc123def".to_string(),
                title: Some("Tighten tests".to_string()),
            }
        );
        assert_eq!(resolved.user_facing_hint, "commit abc123d: Tighten tests");
        assert_eq!(
            resolved.prompt,
            "Review the code changes introduced by commit abc123def (\"Tighten tests\"). Provide prioritized, actionable findings."
        );
    }

    #[test]
    fn resolve_review_request_from_message_falls_back_to_custom_instructions() {
        let resolved = resolve_review_request_from_message(
            "Review the current patch carefully.",
            Path::new("."),
        )
        .expect("resolve plain-text review request");

        assert_eq!(
            resolved.target,
            ReviewTarget::Custom {
                instructions: "Review the current patch carefully.".to_string(),
            }
        );
        assert_eq!(
            resolved.user_facing_hint,
            "Review the current patch carefully."
        );
        assert_eq!(resolved.prompt, "Review the current patch carefully.");
    }
}
