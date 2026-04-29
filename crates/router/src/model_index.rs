use std::collections::{BTreeSet, HashMap};

struct ModelEntry {
    original: String,
    normalized: String,
    tokens: Vec<String>,
}

pub struct ModelIndex {
    providers: HashMap<String, Vec<ModelEntry>>,
}

fn tokenize(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| c == '-' || c == '_' || c == '.' || c == '/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn score(requested_tokens: &[String], candidate_tokens: &[String], req_norm: &str, cand_norm: &str) -> f64 {
    if req_norm == cand_norm {
        return 1.0;
    }

    if req_norm.starts_with(cand_norm) || cand_norm.starts_with(req_norm) {
        let shorter = req_norm.len().min(cand_norm.len()) as f64;
        let longer = req_norm.len().max(cand_norm.len()) as f64;
        return 0.8 + 0.2 * (shorter / longer);
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

const THRESHOLD: f64 = 0.4;

impl ModelIndex {
    pub fn new(discovered: HashMap<String, BTreeSet<String>>) -> Self {
        let providers = discovered
            .into_iter()
            .map(|(provider, models)| {
                let entries = models
                    .into_iter()
                    .map(|name| {
                        let normalized = name.to_lowercase();
                        let tokens = tokenize(&name);
                        ModelEntry { original: name, normalized, tokens }
                    })
                    .collect();
                (provider, entries)
            })
            .collect();
        Self { providers }
    }

    pub fn empty() -> Self {
        Self { providers: HashMap::new() }
    }

    pub fn resolve(&self, provider: &str, requested: &str) -> Option<&str> {
        let entries = self.providers.get(provider)?;
        let req_norm = requested.to_lowercase();
        let req_tokens = tokenize(requested);

        let mut best_score = 0.0_f64;
        let mut best: Option<&ModelEntry> = None;

        for entry in entries {
            let s = score(&req_tokens, &entry.tokens, &req_norm, &entry.normalized);
            if s > best_score
                || (s == best_score
                    && best.map_or(true, |b| {
                        entry.original.len() < b.original.len()
                            || (entry.original.len() == b.original.len()
                                && entry.original < b.original)
                    }))
            {
                best_score = s;
                best = Some(entry);
            }
        }

        if best_score >= THRESHOLD {
            best.map(|e| e.original.as_str())
        } else {
            None
        }
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
        assert_eq!(tokenize("claude-sonnet-4-5-20250514"), vec!["claude", "sonnet", "4", "5", "20250514"]);
        assert_eq!(tokenize("gpt-4o-mini"), vec!["gpt", "4o", "mini"]);
        assert_eq!(tokenize("Qwen/Qwen3-235B-A22B"), vec!["qwen", "qwen3", "235b", "a22b"]);
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
        assert_eq!(idx.resolve("p", "claude-sonnet-4-5"), Some("claude-sonnet-4-5-20250514"));
    }

    #[test]
    fn prefix_match_prefers_shorter_canonical() {
        let idx = index_with("p", &["claude-sonnet-4-5", "claude-sonnet-4-5-20250514"]);
        assert_eq!(idx.resolve("p", "claude-sonnet-4-5"), Some("claude-sonnet-4-5"));
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
        let idx = index_with("p", &["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-3-5"]);
        assert_eq!(idx.resolve("p", "claude-opus-4-5"), Some("claude-opus-4-5"));
        assert_eq!(idx.resolve("p", "claude-sonnet-4-5"), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn tie_breaking_prefers_shorter_then_alphabetical() {
        let idx = index_with("p", &["model-b", "model-a"]);
        assert_eq!(idx.resolve("p", "model"), Some("model-a"));
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
