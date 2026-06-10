//! Model normalization and reasoning-effort clamping.
//!
//! Direct ports from Go `codex-proxy/internal/server/transform.go`.

// ── Model name constants ────────────────────────────────────────────

pub const GPT5: &str = "gpt-5";
pub const GPT5_CODEX: &str = "gpt-5-codex";
pub const GPT5_1: &str = "gpt-5.1";
pub const GPT5_1_CODEX: &str = "gpt-5.1-codex";
pub const GPT5_1_CODEX_MAX: &str = "gpt-5.1-codex-max";
pub const GPT5_2: &str = "gpt-5.2";
pub const GPT5_4: &str = "gpt-5.4";
pub const GPT5_2_CODEX: &str = "gpt-5.2-codex";
pub const GPT5_3_CODEX: &str = "gpt-5.3-codex";
pub const GPT5_3_CODEX_SPARK: &str = "gpt-5.3-codex-spark";
pub const GPT5_5: &str = "gpt-5.5";
pub const GPT5_CODEX_MINI: &str = "gpt-5-codex-mini";
pub const GPT5_1_CODEX_MINI: &str = "gpt-5.1-codex-mini";

const EFFORT_SUFFIXES: &[&str] = &["-xhigh", "-high", "-medium", "-low", "-minimal"];

// ── Per-model allowed efforts ────────────────────────────────────────

fn model_allowed_efforts(model: &str) -> Option<&'static [&'static str]> {
    Some(match model {
        GPT5 => &["minimal", "low", "medium", "high"],
        GPT5_2 | GPT5_4 | GPT5_2_CODEX | GPT5_3_CODEX | GPT5_3_CODEX_SPARK | GPT5_5 => {
            &["low", "medium", "high", "xhigh"]
        }
        GPT5_CODEX => &["minimal", "low", "medium", "high"],
        GPT5_1 | GPT5_1_CODEX => &["low", "medium", "high"],
        GPT5_1_CODEX_MAX => &["low", "medium", "high", "xhigh"],
        GPT5_CODEX_MINI | GPT5_1_CODEX_MINI => &["medium", "high"],
        _ => return None,
    })
}

fn model_default_effort(model: &str) -> Option<&'static str> {
    Some(match model {
        GPT5_1 | GPT5_1_CODEX | GPT5_1_CODEX_MAX => "low",
        GPT5_2 | GPT5_4 | GPT5_2_CODEX | GPT5_3_CODEX | GPT5_5 | GPT5_CODEX_MINI
        | GPT5_1_CODEX_MINI => "medium",
        GPT5_3_CODEX_SPARK => "high",
        _ => return None,
    })
}

// ── Public functions ─────────────────────────────────────────────────

/// Strip effort suffix and map model name to canonical form.
///
/// Port of Go `normalizeModel`.
pub fn normalize_model(model: &str) -> &'static str {
    let lower = model.trim().to_ascii_lowercase();

    // Strip effort suffix
    let mut lower = lower.as_str();
    for suffix in EFFORT_SUFFIXES {
        if lower.ends_with(suffix) {
            lower = &lower[..lower.len() - suffix.len()];
            break;
        }
    }

    if lower.is_empty() {
        return GPT5;
    }

    // Prefer explicit new model IDs first to keep mapping predictable.
    if lower == GPT5_5 {
        return GPT5_5;
    }
    if lower.contains("gpt-5.2-codex") {
        return GPT5_2_CODEX;
    }
    if lower.contains("gpt-5.3-codex-spark") {
        return GPT5_3_CODEX_SPARK;
    }
    if lower.contains("gpt-5.3-codex") {
        return GPT5_3_CODEX;
    }
    if lower.contains("gpt-5.4") {
        return GPT5_4;
    }
    if lower.contains("gpt-5.2") {
        return GPT5_2;
    }
    if lower.contains("gpt-5.1-codex-max") {
        return GPT5_1_CODEX_MAX;
    }
    if lower.contains("gpt-5.1-codex-mini") {
        return GPT5_1_CODEX_MINI;
    }
    if lower.contains("gpt-5.1-codex") {
        return GPT5_1_CODEX;
    }
    if lower.contains("gpt-5.1") {
        return GPT5_1;
    }

    if lower.contains("gpt-5-codex-mini") {
        return GPT5_CODEX_MINI;
    }
    // Fallbacks for older/legacy mini family naming.
    if lower.contains("mini") {
        return GPT5_1_CODEX_MINI;
    }
    if lower.contains("4o") {
        return GPT5_1_CODEX_MINI;
    }
    if lower.contains("gpt-5-codex") || lower.contains("codex") {
        return GPT5_CODEX;
    }

    // Fallback: any other 5-series model collapses to gpt-5.
    GPT5
}

/// Validate and normalize a reasoning effort string.
///
/// Returns the canonical effort level, or empty string for invalid input.
/// `"none"` maps to `"low"`.
///
/// Port of Go `normalizeReasoningEffort`.
pub fn normalize_reasoning_effort(effort: &str) -> &'static str {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "none" => "low",
        _ => "",
    }
}

