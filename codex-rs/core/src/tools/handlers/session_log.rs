//! Secure session log inspection tool.
//!
//! This tool is bound to a specific session file at creation time and only
//! allows searching/reading that file. The LLM cannot specify a file path,
//! providing hard security isolation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::spec::JsonSchema;

/// Maximum number of lines to return from a single read operation.
const MAX_READ_LINES: i32 = 500;

/// Maximum number of context lines for search results.
const MAX_CONTEXT_LINES: i32 = 10;

/// Maximum output size in bytes.
const MAX_OUTPUT_BYTES: usize = 50_000;

/// A tool handler bound to a specific session log file.
///
/// This handler supports two operations:
/// - `search_session_log`: Search for patterns using ripgrep
/// - `read_session_log`: Read specific line ranges
#[derive(Clone)]
pub struct SessionLogTool {
    log_path: PathBuf,
}

impl SessionLogTool {
    /// Create a new session log tool bound to the specified file.
    pub fn new(path: PathBuf) -> Self {
        Self { log_path: path }
    }

    /// Returns the tool specifications for search and read operations.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![self.search_spec(), self.read_spec()]
    }

    fn search_spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "pattern".to_string(),
            JsonSchema::String {
                description: Some("Search pattern (supports regex)".to_string()),
                enum_values: None,
            },
        );
        properties.insert(
            "context_lines".to_string(),
            JsonSchema::Number {
                description: Some(
                    "Number of context lines before and after each match (default: 2, max: 10)"
                        .to_string(),
                ),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "search_session_log".to_string(),
            description:
                "Search the session log for a pattern. Returns matching lines with context."
                    .to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: Some(vec!["pattern".to_string()]),
                additional_properties: Some(false.into()),
            },
        })
    }

    fn read_spec(&self) -> ToolSpec {
        let mut properties = BTreeMap::new();
        properties.insert(
            "start_line".to_string(),
            JsonSchema::Number {
                description: Some(
                    "Line number to start reading from (1-indexed, default: 1)".to_string(),
                ),
            },
        );
        properties.insert(
            "num_lines".to_string(),
            JsonSchema::Number {
                description: Some(format!(
                    "Number of lines to read (default: 100, max: {MAX_READ_LINES})"
                )),
            },
        );

        ToolSpec::Function(ResponsesApiTool {
            name: "read_session_log".to_string(),
            description: "Read lines from the session log file.".to_string(),
            strict: false,
            parameters: JsonSchema::Object {
                properties,
                required: None,
                additional_properties: Some(false.into()),
            },
        })
    }

    async fn handle_search(&self, args: SearchArgs) -> Result<String, FunctionCallError> {
        let context = args
            .context_lines
            .unwrap_or(2)
            .min(MAX_CONTEXT_LINES)
            .max(0);

        // Use ripgrep with -e to prevent flag injection from pattern
        let output = Command::new("rg")
            .arg("--line-number")
            .arg("--context")
            .arg(context.to_string())
            .arg("-e") // Treat next arg as pattern (prevents flag injection)
            .arg(&args.pattern)
            .arg(&self.log_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| FunctionCallError::RespondToModel(format!("Failed to run search: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && stdout.is_empty() {
            if stderr.contains("No such file") {
                return Err(FunctionCallError::RespondToModel(
                    "Session log file not found".to_string(),
                ));
            }
            // rg returns exit code 1 when no matches found
            return Ok("No matches found".to_string());
        }

        let result = Self::truncate_output(&stdout);
        Ok(result)
    }

    async fn handle_read(&self, args: ReadArgs) -> Result<String, FunctionCallError> {
        let start = args.start_line.unwrap_or(1).max(1);
        let count = args.num_lines.unwrap_or(100).min(MAX_READ_LINES).max(1);

        // Read file and extract lines
        let content = tokio::fs::read_to_string(&self.log_path)
            .await
            .map_err(|e| {
                FunctionCallError::RespondToModel(format!("Failed to read session log: {e}"))
            })?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        if start as usize > total_lines {
            return Ok(format!(
                "Start line {start} exceeds file length ({total_lines} lines)"
            ));
        }

        let start_idx = (start - 1) as usize;
        let end_idx = (start_idx + count as usize).min(total_lines);

        let selected: Vec<String> = lines[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {line}", start_idx + i + 1))
            .collect();

        let result = selected.join("\n");
        let truncated = Self::truncate_output(&result);

        if end_idx < total_lines {
            Ok(format!(
                "{truncated}\n\n[Showing lines {start}-{end_idx} of {total_lines}]"
            ))
        } else {
            Ok(format!(
                "{truncated}\n\n[End of file, {total_lines} total lines]"
            ))
        }
    }

    fn truncate_output(s: &str) -> String {
        if s.len() <= MAX_OUTPUT_BYTES {
            s.to_string()
        } else {
            let truncated = &s[..MAX_OUTPUT_BYTES];
            // Find last newline to avoid cutting mid-line
            if let Some(last_nl) = truncated.rfind('\n') {
                format!(
                    "{}\n\n[Output truncated at {} bytes]",
                    &truncated[..last_nl],
                    MAX_OUTPUT_BYTES
                )
            } else {
                format!("{truncated}\n\n[Output truncated at {MAX_OUTPUT_BYTES} bytes]")
            }
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    pattern: String,
    context_lines: Option<i32>,
}

#[derive(Deserialize)]
struct ReadArgs {
    start_line: Option<i32>,
    num_lines: Option<i32>,
}

#[async_trait]
impl ToolHandler for SessionLogTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "Session log tool received unsupported payload".to_string(),
                ));
            }
        };

        let result = match invocation.tool_name.as_str() {
            "search_session_log" => {
                let args: SearchArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!("Invalid search arguments: {e}"))
                })?;
                self.handle_search(args).await?
            }
            "read_session_log" => {
                let args: ReadArgs = serde_json::from_str(&arguments).map_err(|e| {
                    FunctionCallError::RespondToModel(format!("Invalid read arguments: {e}"))
                })?;
                self.handle_read(args).await?
            }
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "Unknown session log operation: {other}"
                )));
            }
        };

        Ok(ToolOutput::Function {
            content: result,
            content_items: None,
            success: Some(true),
        })
    }
}
