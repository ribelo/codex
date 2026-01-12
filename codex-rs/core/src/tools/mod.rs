pub mod context;
pub mod events;
pub(crate) mod handlers;
pub mod orchestrator;
pub mod parallel;
pub mod registry;
pub mod router;
pub mod runtimes;
pub mod sandboxing;
pub mod spec;

use crate::exec::ExecToolCallOutput;
use crate::truncate::TruncationBias;
use crate::truncate::TruncationPolicy;
use crate::truncate::formatted_truncate_text;
use crate::truncate::truncate_text;
use crate::truncate::truncate_with_file_fallback;
pub use router::ToolRouter;
use serde::Serialize;
use std::path::Path;

// Telemetry preview limits: keep log events smaller than model budgets.
pub(crate) const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024; // 2 KiB
pub(crate) const TELEMETRY_PREVIEW_MAX_LINES: usize = 64; // lines
pub(crate) const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str =
    "[... telemetry preview truncated ...]";

/// Format the combined exec output for sending back to the model.
/// Includes exit code and duration metadata; truncates large bodies safely.
pub fn format_exec_output_for_model_structured(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    truncation_bias: TruncationBias,
    hint: Option<&str>,
    temp_dir: &Path,
    call_id: &str,
) -> String {
    let ExecToolCallOutput {
        exit_code,
        duration,
        ..
    } = exec_output;

    #[derive(Serialize)]
    struct ExecMetadata {
        exit_code: i32,
        duration_seconds: f32,
    }

    #[derive(Serialize)]
    struct ExecOutput<'a> {
        output: &'a str,
        metadata: ExecMetadata,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<&'a str>,
    }

    // round to 1 decimal place
    let duration_seconds = ((duration.as_secs_f32()) * 10.0).round() / 10.0;

    let formatted_output = format_exec_output_str(
        exec_output,
        truncation_policy,
        truncation_bias,
        temp_dir,
        call_id,
    );

    let payload = ExecOutput {
        output: &formatted_output,
        metadata: ExecMetadata {
            exit_code: *exit_code,
            duration_seconds,
        },
        hint,
    };

    #[expect(clippy::expect_used)]
    serde_json::to_string(&payload).expect("serialize ExecOutput")
}

pub fn format_exec_output_for_model_freeform(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    truncation_bias: TruncationBias,
    hint: Option<&str>,
    cwd: &Path,
    call_id: &str,
) -> String {
    // round to 1 decimal place
    let duration_seconds = ((exec_output.duration.as_secs_f32()) * 10.0).round() / 10.0;

    let total_lines = exec_output.aggregated_output.text.lines().count();

    let (formatted_output, is_truncated_and_formatted) = truncate_with_file_fallback(
        &exec_output.aggregated_output.text,
        truncation_policy,
        truncation_bias,
        cwd,
        call_id,
    )
    .map(|r| (r.content, r.saved_file.is_some()))
    .unwrap_or_else(|e| {
        tracing::warn!("Failed to write truncated output to file: {}", e);
        (
            truncate_text(
                &exec_output.aggregated_output.text,
                truncation_policy,
                truncation_bias,
            ),
            false,
        )
    });

    let mut sections = Vec::new();

    sections.push(format!("Exit code: {}", exec_output.exit_code));
    sections.push(format!("Wall time: {duration_seconds} seconds"));
    if is_truncated_and_formatted || total_lines != formatted_output.lines().count() {
        sections.push(format!("Total output lines: {total_lines}"));
    }

    sections.push("Output:".to_string());
    sections.push(formatted_output);
    if let Some(hint) = hint {
        sections.push(format!("Hint: {hint}"));
    }

    sections.join("\n")
}

pub fn format_exec_output_str(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
    truncation_bias: TruncationBias,
    cwd: &Path,
    call_id: &str,
) -> String {
    let ExecToolCallOutput {
        aggregated_output, ..
    } = exec_output;

    let content = aggregated_output.text.as_str();

    let body = if exec_output.timed_out {
        format!(
            "command timed out after {} milliseconds\n{content}",
            exec_output.duration.as_millis()
        )
    } else {
        content.to_string()
    };

    // Truncate for model consumption before serialization.
    truncate_with_file_fallback(&body, truncation_policy, truncation_bias, cwd, call_id)
        .map(|r| {
            if r.saved_file.is_some() {
                let total_lines = body.lines().count();
                format!("Total output lines: {total_lines}\n\n{}", r.content)
            } else {
                r.content
            }
        })
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to write truncated output to file: {}", e);
            formatted_truncate_text(&body, truncation_policy, truncation_bias)
        })
}
