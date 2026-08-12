use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const PRIMARY_JSON: &str = include_str!("../../../resources/pricing/pricing_litellm_snapshot.json");
const SECONDARY_JSON: &str =
    include_str!("../../../resources/pricing/pricing_models_dev_snapshot.json");
const SUPPLEMENT_JSON: &str = include_str!("../../../resources/pricing/pricing_supplement.json");

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TokenBreakdown {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
    pub fast: bool,
}

impl TokenBreakdown {
    pub fn total(self) -> f64 {
        self.input + self.cache_write_5m + self.cache_write_1h + self.cache_read + self.output
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct Rates {
    #[serde(alias = "input_per_million")]
    i: f64,
    #[serde(alias = "output_per_million")]
    o: f64,
    #[serde(default, alias = "cache_write_per_million")]
    cw: Option<f64>,
    #[serde(default, alias = "cache_read_per_million")]
    cr: Option<f64>,
    #[serde(default)]
    ia: Option<f64>,
    #[serde(default)]
    oa: Option<f64>,
    #[serde(default)]
    cwa: Option<f64>,
    #[serde(default)]
    cra: Option<f64>,
    #[serde(default)]
    fast: Option<f64>,
}

impl Rates {
    fn complete(self) -> Self {
        Self {
            cw: Some(self.cw.unwrap_or(self.i)),
            cr: Some(self.cr.unwrap_or(self.i * 0.1)),
            ..self
        }
    }

    fn scaled(self, multiplier: f64) -> Self {
        Self {
            i: self.i * multiplier,
            o: self.o * multiplier,
            cw: self.cw.map(|rate| rate * multiplier),
            cr: self.cr.map(|rate| rate * multiplier),
            ia: self.ia.map(|rate| rate * multiplier),
            oa: self.oa.map(|rate| rate * multiplier),
            cwa: self.cwa.map(|rate| rate * multiplier),
            cra: self.cra.map(|rate| rate * multiplier),
            fast: Some(1.0),
        }
    }

    fn cost(self, tokens: TokenBreakdown, apply_long_context: bool) -> f64 {
        let long_context = apply_long_context
            && tokens.input + tokens.cache_write_5m + tokens.cache_write_1h + tokens.cache_read
                > 200_000.0;
        let select = |base: f64, high: Option<f64>| {
            if long_context {
                high.unwrap_or(base)
            } else {
                base
            }
        };
        let input = select(self.i, self.ia);
        let output = select(self.o, self.oa);
        let cache_write = select(self.cw.unwrap_or(self.i), self.cwa);
        let cache_read = select(self.cr.unwrap_or(self.i * 0.1), self.cra);
        let mut cost = tokens.input * input
            + tokens.output * output
            + tokens.cache_write_5m * cache_write
            + tokens.cache_write_1h * input * 2.0
            + tokens.cache_read * cache_read;
        if tokens.fast {
            cost *= self.fast.unwrap_or(1.0);
        }
        cost / 1_000_000.0
    }
}

#[derive(Deserialize)]
struct CompactCatalog {
    models: HashMap<String, Rates>,
}

#[derive(Deserialize)]
struct SupplementFile {
    pricing: HashMap<String, Rates>,
    #[serde(default)]
    fast_multipliers: HashMap<String, f64>,
    #[serde(default)]
    alias_rules: Vec<AliasFile>,
}

#[derive(Deserialize)]
struct AliasFile {
    pattern: String,
    canonical: String,
}

struct AliasRule {
    pattern: Regex,
    canonical: String,
}

struct Pricing {
    supplement: HashMap<String, Rates>,
    fast_multipliers: HashMap<String, f64>,
    aliases: Vec<AliasRule>,
    primary: HashMap<String, Rates>,
    secondary: HashMap<String, Rates>,
}

static PRICING: OnceLock<Pricing> = OnceLock::new();
static RESOLUTION_CACHE: OnceLock<Mutex<HashMap<String, Option<Rates>>>> = OnceLock::new();

pub fn estimate_cost(model: &str, tokens: TokenBreakdown) -> Option<f64> {
    let rates = resolve(model)?;
    Some(rates.cost(tokens, true).max(0.0))
}

/// Price aggregate rows (such as Cursor's daily CSV export) without applying
/// request-level long-context tiers that cannot be inferred from aggregate totals.
pub fn estimate_aggregated_cost(model: &str, tokens: TokenBreakdown) -> Option<f64> {
    let rates = resolve(model)?;
    Some(rates.cost(tokens, false).max(0.0))
}

fn resolve(model: &str) -> Option<Rates> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let cache = RESOLUTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache.lock().ok()?.get(model).copied() {
        return cached;
    }
    let pricing = PRICING.get_or_init(load_pricing);
    let resolved = pricing
        .aliases
        .iter()
        .find(|rule| rule.pattern.is_match(model))
        .and_then(|rule| {
            pricing
                .lookup(&rule.canonical)
                .or_else(|| pricing.lookup(model))
        })
        .or_else(|| pricing.lookup(model));
    if let Ok(mut cache) = cache.lock() {
        cache.insert(model.to_string(), resolved);
    }
    resolved
}

fn load_pricing() -> Pricing {
    let primary: CompactCatalog =
        serde_json::from_str(PRIMARY_JSON).expect("bundled LiteLLM pricing must be valid");
    let secondary: CompactCatalog =
        serde_json::from_str(SECONDARY_JSON).expect("bundled models.dev pricing must be valid");
    let supplement: SupplementFile =
        serde_json::from_str(SUPPLEMENT_JSON).expect("bundled pricing supplement must be valid");
    let aliases = supplement
        .alias_rules
        .into_iter()
        .filter_map(|rule| match Regex::new(&rule.pattern) {
            Ok(pattern) => Some(AliasRule {
                pattern,
                canonical: rule.canonical,
            }),
            Err(error) => {
                tracing::warn!(pattern = %rule.pattern, %error, "invalid bundled pricing alias");
                None
            }
        })
        .collect();
    Pricing {
        supplement: supplement
            .pricing
            .into_iter()
            .map(|(model, rates)| (model, rates.complete()))
            .collect(),
        fast_multipliers: supplement.fast_multipliers,
        aliases,
        primary: primary
            .models
            .into_iter()
            .map(|(model, rates)| (model, rates.complete()))
            .collect(),
        secondary: secondary
            .models
            .into_iter()
            .map(|(model, rates)| (model, rates.complete()))
            .collect(),
    }
}

impl Pricing {
    fn lookup(&self, model: &str) -> Option<Rates> {
        if let Some(rates) = self.supplement.get(model).copied() {
            return Some(rates);
        }
        if let Some(rates) = self.primary.get(model).copied() {
            return Some(rates);
        }
        if let Some(base) = model.strip_suffix("-fast") {
            if let Some((key, rates)) = self.base_entry(base) {
                let multiplier = rates
                    .fast
                    .filter(|value| *value != 1.0)
                    .or_else(|| self.fast_multiplier(&key))
                    .or_else(|| self.fast_multiplier(base));
                if let Some(multiplier) = multiplier {
                    return Some(rates.scaled(multiplier));
                }
            }
            return self.secondary.get(model).copied();
        }
        fuzzy(&self.primary, model)
            .map(|(_, rates)| rates)
            .or_else(|| self.secondary.get(model).copied())
    }