/// Clamp a normalized effort to the model's allowed set, applying
/// per-model defaults when no effort is specified.
///
/// Port of Go `clampReasoningEffortForModel`.
pub fn clamp_reasoning_effort_for_model(effort: &str, backend_model: &str) -> String {
    let effort = effort.trim();
    let backend_model = backend_model.trim();

    // If nothing specified, fall back to a model default (if any).
    if effort.is_empty() {
        return model_default_effort(backend_model)
            .unwrap_or("")
            .to_string();
    }

    let Some(allowed) = model_allowed_efforts(backend_model) else {
        return effort.to_string(); // no restrictions, pass through
    };

    if allowed.contains(&effort) {
        return effort.to_string();
    }

    // Effort not allowed — fall back to model default.
    model_default_effort(backend_model)
        .unwrap_or(effort)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_model ──────────────────────────────────────────────

    #[test]
    fn normalize_model_codex_lowercase() {
        assert_eq!(normalize_model("gpt-5-codex"), GPT5_CODEX);
    }

    #[test]
    fn normalize_model_codex_uppercase() {
        assert_eq!(normalize_model("GPT-5-CODEX"), GPT5_CODEX);
    }

    #[test]
    fn normalize_model_codex_inside_name() {
        assert_eq!(
            normalize_model("gpt-5-mini-codex-preview"),
            GPT5_1_CODEX_MINI
        );
    }

    #[test]
    fn normalize_model_non_codex() {
        assert_eq!(normalize_model("gpt-5-mini"), GPT5_1_CODEX_MINI);
    }

    #[test]
    fn normalize_model_gpt4o_mini() {
        assert_eq!(normalize_model("gpt-4o-mini"), GPT5_1_CODEX_MINI);
    }

    #[test]
    fn normalize_model_gpt4o_base() {
        assert_eq!(normalize_model("gpt-4o"), GPT5_1_CODEX_MINI);
    }

    #[test]
    fn normalize_model_empty() {
        assert_eq!(normalize_model(""), GPT5);
    }

    #[test]
    fn normalize_model_gpt51_base() {
        assert_eq!(normalize_model("gpt-5.1"), GPT5_1);
    }

    #[test]
    fn normalize_model_gpt52_base() {
        assert_eq!(normalize_model("gpt-5.2"), GPT5_2);
    }

    #[test]
    fn normalize_model_gpt52_with_suffix() {
        assert_eq!(normalize_model("gpt-5.2-high"), GPT5_2);
    }

    #[test]
    fn normalize_model_gpt52_codex_base() {
        assert_eq!(normalize_model("gpt-5.2-codex"), GPT5_2_CODEX);
    }

    #[test]
    fn normalize_model_gpt52_codex_with_suffix() {
        assert_eq!(normalize_model("gpt-5.2-codex-xhigh"), GPT5_2_CODEX);
    }

    #[test]
    fn normalize_model_gpt53_explicit() {
        assert_eq!(normalize_model("gpt-5.3-codex"), GPT5_3_CODEX);
    }

    #[test]
    fn normalize_model_gpt53_with_effort_suffix() {
        assert_eq!(normalize_model("gpt-5.3-codex-high"), GPT5_3_CODEX);
    }

    #[test]
    fn normalize_model_gpt53_spark() {
        assert_eq!(normalize_model("gpt-5.3-codex-spark"), GPT5_3_CODEX_SPARK);
    }

    #[test]
    fn normalize_model_gpt53_spark_with_effort_suffix() {
        assert_eq!(
            normalize_model("gpt-5.3-codex-spark-xhigh"),
            GPT5_3_CODEX_SPARK
        );
    }

    #[test]
    fn normalize_model_gpt54_base() {
        assert_eq!(normalize_model("gpt-5.4"), GPT5_4);
    }

    #[test]
    fn normalize_model_gpt54_with_effort_suffix() {
        assert_eq!(normalize_model("gpt-5.4-high"), GPT5_4);
    }

    #[test]
    fn normalize_model_gpt55_base() {
        assert_eq!(normalize_model("gpt-5.5"), GPT5_5);
    }

    #[test]
    fn normalize_model_gpt55_with_suffix() {
        assert_eq!(normalize_model("gpt-5.5-xhigh"), GPT5_5);
    }

    #[test]
    fn normalize_model_gpt51_with_suffix() {
        assert_eq!(normalize_model("gpt-5.1-high"), GPT5_1);
    }

    #[test]
    fn normalize_model_gpt51_codex() {
        assert_eq!(normalize_model("gpt-5.1-codex"), GPT5_1_CODEX);
    }

    #[test]
    fn normalize_model_gpt51_codex_max() {
        assert_eq!(normalize_model("gpt-5.1-codex-max"), GPT5_1_CODEX_MAX);
    }

    #[test]
    fn normalize_model_gpt51_codex_max_with_suffix() {
        assert_eq!(normalize_model("gpt-5.1-codex-max-xhigh"), GPT5_1_CODEX_MAX);
    }

    #[test]
    fn normalize_model_gpt51_codex_mini() {
        assert_eq!(normalize_model("gpt-5.1-codex-mini"), GPT5_1_CODEX_MINI);
    }

    #[test]
    fn normalize_model_gpt51_codex_mini_with_suffix() {
        assert_eq!(
            normalize_model("gpt-5.1-codex-mini-high"),
            GPT5_1_CODEX_MINI
        );
    }

    #[test]
    fn normalize_model_gpt5_codex_mini() {
        assert_eq!(normalize_model("gpt-5-codex-mini"), GPT5_CODEX_MINI);
    }

    #[test]
    fn normalize_model_gpt5_codex_mini_with_suffix() {
        assert_eq!(normalize_model("gpt-5-codex-mini-low"), GPT5_CODEX_MINI);
    }

    // ── normalize_reasoning_effort ───────────────────────────────────

    #[test]
    fn effort_explicit_minimal() {
        assert_eq!(normalize_reasoning_effort("minimal"), "minimal");
    }

    #[test]
    fn effort_explicit_low() {
        assert_eq!(normalize_reasoning_effort("low"), "low");
    }

    #[test]
    fn effort_explicit_medium() {
        assert_eq!(normalize_reasoning_effort("medium"), "medium");
    }

    #[test]
    fn effort_explicit_high() {
        assert_eq!(normalize_reasoning_effort("high"), "high");
    }

    #[test]
    fn effort_explicit_xhigh() {
        assert_eq!(normalize_reasoning_effort("xhigh"), "xhigh");
    }

    #[test]
    fn effort_none_maps_to_low() {
        assert_eq!(normalize_reasoning_effort("none"), "low");
    }

    #[test]
    fn effort_uppercase() {
        assert_eq!(normalize_reasoning_effort("MEDIUM"), "medium");
    }

    #[test]
    fn effort_empty() {
        assert_eq!(normalize_reasoning_effort(""), "");
    }

    #[test]
    fn effort_invalid() {
        assert_eq!(normalize_reasoning_effort("aggressive"), "");
    }

    // ── clamp_reasoning_effort_for_model ─────────────────────────────

    #[test]
    fn clamp_gpt5_allows_minimal() {
        assert_eq!(clamp_reasoning_effort_for_model("minimal", GPT5), "minimal");
    }

    #[test]
    fn clamp_gpt51_disallows_minimal() {
        assert_eq!(clamp_reasoning_effort_for_model("minimal", GPT5_1), "low");
    }

    #[test]
    fn clamp_gpt51_default_when_empty() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_1), "low");
    }

    #[test]
    fn clamp_gpt52_allows_xhigh() {
        assert_eq!(clamp_reasoning_effort_for_model("xhigh", GPT5_2), "xhigh");
    }

    #[test]
    fn clamp_gpt52_default_when_empty() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_2), "medium");
    }

    #[test]
    fn clamp_gpt54_disallows_minimal() {
        assert_eq!(
            clamp_reasoning_effort_for_model("minimal", GPT5_4),
            "medium"
        );
    }

    #[test]
    fn clamp_gpt52_codex_allows_xhigh() {
        assert_eq!(
            clamp_reasoning_effort_for_model("xhigh", GPT5_2_CODEX),
            "xhigh"
        );
    }

    #[test]
    fn clamp_gpt52_codex_default() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_2_CODEX), "medium");
    }

    #[test]
    fn clamp_gpt53_codex_default() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_3_CODEX), "medium");
    }

    #[test]
    fn clamp_gpt53_codex_spark_default() {
        assert_eq!(
            clamp_reasoning_effort_for_model("", GPT5_3_CODEX_SPARK),
            "high"
        );
    }

    #[test]
    fn clamp_gpt53_codex_spark_allows_xhigh() {
        assert_eq!(
            clamp_reasoning_effort_for_model("xhigh", GPT5_3_CODEX_SPARK),
            "xhigh"
        );
    }

    #[test]
    fn clamp_gpt55_default() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_5), "medium");
    }

    #[test]
    fn clamp_gpt5_codex_mini_clamps_low() {
        assert_eq!(
            clamp_reasoning_effort_for_model("low", GPT5_CODEX_MINI),
            "medium"
        );
    }

    #[test]
    fn clamp_gpt5_codex_mini_default() {
        assert_eq!(
            clamp_reasoning_effort_for_model("", GPT5_CODEX_MINI),
            "medium"
        );
    }

    #[test]
    fn clamp_gpt51_codex_allows_high() {
        assert_eq!(
            clamp_reasoning_effort_for_model("high", GPT5_1_CODEX),
            "high"
        );
    }

    #[test]
    fn clamp_gpt51_codex_max_allows_xhigh() {
        assert_eq!(
            clamp_reasoning_effort_for_model("xhigh", GPT5_1_CODEX_MAX),
            "xhigh"
        );
    }

    #[test]
    fn clamp_gpt51_codex_max_minimal_to_low() {
        assert_eq!(
            clamp_reasoning_effort_for_model("minimal", GPT5_1_CODEX_MAX),
            "low"
        );
    }
}
