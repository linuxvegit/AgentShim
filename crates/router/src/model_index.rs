use std::collections::{BTreeMap, BTreeSet, HashMap};

use agent_shim_core::ModelMetadata;

struct ModelEntry {
    original: String,
    normalized: String,
    tokens: Vec<String>,
}

/// Map of `provider name → (upstream model id → metadata)` plus pre-tokenised
/// entries used by the fuzzy resolver. Plan B Task 4 extended this beyond
/// "set of names" so the catalog endpoints can surface upstream-reported
/// capabilities (context window, vision support, family) without re-walking
/// the original JSON each time.
pub struct ModelIndex {
    /// Per-provider metadata. Source of truth for both `resolve` (via the
    /// tokenised mirror below) and the new `metadata` / `provider_models`
    /// accessors that back `/v1/models`.
    metadata: HashMap<String, BTreeMap<String, ModelMetadata>>,
    /// Pre-tokenised mirror of the keys in `metadata`, used by the existing
    /// fuzzy resolver. Built at construction time so per-request lookups
    /// don't pay tokenisation cost.
    fuzzy: HashMap<String, Vec<ModelEntry>>,
}

fn tokenize(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(['-', '_', '.', '/'])
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn score(
    requested_tokens: &[String],
    candidate_tokens: &[String],
    req_norm: &str,
    cand_norm: &str,
) -> f64 {
    if req_norm == cand_norm {
        return 1.0;
    }

    if cand_norm.starts_with(req_norm) {
        let ratio = req_norm.len() as f64 / cand_norm.len() as f64;
        return 0.8 + 0.2 * ratio;
    }
    if req_norm.starts_with(cand_norm) {
        let ratio = cand_norm.len() as f64 / req_norm.len() as f64;
        return 0.8 + 0.2 * ratio;
    }

    let max_len = requested_tokens.len().max(candidate_tokens.len());
    if max_len == 0 {
        return 0.0;
    }

    let mut weighted_matches = 0.0;
    let mut total_weight = 0.0;

    for (i, req_tok) in requested_tokens.iter().enumerate() {
        let weight = 1.0 / (1.0 + i as f64);
        total_weight += weight;
        if candidate_tokens.contains(req_tok) {
            weighted_matches += weight;
        }
    }

    for (i, _cand_tok) in candidate_tokens.iter().enumerate() {
        if i >= requested_tokens.len() {
            let weight = 1.0 / (1.0 + i as f64);
            total_weight += weight;
        }
    }

    weighted_matches / total_weight
}

const THRESHOLD: f64 = 0.5;

impl ModelIndex {
    /// Primary constructor: takes the metadata-bearing per-provider map. The
    /// fuzzy resolver is built from the same map's keys.
    pub fn with_metadata(providers: HashMap<String, BTreeMap<String, ModelMetadata>>) -> Self {
        let fuzzy = providers
            .iter()
            .map(|(provider, map)| {
                let entries = map
                    .keys()
                    .map(|name| {
                        let normalized = name.to_lowercase();
                        let tokens = tokenize(name);
                        ModelEntry {
                            original: name.clone(),
                            normalized,
                            tokens,
                        }
                    })
                    .collect();
                (provider.clone(), entries)
            })
            .collect();
        Self {
            metadata: providers,
            fuzzy,
        }
    }

    /// Back-compat shim for tests that only have name sets and don't care
    /// about metadata. Wraps each name with `ModelMetadata::default()`.
    pub fn from_ids(providers: HashMap<String, BTreeSet<String>>) -> Self {
        let with_meta = providers
            .into_iter()
            .map(|(p, set)| {
                let map: BTreeMap<String, ModelMetadata> = set
                    .into_iter()
                    .map(|id| (id, ModelMetadata::default()))
                    .collect();
                (p, map)
            })
            .collect();
        Self::with_metadata(with_meta)
    }

    /// Legacy constructor: one-line alias for `from_ids` so existing
    /// `ModelIndex::new(...)` call sites compile unchanged.
    pub fn new(providers: HashMap<String, BTreeSet<String>>) -> Self {
        Self::from_ids(providers)
    }

    pub fn empty() -> Self {
        Self {
            metadata: HashMap::new(),
            fuzzy: HashMap::new(),
        }
    }

    pub fn resolve(&self, provider: &str, requested: &str) -> Option<&str> {
        let entries = self.fuzzy.get(provider)?;

        // Fast path: exact case-insensitive match avoids tokenize() allocation
        for entry in entries {
            if entry.normalized.eq_ignore_ascii_case(requested) {
                return Some(entry.original.as_str());
            }
        }

        let req_norm = requested.to_lowercase();
        let req_tokens = tokenize(requested);

        let mut best_score = THRESHOLD - f64::EPSILON;
        let mut best: Option<&ModelEntry> = None;

        for entry in entries {
            let s = score(&req_tokens, &entry.tokens, &req_norm, &entry.normalized);
            let dominated = match best {
                None => s >= THRESHOLD,
                Some(b) => {
                    s > best_score
                        || (s == best_score
                            && (entry.original.len() < b.original.len()
                                || (entry.original.len() == b.original.len()
                                    && entry.original < b.original)))
                }
            };
            if dominated {
                best_score = s;
                best = Some(entry);
            }
        }

        best.map(|e| e.original.as_str())
    }

    /// Look up upstream metadata for an exact `(provider, model_id)` pair.
    /// Returns `None` if either the provider or the model is unknown. The
    /// `model_id` must be the canonical form discovered from upstream — call
    /// `resolve` first if you only have a fuzzy alias.
    pub fn metadata(&self, provider: &str, model: &str) -> Option<&ModelMetadata> {
        self.metadata.get(provider)?.get(model)
    }

    /// Enumerate all `(model_id, metadata)` pairs for one provider, ordered
    /// by model id (`BTreeMap` iteration order). Used by catalog builders.
    pub fn provider_models(&self, provider: &str) -> impl Iterator<Item = (&str, &ModelMetadata)> {
        self.metadata
            .get(provider)
            .into_iter()
            .flat_map(|map| map.iter().map(|(k, v)| (k.as_str(), v)))
    }

    /// Enumerate all providers stored in the index. Useful when catalogs
    /// need to walk every provider without a-priori knowledge of names.
    pub fn providers(&self) -> impl Iterator<Item = &str> {
        self.metadata.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn index_with(provider: &str, models: &[&str]) -> ModelIndex {
        let set: BTreeSet<String> = models.iter().map(|s| s.to_string()).collect();
        let mut map = HashMap::new();
        map.insert(provider.to_string(), set);
        ModelIndex::new(map)
    }

    #[test]
    fn tokenize_splits_on_delimiters() {
        assert_eq!(
            tokenize("claude-sonnet-4-5-20250514"),
            vec!["claude", "sonnet", "4", "5", "20250514"]
        );
        assert_eq!(tokenize("gpt-4o-mini"), vec!["gpt", "4o", "mini"]);
        assert_eq!(
            tokenize("Qwen/Qwen3-235B-A22B"),
            vec!["qwen", "qwen3", "235b", "a22b"]
        );
        assert_eq!(tokenize("deepseek_chat"), vec!["deepseek", "chat"]);
        assert_eq!(tokenize("model.v2.1"), vec!["model", "v2", "1"]);
    }

    #[test]
    fn exact_match_case_insensitive() {
        let idx = index_with("p", &["gpt-4o", "gpt-4o-mini"]);
        assert_eq!(idx.resolve("p", "gpt-4o"), Some("gpt-4o"));
        assert_eq!(idx.resolve("p", "GPT-4o"), Some("gpt-4o"));
    }

    #[test]
    fn prefix_match_finds_dated_variant() {
        let idx = index_with("p", &["claude-sonnet-4-5-20250514"]);
        assert_eq!(
            idx.resolve("p", "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5-20250514")
        );
    }

    #[test]
    fn prefix_match_prefers_shorter_canonical() {
        let idx = index_with("p", &["claude-sonnet-4-5", "claude-sonnet-4-5-20250514"]);
        assert_eq!(
            idx.resolve("p", "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5")
        );
    }

    #[test]
    fn unrelated_model_returns_none() {
        let idx = index_with("p", &["gpt-4o", "gpt-4o-mini"]);
        assert_eq!(idx.resolve("p", "llama-3-70b"), None);
    }

    #[test]
    fn unknown_provider_returns_none() {
        let idx = index_with("copilot", &["gpt-4o"]);
        assert_eq!(idx.resolve("deepseek", "gpt-4o"), None);
    }

    #[test]
    fn empty_index_returns_none() {
        let idx = ModelIndex::empty();
        assert_eq!(idx.resolve("p", "gpt-4o"), None);
    }

    #[test]
    fn token_overlap_selects_best_match() {
        let idx = index_with(
            "p",
            &["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-3-5"],
        );
        assert_eq!(idx.resolve("p", "claude-opus-4-5"), Some("claude-opus-4-5"));
        assert_eq!(
            idx.resolve("p", "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5")
        );
    }

    #[test]
    fn tie_breaking_prefers_shorter_then_alphabetical() {
        let idx = index_with("p", &["model-b", "model-a"]);
        assert_eq!(idx.resolve("p", "model"), Some("model-a"));
    }

    #[test]
    fn prefix_match_prefers_more_specific() {
        let idx = index_with(
            "p",
            &[
                "claude-sonnet-4",
                "claude-sonnet-4-5",
                "claude-sonnet-4-5-20250514",
            ],
        );
        assert_eq!(
            idx.resolve("p", "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5")
        );
    }

    proptest! {
        #[test]
        fn exact_match_always_wins(model in "[a-z][a-z0-9-]{1,30}") {
            let idx = index_with("p", &[&model, "unrelated-model-xyz"]);
            let result = idx.resolve("p", &model);
            prop_assert_eq!(result, Some(model.as_str()));
        }
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use agent_shim_core::ModelMetadata;

    fn metadata_with_ctx(ctx: u32) -> ModelMetadata {
        ModelMetadata {
            context_window_tokens: Some(ctx),
            ..Default::default()
        }
    }

    #[test]
    fn metadata_lookup_returns_stored_value() {
        let mut p1: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        p1.insert("gpt-5.5".into(), metadata_with_ctx(1_050_000));
        let mut all = HashMap::new();
        all.insert("copilot".into(), p1);
        let idx = ModelIndex::with_metadata(all);
        let m = idx.metadata("copilot", "gpt-5.5").expect("present");
        assert_eq!(m.context_window_tokens, Some(1_050_000));
    }

    #[test]
    fn metadata_lookup_returns_none_for_unknown_model() {
        let mut p1: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        p1.insert("gpt-5.5".into(), metadata_with_ctx(1_050_000));
        let mut all = HashMap::new();
        all.insert("copilot".into(), p1);
        let idx = ModelIndex::with_metadata(all);
        assert!(idx.metadata("copilot", "gpt-99999").is_none());
    }

    #[test]
    fn metadata_lookup_returns_none_for_unknown_provider() {
        let idx = ModelIndex::with_metadata(HashMap::new());
        assert!(idx.metadata("copilot", "x").is_none());
    }

    #[test]
    fn provider_models_iterates_in_btreemap_order() {
        let mut p1: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        p1.insert("a".into(), Default::default());
        p1.insert("c".into(), Default::default());
        p1.insert("b".into(), Default::default());
        let mut all = HashMap::new();
        all.insert("copilot".into(), p1);
        let idx = ModelIndex::with_metadata(all);
        let names: Vec<_> = idx
            .provider_models("copilot")
            .map(|(n, _)| n.to_string())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn provider_models_unknown_provider_yields_empty() {
        let idx = ModelIndex::with_metadata(HashMap::new());
        let names: Vec<_> = idx.provider_models("nope").collect();
        assert!(names.is_empty());
    }

    #[test]
    fn providers_lists_all_registered_providers() {
        let mut all = HashMap::new();
        all.insert("a".to_string(), BTreeMap::new());
        all.insert("b".to_string(), BTreeMap::new());
        let idx = ModelIndex::with_metadata(all);
        let mut names: Vec<_> = idx.providers().map(String::from).collect();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn with_metadata_resolves_fuzzy_against_names() {
        let mut p1: BTreeMap<String, ModelMetadata> = BTreeMap::new();
        p1.insert("claude-sonnet-4-5-20250514".into(), Default::default());
        let mut all = HashMap::new();
        all.insert("copilot".into(), p1);
        let idx = ModelIndex::with_metadata(all);
        assert_eq!(
            idx.resolve("copilot", "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5-20250514")
        );
    }

    #[test]
    fn from_ids_still_works_for_existing_callers() {
        let mut set = BTreeSet::new();
        set.insert("foo".to_string());
        let mut all = HashMap::new();
        all.insert("p".into(), set);
        let idx = ModelIndex::from_ids(all);
        assert_eq!(idx.resolve("p", "foo"), Some("foo"));
        // from_ids fills in default metadata, so the model is queryable.
        let m = idx.metadata("p", "foo").expect("present");
        assert_eq!(m, &ModelMetadata::default());
    }
}