    fn base_entry(&self, model: &str) -> Option<(String, Rates)> {
        self.supplement
            .get(model)
            .copied()
            .map(|rates| (model.to_string(), rates))
            .or_else(|| {
                self.primary
                    .get(model)
                    .copied()
                    .map(|rates| (model.to_string(), rates))
            })
            .or_else(|| fuzzy(&self.primary, model))
            .or_else(|| {
                self.secondary
                    .get(model)
                    .copied()
                    .map(|rates| (model.to_string(), rates))
            })
    }

    fn fast_multiplier(&self, model: &str) -> Option<f64> {
        if let Some(multiplier) = self.fast_multipliers.get(model) {
            return Some(*multiplier);
        }
        let normalized = normalized_key(model);
        normalized.split(['/', ':']).find_map(|part| {
            self.fast_multipliers.iter().find_map(|(base, multiplier)| {
                let base = normalized_key(base);
                part.rfind(&base).and_then(|index| {
                    let suffix = &part[index + base.len()..];
                    (suffix.is_empty() || suffix.starts_with('-')).then_some(*multiplier)
                })
            })
        })
    }
}

fn fuzzy(catalog: &HashMap<String, Rates>, model: &str) -> Option<(String, Rates)> {
    let normalized_model = normalized_key(model);
    catalog
        .iter()
        .filter(|(candidate, _)| key_matches(candidate, model, &normalized_model))
        .map(|(key, rates)| (key.clone(), *rates))
        .max_by(|left, right| {
            left.0
                .len()
                .cmp(&right.0.len())
                .then_with(|| right.0.cmp(&left.0))
        })
}

fn normalized_key(value: &str) -> String {
    value.replace(['.', '@'], "-")
}

fn key_matches(candidate: &str, model: &str, normalized_model: &str) -> bool {
    contains_key(model, candidate)
        || contains_key(candidate, model)
        || contains_key(normalized_model, &normalized_key(candidate))
        || contains_key(&normalized_key(candidate), normalized_model)
}

fn contains_key(value: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    value.match_indices(key).any(|(start, _)| {
        let before_ok = start == 0 || !value.as_bytes()[start - 1].is_ascii_alphanumeric();
        before_ok && suffix_allows_match(key.as_bytes(), &value.as_bytes()[start + key.len()..])
    })
}

fn suffix_allows_match(key: &[u8], suffix: &[u8]) -> bool {
    let Some(separator) = suffix.first() else {
        return true;
    };
    if separator.is_ascii_alphanumeric() {
        return false;
    }
    let Some(last) = key.last() else {
        return true;
    };
    if !last.is_ascii_digit() || !matches!(separator, b'-' | b'.') {
        return true;
    }
    let digits = suffix[1..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return true;
    }
    let date_suffix = digits == 8
        && suffix
            .get(digits + 1)
            .is_none_or(|byte| !byte.is_ascii_alphanumeric());
    date_suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_exact_and_aliased_models_from_bundled_catalogs() {
        let tokens = TokenBreakdown {
            input: 1_000_000.0,
            output: 1_000_000.0,
            ..TokenBreakdown::default()
        };
        assert_eq!(estimate_cost("gpt-5.2-codex", tokens), Some(15.75));
        assert_eq!(estimate_cost("grok-code-fast-1", tokens), Some(3.0));
    }

    #[test]
    fn cache_buckets_use_their_own_rates() {
        let tokens = TokenBreakdown {
            cache_write_5m: 1_000_000.0,
            cache_read: 1_000_000.0,
            ..TokenBreakdown::default()
        };
        assert_eq!(
            estimate_cost("claude-sonnet-4-5-20250929", tokens),
            Some(8.1)
        );
        assert_eq!(
            estimate_aggregated_cost("claude-sonnet-4-5-20250929", tokens),
            Some(4.05)
        );
    }

    #[test]
    fn fuzzy_versions_do_not_conflate_minor_releases() {
        assert!(!contains_key("claude-sonnet-4-5", "claude-sonnet-4"));
        assert!(contains_key("claude-sonnet-4-20250514", "claude-sonnet-4"));
    }
}
