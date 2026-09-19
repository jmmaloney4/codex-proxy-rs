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
pub const GPT5_6_SOL: &str = "gpt-5.6-sol";
pub const GPT5_6_TERRA: &str = "gpt-5.6-terra";
pub const GPT5_6_LUNA: &str = "gpt-5.6-luna";
pub const GPT6_ASTRA: &str = "gpt-6-astra";
pub const GPT5_CODEX_MINI: &str = "gpt-5-codex-mini";
pub const GPT5_1_CODEX_MINI: &str = "gpt-5.1-codex-mini";
pub const GPT5_4_MINI: &str = "gpt-5.4-mini";

const EFFORT_SUFFIXES: &[&str] = &[
    "-ultra", "-xhigh", "-high", "-medium", "-low", "-minimal", "-max",
];

/// Every model id this proxy knows, as a single list.
///
/// `normalize_model` consults this **before** stripping an effort suffix,
/// because `gpt-5.1-codex-max` ends in `-max`, which is also a reasoning
/// level. Stripping first would rewrite it to `gpt-5.1-codex` and silently
/// serve a weaker model than the caller asked for.
const KNOWN_MODELS: &[&str] = &[
    GPT6_ASTRA,
    GPT5_6_SOL,
    GPT5_6_TERRA,
    GPT5_6_LUNA,
    GPT5_5,
    GPT5_4,
    GPT5_4_MINI,
    GPT5_3_CODEX_SPARK,
    GPT5_3_CODEX,
    GPT5_2_CODEX,
    GPT5_2,
    GPT5_1_CODEX_MAX,
    GPT5_1_CODEX_MINI,
    GPT5_1_CODEX,
    GPT5_1,
    GPT5_CODEX_MINI,
    GPT5_CODEX,
    GPT5,
];

/// Exact (already-lowercased) model id lookup against [`KNOWN_MODELS`].
fn exact_model(id: &str) -> Option<&'static str> {
    KNOWN_MODELS.iter().copied().find(|known| *known == id)
}

// ── Per-model allowed efforts ────────────────────────────────────────

fn model_allowed_efforts(model: &str) -> Option<&'static [&'static str]> {
    Some(match model {
        GPT5 => &["minimal", "low", "medium", "high"],
        GPT5_2 | GPT5_4 | GPT5_4_MINI | GPT5_2_CODEX | GPT5_3_CODEX | GPT5_3_CODEX_SPARK
        | GPT5_5 => &["low", "medium", "high", "xhigh"],
        // The 5.6 tiers and gpt-6-astra reach past `xhigh`. Sets mirror
        // `supported_reasoning_levels` in openai/codex
        // `codex-rs/models-manager/models.json` (rust-v0.155.0); luna is the
        // one tier without `ultra`.
        GPT6_ASTRA | GPT5_6_SOL | GPT5_6_TERRA => {
            &["low", "medium", "high", "xhigh", "max", "ultra"]
        }
        GPT5_6_LUNA => &["low", "medium", "high", "xhigh", "max"],
        GPT5_CODEX => &["minimal", "low", "medium", "high"],
        GPT5_1 | GPT5_1_CODEX => &["low", "medium", "high"],
        GPT5_1_CODEX_MAX => &["low", "medium", "high", "xhigh"],
        GPT5_CODEX_MINI | GPT5_1_CODEX_MINI => &["medium", "high"],
        _ => return None,
    })
}

fn model_default_effort(model: &str) -> Option<&'static str> {
    Some(match model {
        GPT5_1 | GPT5_1_CODEX | GPT5_1_CODEX_MAX | GPT5_6_SOL | GPT6_ASTRA => "low",
        GPT5_2 | GPT5_4 | GPT5_4_MINI | GPT5_2_CODEX | GPT5_3_CODEX | GPT5_5 | GPT5_CODEX_MINI
        | GPT5_1_CODEX_MINI | GPT5_6_TERRA | GPT5_6_LUNA => "medium",
        GPT5_3_CODEX_SPARK => "high",
        _ => return None,
    })
}

// ── Public functions ─────────────────────────────────────────────────

