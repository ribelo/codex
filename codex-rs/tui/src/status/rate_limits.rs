use crate::chatwidget::get_limits_duration;

use super::account::StatusAccountDisplay;
use super::helpers::format_reset_timestamp;
use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use chrono::Utc;
use codex_core::protocol::AntigravityQuota;
use codex_core::protocol::CreditsSnapshot as CoreCreditsSnapshot;
use codex_core::protocol::GeminiBucket;
use codex_core::protocol::GeminiQuota;
use codex_core::protocol::ModelQuota;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::RateLimitWindow;

const STATUS_LIMIT_BAR_SEGMENTS: usize = 20;
const STATUS_LIMIT_BAR_FILLED: &str = "█";
const STATUS_LIMIT_BAR_EMPTY: &str = "░";

#[derive(Debug, Clone)]
pub(crate) struct StatusRateLimitRow {
    pub label: String,
    pub value: StatusRateLimitValue,
}

#[derive(Debug, Clone)]
pub(crate) enum StatusRateLimitValue {
    Window {
        percent_used: f64,
        resets_at: Option<String>,
    },
    Text(String),
    /// Section header for provider grouping
    SectionHeader,
}

#[derive(Debug, Clone)]
pub(crate) enum StatusRateLimitData {
    Available(Vec<StatusRateLimitRow>),
    Stale(Vec<StatusRateLimitRow>),
    Missing,
}

pub(crate) const RATE_LIMIT_STALE_THRESHOLD_MINUTES: i64 = 15;

#[derive(Debug, Clone)]
pub(crate) struct RateLimitWindowDisplay {
    pub used_percent: f64,
    pub resets_at: Option<String>,
    pub window_minutes: Option<i64>,
}