/// Whether an (already-lowercased) id names something in the gpt-6 family.
///
/// Deliberately narrower than `starts_with("gpt-6")`, which would also swallow
/// `gpt-60` and `gpt-6experimental` and serve them as astra. Accepts the bare
/// alias, the `gpt-6-<tier>` shape the 6 line uses today, and `gpt-6.<n>` so a
/// future point release stays in its family instead of falling through to the
/// generic gpt-5 fallback.
fn is_gpt6_family(id: &str) -> bool {
    id == "gpt-6" || id.starts_with("gpt-6-") || id.starts_with("gpt-6.")
}

/// The reasoning effort encoded in a model id's `-<effort>` suffix, if any.
///
/// The read half of [`EFFORT_SUFFIXES`]; `normalize_model` is the strip half.
/// Both must agree on the vocabulary *and* the precedence, so both consult
/// [`exact_model`] first: `gpt-5.1-codex-max` is a model name, not a request
/// for `max` effort, and reading it as the latter would hand the caller a
/// silently different model-and-effort pair than the one they asked for.
pub fn effort_suffix(model: &str) -> Option<&'static str> {
    let lower = model.trim().to_ascii_lowercase();
    if exact_model(&lower).is_some() {
        return None;
    }
    for suffix in EFFORT_SUFFIXES.iter().copied() {
        if let Some(rest) = lower.strip_suffix(suffix) {
            if rest.is_empty() {
                return None;
            }
            return Some(&suffix[1..]);
        }
    }
    None
}