impl RateLimitWindowDisplay {
    fn from_window(window: &RateLimitWindow, captured_at: DateTime<Local>) -> Self {
        let resets_at = window
            .resets_at
            .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
            .map(|dt| dt.with_timezone(&Local))
            .map(|dt| format_reset_timestamp(dt, captured_at));

        Self {
            used_percent: window.used_percent,
            resets_at,
            window_minutes: window.window_minutes,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RateLimitSnapshotDisplay {
    pub captured_at: DateTime<Local>,
    pub primary: Option<RateLimitWindowDisplay>,
    pub secondary: Option<RateLimitWindowDisplay>,
    pub credits: Option<CreditsSnapshotDisplay>,
    pub antigravity: Option<AntigravityQuotaDisplay>,
    pub gemini: Option<GeminiQuotaDisplay>,
}

#[derive(Debug, Clone)]
pub(crate) struct CreditsSnapshotDisplay {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

/// Display-ready Antigravity quota data.
#[derive(Debug, Clone)]
pub(crate) struct AntigravityQuotaDisplay {
    pub plan_name: Option<String>,
    pub user_tier: Option<String>,
    pub prompt_credits: CreditsDisplay,
    pub flow_credits: CreditsDisplay,
    pub model_quotas: Vec<ModelQuotaDisplay>,
}

/// Display-ready credits with available/total.
#[derive(Debug, Clone)]
pub(crate) struct CreditsDisplay {
    pub available: i64,
    pub monthly: i64,
}

/// Display-ready per-model quota.
#[derive(Debug, Clone)]
pub(crate) struct ModelQuotaDisplay {
    pub label: String,
    pub remaining_percent: f64,
    pub resets_at: Option<String>,
}

/// Display-ready Gemini quota data.
#[derive(Debug, Clone)]
pub(crate) struct GeminiQuotaDisplay {
    pub buckets: Vec<GeminiBucketDisplay>,
}

/// Display-ready per-bucket Gemini quota.
#[derive(Debug, Clone)]
pub(crate) struct GeminiBucketDisplay {
    pub label: String,
    pub remaining_percent: f64,
    pub resets_at: Option<String>,
}

pub(crate) fn rate_limit_snapshot_display(
    snapshot: &RateLimitSnapshot,
    captured_at: DateTime<Local>,
) -> RateLimitSnapshotDisplay {
    let antigravity = snapshot
        .antigravity
        .as_ref()
        .map(|ag| antigravity_quota_display(ag, captured_at));

    let gemini = snapshot
        .gemini
        .as_ref()
        .map(|g| gemini_quota_display(g, captured_at));

    RateLimitSnapshotDisplay {
        captured_at,
        primary: snapshot
            .primary
            .as_ref()
            .map(|window| RateLimitWindowDisplay::from_window(window, captured_at)),
        secondary: snapshot
            .secondary
            .as_ref()
            .map(|window| RateLimitWindowDisplay::from_window(window, captured_at)),
        credits: snapshot.credits.as_ref().map(CreditsSnapshotDisplay::from),
        antigravity,
        gemini,
    }
}

impl From<&CoreCreditsSnapshot> for CreditsSnapshotDisplay {
    fn from(value: &CoreCreditsSnapshot) -> Self {
        Self {
            has_credits: value.has_credits,
            unlimited: value.unlimited,
            balance: value.balance.clone(),
        }
    }
}

fn antigravity_quota_display(
    quota: &AntigravityQuota,
    captured_at: DateTime<Local>,
) -> AntigravityQuotaDisplay {
    let model_quotas = quota
        .model_quotas
        .iter()
        .map(|mq| model_quota_display(mq, captured_at))
        .collect();

    AntigravityQuotaDisplay {
        plan_name: quota.plan_name.clone(),
        user_tier: quota.user_tier.clone(),
        prompt_credits: CreditsDisplay {
            available: quota.available_prompt_credits,
            monthly: quota.monthly_prompt_credits,
        },
        flow_credits: CreditsDisplay {
            available: quota.available_flow_credits,
            monthly: quota.monthly_flow_credits,
        },
        model_quotas,
    }
}

fn model_quota_display(quota: &ModelQuota, captured_at: DateTime<Local>) -> ModelQuotaDisplay {
    let resets_at = quota
        .resets_at
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|dt| dt.with_timezone(&Local))
        .map(|dt| format_reset_timestamp(dt, captured_at));

    ModelQuotaDisplay {
        label: quota.label.clone(),
        remaining_percent: quota.remaining_fraction * 100.0,
        resets_at,
    }
}

fn gemini_quota_display(quota: &GeminiQuota, captured_at: DateTime<Local>) -> GeminiQuotaDisplay {
    let buckets = quota
        .buckets
        .iter()
        .map(|b| gemini_bucket_display(b, captured_at))
        .collect();

    GeminiQuotaDisplay { buckets }
}

fn gemini_bucket_display(
    bucket: &GeminiBucket,
    captured_at: DateTime<Local>,
) -> GeminiBucketDisplay {
    let resets_at = bucket
        .reset_time
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|dt| dt.with_timezone(&Local))
        .map(|dt| format_reset_timestamp(dt, captured_at));

    let label = match (&bucket.model_id, &bucket.token_type) {
        (Some(model), Some(token)) => format!("{model} ({token})"),
        (Some(model), None) => model.clone(),
        (None, Some(token)) => token.clone(),
        (None, None) => "Gemini".to_string(),
    };

    GeminiBucketDisplay {
        label,
        remaining_percent: bucket.remaining_fraction.unwrap_or(0.0) * 100.0,
        resets_at,
    }
}

pub(crate) fn compose_rate_limit_data(
    snapshot: Option<&RateLimitSnapshotDisplay>,
    account: &StatusAccountDisplay,
    now: DateTime<Local>,
) -> StatusRateLimitData {
    let mut rows = Vec::with_capacity(20);

    // --- OpenAI Section ---
    // Show if we have OpenAI API key OR ChatGPT login OR there are OpenAI rate limits
    let has_openai_auth = account.openai_api_key_set || account.chatgpt.is_some();
    let has_openai_limits = snapshot
        .is_some_and(|s| s.primary.is_some() || s.secondary.is_some() || s.credits.is_some());

    if has_openai_auth || has_openai_limits {
        rows.push(StatusRateLimitRow {
            label: "OpenAI".to_string(),
            value: StatusRateLimitValue::SectionHeader,
        });

        // Auth info
        if let Some(chatgpt) = &account.chatgpt {
            let value = match (&chatgpt.email, &chatgpt.plan) {
                (Some(email), Some(plan)) => format!("{email} ({plan})"),
                (Some(email), None) => email.clone(),
                (None, Some(plan)) => plan.clone(),
                (None, None) => "logged in".to_string(),
            };
            rows.push(StatusRateLimitRow {
                label: "ChatGPT".to_string(),
                value: StatusRateLimitValue::Text(value),
            });
        }
        if account.openai_api_key_set {
            rows.push(StatusRateLimitRow {
                label: "API Key".to_string(),
                value: StatusRateLimitValue::Text("OPENAI_API_KEY".to_string()),
            });
        }

        // Rate limits
        if let Some(snapshot) = snapshot {
            if let Some(primary) = snapshot.primary.as_ref() {
                let label: String = primary
                    .window_minutes
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "5h".to_string());
                let label = capitalize_first(&label);
                rows.push(StatusRateLimitRow {
                    label: format!("{label} limit"),
                    value: StatusRateLimitValue::Window {
                        percent_used: primary.used_percent,
                        resets_at: primary.resets_at.clone(),
                    },
                });
            }

            if let Some(secondary) = snapshot.secondary.as_ref() {
                let label: String = secondary
                    .window_minutes
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "weekly".to_string());
                let label = capitalize_first(&label);
                rows.push(StatusRateLimitRow {
                    label: format!("{label} limit"),
                    value: StatusRateLimitValue::Window {
                        percent_used: secondary.used_percent,
                        resets_at: secondary.resets_at.clone(),
                    },
                });
            }

            if let Some(credits) = snapshot.credits.as_ref()
                && let Some(row) = credit_status_row(credits)
            {
                rows.push(row);
            }
        }
    }