/// Strip effort suffix and map model name to canonical form.
///
/// Port of Go `normalizeModel`.
pub fn normalize_model(model: &str) -> &'static str {
    let lower = model.trim().to_ascii_lowercase();

    // A known id matches as-is, before any suffix stripping — see KNOWN_MODELS
    // for why the order matters (`gpt-5.1-codex-max`).
    if let Some(known) = exact_model(&lower) {
        return known;
    }

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

    // ...and again once the effort suffix is gone (`gpt-6-astra-ultra`,
    // `gpt-5.1-codex-max-high`).
    if let Some(known) = exact_model(lower) {
        return known;
    }

    // Family guards. An unrecognized variant of a known family must stay in
    // that family rather than collapsing to the generic gpt-5 fallback at the
    // bottom of this function, which would silently serve a much weaker model.
    // The bare `gpt-6` / `gpt-5.6` aliases resolve to each family's flagship.
    if is_gpt6_family(lower) {
        return GPT6_ASTRA;
    }
    if lower == "gpt-5.6" || lower.starts_with("gpt-5.6-") {
        return GPT5_6_SOL;
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
    // Must precede the bare `gpt-5.4` check below: "gpt-5.4-mini" contains the
    // "gpt-5.4" substring, so without this it would collapse to full gpt-5.4.
    if lower.contains("gpt-5.4-mini") {
        return GPT5_4_MINI;
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
/// `"none"` maps to `"low"`. `max` and `ultra` are the two levels the 5.6
/// tiers and gpt-6-astra added above `xhigh`; `clamp_reasoning_effort_for_model`
/// is what keeps them off the models that do not accept them.
///
/// Port of Go `normalizeReasoningEffort`.
pub fn normalize_reasoning_effort(effort: &str) -> &'static str {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        "ultra" => "ultra",
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
    fn normalize_model_gpt54_mini_base() {
        assert_eq!(normalize_model("gpt-5.4-mini"), GPT5_4_MINI);
    }

    #[test]
    fn normalize_model_gpt54_mini_with_effort_suffix() {
        // The "-high" effort suffix is stripped first, leaving "gpt-5.4-mini",
        // which must map to the mini and not collapse to full gpt-5.4.
        assert_eq!(normalize_model("gpt-5.4-mini-high"), GPT5_4_MINI);
    }

    #[test]
    fn normalize_model_gpt54_mini_does_not_collapse_to_full() {
        // Regression guard for the substring-ordering bug: "gpt-5.4-mini"
        // contains "gpt-5.4", so the mini check must run first.
        assert_ne!(normalize_model("gpt-5.4-mini"), GPT5_4);
    }

    #[test]
    fn clamp_effort_gpt54_mini_defaults_to_medium() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_4_MINI), "medium");
    }

    #[test]
    fn normalize_model_gpt55_base() {
        assert_eq!(normalize_model("gpt-5.5"), GPT5_5);
    }

    #[test]
    fn normalize_model_gpt55_with_suffix() {
        assert_eq!(normalize_model("gpt-5.5-xhigh"), GPT5_5);
    }

    // ---- gpt-5.6 tier family ----

    #[test]
    fn normalize_model_gpt56_bare_resolves_to_sol() {
        assert_eq!(normalize_model("gpt-5.6"), GPT5_6_SOL);
    }

    #[test]
    fn normalize_model_gpt56_sol_base() {
        assert_eq!(normalize_model("gpt-5.6-sol"), GPT5_6_SOL);
    }

    #[test]
    fn normalize_model_gpt56_terra_base() {
        assert_eq!(normalize_model("gpt-5.6-terra"), GPT5_6_TERRA);
    }

    #[test]
    fn normalize_model_gpt56_luna_base() {
        assert_eq!(normalize_model("gpt-5.6-luna"), GPT5_6_LUNA);
    }

    #[test]
    fn normalize_model_gpt56_sol_with_effort_suffix() {
        assert_eq!(normalize_model("gpt-5.6-sol-high"), GPT5_6_SOL);
    }

    #[test]
    fn normalize_model_gpt56_terra_with_effort_suffix() {
        assert_eq!(normalize_model("gpt-5.6-terra-xhigh"), GPT5_6_TERRA);
    }

    #[test]
    fn normalize_model_gpt56_luna_with_effort_suffix() {
        assert_eq!(normalize_model("gpt-5.6-luna-low"), GPT5_6_LUNA);
    }

    #[test]
    fn normalize_model_gpt56_sol_uppercase() {
        assert_eq!(normalize_model("GPT-5.6-SOL"), GPT5_6_SOL);
    }

    #[test]
    fn normalize_model_gpt56_unknown_tier_does_not_collapse_to_gpt5() {
        // An unrecognized 5.6 variant must not silently fall back to gpt-5.
        // "gpt-5.6-pro" is not a real tier; we assert it resolves to the bare
        // family alias (gpt-5.6) → sol, NOT to gpt-5.
        assert_eq!(normalize_model("gpt-5.6-pro"), GPT5_6_SOL);
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
    fn clamp_gpt56_sol_allows_xhigh() {
        assert_eq!(
            clamp_reasoning_effort_for_model("xhigh", GPT5_6_SOL),
            "xhigh"
        );
    }

    #[test]
    fn clamp_gpt56_sol_default_when_empty() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_6_SOL), "low");
    }

    #[test]
    fn clamp_gpt56_sol_disallows_minimal() {
        assert_eq!(
            clamp_reasoning_effort_for_model("minimal", GPT5_6_SOL),
            "low"
        );
    }

    #[test]
    fn clamp_gpt56_terra_default_when_empty() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_6_TERRA), "medium");
    }

    #[test]
    fn clamp_gpt56_terra_clamps_invalid_to_default() {
        // "aggressive" is not a valid effort; after normalize it's "" but clamp
        // is called with the already-normalized value, so an out-of-set effort
        // falls back to the model default. (This test used to use "ultra",
        // which is now a real level terra accepts — hence "turbo".)
        assert_eq!(
            clamp_reasoning_effort_for_model("turbo", GPT5_6_TERRA),
            "medium"
        );
    }

    #[test]
    fn clamp_gpt56_luna_default_when_empty() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT5_6_LUNA), "medium");
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

    // ---- gpt-6 family ----

    #[test]
    fn normalize_model_gpt6_astra_base() {
        assert_eq!(normalize_model("gpt-6-astra"), GPT6_ASTRA);
    }

    #[test]
    fn normalize_model_gpt6_astra_uppercase() {
        assert_eq!(normalize_model("GPT-6-ASTRA"), GPT6_ASTRA);
    }

    #[test]
    fn normalize_model_gpt6_astra_with_effort_suffixes() {
        for id in [
            "gpt-6-astra-low",
            "gpt-6-astra-medium",
            "gpt-6-astra-high",
            "gpt-6-astra-xhigh",
            "gpt-6-astra-max",
            "gpt-6-astra-ultra",
        ] {
            assert_eq!(normalize_model(id), GPT6_ASTRA, "{id}");
        }
    }

    #[test]
    fn normalize_model_gpt6_bare_resolves_to_astra() {
        assert_eq!(normalize_model("gpt-6"), GPT6_ASTRA);
    }

    #[test]
    fn normalize_model_gpt6_unknown_variant_does_not_collapse_to_gpt5() {
        // The defect this guard exists for: before gpt-6 was known, every
        // 6-series id fell through to the generic gpt-5 fallback and was served
        // as gpt-5 with no error to the caller.
        assert_eq!(normalize_model("gpt-6-nova"), GPT6_ASTRA);
        assert_ne!(normalize_model("gpt-6-nova"), GPT5);
        // A future point release stays in the family too.
        assert_eq!(normalize_model("gpt-6.1"), GPT6_ASTRA);
        assert_eq!(normalize_model("gpt-6.1-nova"), GPT6_ASTRA);
    }

    #[test]
    fn normalize_model_gpt6_guard_does_not_swallow_lookalikes() {
        // `starts_with("gpt-6")` alone would route these to astra. They are not
        // gpt-6 models, so they take the generic fallback like any other
        // unknown id rather than being served as the family flagship.
        for id in ["gpt-60", "gpt-6experimental", "gpt-64k"] {
            assert_eq!(normalize_model(id), GPT5, "{id}");
        }
    }

    #[test]
    fn normalize_model_gpt6_astra_is_not_gpt5() {
        assert_ne!(normalize_model("gpt-6-astra"), GPT5);
    }

    // ---- `-max` is both an effort suffix and part of a real model id ----

    #[test]
    fn normalize_model_codex_max_survives_max_effort_suffix() {
        // Regression guard for adding "-max" to EFFORT_SUFFIXES: stripping it
        // blindly would rewrite gpt-5.1-codex-max to gpt-5.1-codex.
        assert_eq!(normalize_model("gpt-5.1-codex-max"), GPT5_1_CODEX_MAX);
        assert_eq!(normalize_model("GPT-5.1-CODEX-MAX"), GPT5_1_CODEX_MAX);
    }

    #[test]
    fn normalize_model_codex_max_with_effort_suffix_still_resolves() {
        assert_eq!(normalize_model("gpt-5.1-codex-max-high"), GPT5_1_CODEX_MAX);
        assert_eq!(normalize_model("gpt-5.1-codex-max-xhigh"), GPT5_1_CODEX_MAX);
    }

    // ---- max / ultra effort levels ----

    #[test]
    fn effort_explicit_max() {
        assert_eq!(normalize_reasoning_effort("max"), "max");
    }

    #[test]
    fn effort_explicit_ultra() {
        assert_eq!(normalize_reasoning_effort("ultra"), "ultra");
    }

    #[test]
    fn effort_ultra_uppercase() {
        assert_eq!(normalize_reasoning_effort("ULTRA"), "ultra");
    }

    #[test]
    fn clamp_gpt6_astra_default_is_low() {
        assert_eq!(clamp_reasoning_effort_for_model("", GPT6_ASTRA), "low");
    }

    #[test]
    fn clamp_gpt6_astra_allows_max_and_ultra() {
        assert_eq!(clamp_reasoning_effort_for_model("max", GPT6_ASTRA), "max");
        assert_eq!(
            clamp_reasoning_effort_for_model("ultra", GPT6_ASTRA),
            "ultra"
        );
    }

    #[test]
    fn clamp_gpt6_astra_disallows_minimal() {
        assert_eq!(
            clamp_reasoning_effort_for_model("minimal", GPT6_ASTRA),
            "low"
        );
    }

    #[test]
    fn clamp_gpt56_sol_allows_max_and_ultra() {
        assert_eq!(clamp_reasoning_effort_for_model("max", GPT5_6_SOL), "max");
        assert_eq!(
            clamp_reasoning_effort_for_model("ultra", GPT5_6_SOL),
            "ultra"
        );
    }

    #[test]
    fn clamp_gpt56_luna_allows_max_but_not_ultra() {
        // Upstream lists `max` but not `ultra` for luna; an ultra request
        // clamps to luna's default rather than being passed through.
        assert_eq!(clamp_reasoning_effort_for_model("max", GPT5_6_LUNA), "max");
        assert_eq!(
            clamp_reasoning_effort_for_model("ultra", GPT5_6_LUNA),
            "medium"
        );
    }

    #[test]
    fn clamp_gpt52_rejects_ultra() {
        // The 5.x line never gained max/ultra; they clamp to the model default.
        assert_eq!(clamp_reasoning_effort_for_model("ultra", GPT5_2), "medium");
        assert_eq!(clamp_reasoning_effort_for_model("max", GPT5_2), "medium");
    }

    #[test]
    fn every_known_model_normalizes_to_itself() {
        // The exact-id table and the substring chain must not disagree.
        for id in KNOWN_MODELS {
            assert_eq!(normalize_model(id), *id, "{id}");
        }
    }

    // ---- effort_suffix: the read half of EFFORT_SUFFIXES ----

    #[test]
    fn effort_suffix_reads_each_level() {
        for (id, want) in [
            ("gpt-6-astra-low", "low"),
            ("gpt-6-astra-medium", "medium"),
            ("gpt-6-astra-high", "high"),
            ("gpt-6-astra-xhigh", "xhigh"),
            ("gpt-6-astra-max", "max"),
            ("gpt-6-astra-ultra", "ultra"),
            ("gpt-5-minimal", "minimal"),
            ("GPT-5.2-HIGH", "high"),
        ] {
            assert_eq!(effort_suffix(id), Some(want), "{id}");
        }
    }

    #[test]
    fn effort_suffix_none_for_bare_ids() {
        for id in ["gpt-6-astra", "gpt-5.2", "gpt-5.6-sol", ""] {
            assert_eq!(effort_suffix(id), None, "{id}");
        }
    }

    #[test]
    fn effort_suffix_none_for_codex_max_model_id() {
        // The whole reason exact_model is consulted first: codex-max is a
        // model, not a `max` effort request.
        assert_eq!(effort_suffix("gpt-5.1-codex-max"), None);
        // ...but an effort suffix *on top of* it still reads.
        assert_eq!(effort_suffix("gpt-5.1-codex-max-xhigh"), Some("xhigh"));
    }

    #[test]
    fn effort_suffix_agrees_with_normalize_model() {
        // Whatever normalize_model strips, effort_suffix must report — one
        // vocabulary, read and stripped the same way.
        for base in KNOWN_MODELS {
            for effort in ["low", "medium", "high", "xhigh", "max", "ultra"] {
                let id = format!("{base}-{effort}");
                // One exception, by design: when base+effort spells another
                // real model id, the model id wins in both halves. See
                // `suffix_encoding_cannot_reach_codex_at_max_effort`.
                if exact_model(&id).is_some() {
                    continue;
                }
                assert_eq!(normalize_model(&id), *base, "normalize {id}");
                assert_eq!(effort_suffix(&id), Some(effort), "suffix {id}");
            }
        }
    }

    #[test]
    fn suffix_encoding_cannot_reach_codex_at_max_effort() {
        // "gpt-5.1-codex" + max effort spells "gpt-5.1-codex-max", a different
        // (stronger) model. The id wins, so this pair is simply unreachable
        // through the suffix encoding — callers wanting it must send an
        // explicit `reasoning_effort`. Harmless in practice: gpt-5.1-codex
        // does not accept `max` anyway, so the request would clamp.
        assert_eq!(normalize_model("gpt-5.1-codex-max"), GPT5_1_CODEX_MAX);
        assert_eq!(effort_suffix("gpt-5.1-codex-max"), None);
        assert!(
            !model_allowed_efforts(GPT5_1_CODEX)
                .unwrap()
                .contains(&"max")
        );
    }
}