    // --- Gemini Section ---
    let has_gemini_auth = !account.gemini_accounts.is_empty() || account.gemini_api_key_set;
    let has_gemini_limits = snapshot.is_some_and(|s| s.gemini.is_some());

    if has_gemini_auth || has_gemini_limits {
        rows.push(StatusRateLimitRow {
            label: "Gemini".to_string(),
            value: StatusRateLimitValue::SectionHeader,
        });

        // Auth info
        if !account.gemini_accounts.is_empty() {
            let emails = account.gemini_accounts.join(", ");
            rows.push(StatusRateLimitRow {
                label: "OAuth".to_string(),
                value: StatusRateLimitValue::Text(emails),
            });
        }
        if account.gemini_api_key_set {
            rows.push(StatusRateLimitRow {
                label: "API Key".to_string(),
                value: StatusRateLimitValue::Text("GEMINI_API_KEY".to_string()),
            });
        }

        // Rate limits
        if let Some(snapshot) = snapshot
            && let Some(gemini) = &snapshot.gemini
        {
            for bucket in &gemini.buckets {
                let used_percent = 100.0 - bucket.remaining_percent;
                rows.push(StatusRateLimitRow {
                    label: bucket.label.clone(),
                    value: StatusRateLimitValue::Window {
                        percent_used: used_percent,
                        resets_at: bucket.resets_at.clone(),
                    },
                });
            }
        }
    }

    // --- Antigravity Section ---
    let has_antigravity_auth = !account.antigravity_accounts.is_empty();
    let has_antigravity_limits = snapshot.is_some_and(|s| s.antigravity.is_some());

    if has_antigravity_auth || has_antigravity_limits {
        rows.push(StatusRateLimitRow {
            label: "Antigravity".to_string(),
            value: StatusRateLimitValue::SectionHeader,
        });

        // Auth info
        if !account.antigravity_accounts.is_empty() {
            let emails = account.antigravity_accounts.join(", ");
            rows.push(StatusRateLimitRow {
                label: "OAuth".to_string(),
                value: StatusRateLimitValue::Text(emails),
            });
        }

        // Rate limits
        if let Some(snapshot) = snapshot
            && let Some(antigravity) = &snapshot.antigravity
        {
            // Plan and tier info
            if let Some(plan_name) = &antigravity.plan_name {
                let tier_info = antigravity
                    .user_tier
                    .as_ref()
                    .map(|t| format!("{plan_name} ({t})"))
                    .unwrap_or_else(|| plan_name.clone());
                rows.push(StatusRateLimitRow {
                    label: "Plan".to_string(),
                    value: StatusRateLimitValue::Text(tier_info),
                });
            }

            // Prompt credits
            if antigravity.prompt_credits.monthly > 0 {
                rows.push(StatusRateLimitRow {
                    label: "Prompt credits".to_string(),
                    value: StatusRateLimitValue::Text(format!(
                        "{} / {} monthly",
                        antigravity.prompt_credits.available, antigravity.prompt_credits.monthly
                    )),
                });
            }

            // Flow credits
            if antigravity.flow_credits.monthly > 0 {
                rows.push(StatusRateLimitRow {
                    label: "Flow credits".to_string(),
                    value: StatusRateLimitValue::Text(format!(
                        "{} / {} monthly",
                        antigravity.flow_credits.available, antigravity.flow_credits.monthly
                    )),
                });
            }

            // Per-model quotas
            for model in &antigravity.model_quotas {
                let used_percent = 100.0 - model.remaining_percent;
                rows.push(StatusRateLimitRow {
                    label: model.label.clone(),
                    value: StatusRateLimitValue::Window {
                        percent_used: used_percent,
                        resets_at: model.resets_at.clone(),
                    },
                });
            }
        }
    }

    // Determine status
    if rows.is_empty() {
        if snapshot.is_none() {
            StatusRateLimitData::Missing
        } else {
            StatusRateLimitData::Available(vec![])
        }
    } else {
        let is_stale = if let Some(s) = snapshot {
            now.signed_duration_since(s.captured_at)
                > ChronoDuration::minutes(RATE_LIMIT_STALE_THRESHOLD_MINUTES)
        } else {
            false
        };

        if is_stale {
            StatusRateLimitData::Stale(rows)
        } else {
            StatusRateLimitData::Available(rows)
        }
    }
}

pub(crate) fn render_status_limit_progress_bar(percent_remaining: f64) -> String {
    let ratio = (percent_remaining / 100.0).clamp(0.0, 1.0);
    let filled = (ratio * STATUS_LIMIT_BAR_SEGMENTS as f64).round() as usize;
    let filled = filled.min(STATUS_LIMIT_BAR_SEGMENTS);
    let empty = STATUS_LIMIT_BAR_SEGMENTS.saturating_sub(filled);
    format!(
        "[{}{}]",
        STATUS_LIMIT_BAR_FILLED.repeat(filled),
        STATUS_LIMIT_BAR_EMPTY.repeat(empty)
    )
}

pub(crate) fn format_status_limit_summary(percent_remaining: f64) -> String {
    format!("{percent_remaining:.0}% left")
}

/// Builds a single `StatusRateLimitRow` for credits when the snapshot indicates
/// that the account has credit tracking enabled. When credits are unlimited we
/// show that fact explicitly; otherwise we render the rounded balance in
/// credits. Accounts with credits = 0 skip this section entirely.
fn credit_status_row(credits: &CreditsSnapshotDisplay) -> Option<StatusRateLimitRow> {
    if !credits.has_credits {
        return None;
    }
    if credits.unlimited {
        return Some(StatusRateLimitRow {
            label: "Credits".to_string(),
            value: StatusRateLimitValue::Text("Unlimited".to_string()),
        });
    }
    let balance = credits.balance.as_ref()?;
    let display_balance = format_credit_balance(balance)?;
    Some(StatusRateLimitRow {
        label: "Credits".to_string(),
        value: StatusRateLimitValue::Text(format!("{display_balance} credits")),
    })
}

fn format_credit_balance(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(int_value) = trimmed.parse::<i64>()
        && int_value > 0
    {
        return Some(int_value.to_string());
    }

    if let Ok(value) = trimmed.parse::<f64>()
        && value > 0.0
    {
        let rounded = value.round() as i64;
        return Some(rounded.to_string());
    }

    None
}

fn capitalize_first(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => {
            let mut capitalized = first.to_uppercase().collect::<String>();
            capitalized.push_str(chars.as_str());
            capitalized
        }
        None => String::new(),
    }
}
