use crate::schema::{BucketConfigYaml, GatewayConfig, Tier, UpstreamConfig};
use crate::upstream_accessors::{upstream_cost, upstream_tier};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("port cannot be 0")]
    ZeroPort,
    #[error("route references unknown upstream: {0}")]
    UnknownUpstream(String),
    #[error("duplicate route alias (frontend={0}, model={1})")]
    DuplicateAlias(String, String),
    #[error("unknown frontend protocol: {0} (must be 'anthropic_messages', 'openai_chat', or 'openai_responses')")]
    UnknownFrontend(String),
    #[error("upstream {0}: {1}")]
    InvalidUpstream(String, String),
    #[error("invalid route: {0}")]
    InvalidRoute(String),
    #[error("admin.port {admin} collides with server.port {server} on bind {bind}")]
    PortCollision {
        admin: u16,
        server: u16,
        bind: String,
    },
    /// Rule 15 (Plan 06 P03): cost.* fields must both be present or both
    /// absent. `Option<UpstreamCost>` already enforces this at the schema
    /// layer for the normal parse path; this variant exists for the
    /// hand-rolled / partial-parse path described in spec §5.5.
    #[error(
        "upstream `{upstream}` declares partial `cost` block; both `input_per_million_usd` and `output_per_million_usd` are required"
    )]
    IncompleteCost { upstream: String },
    /// Rule 15 (Plan 06 P03): cost.* fields must be non-negative.
    #[error("upstream `{upstream}` has negative `cost.{field}` (must be >= 0)")]
    NegativeCost {
        upstream: String,
        field: &'static str,
    },
    /// Rule 16 (Plan 06 P03): tier must be one of economy/standard/premium.
    /// Schema-level enum normally catches this; the variant exists to give
    /// operators a clearer error string on hand-crafted invalid YAML.
    #[error("upstream `{upstream}` has invalid tier `{value}` (must be economy/standard/premium)")]
    InvalidTier { upstream: String, value: String },
    /// Rule 17 (Plan 06 P03): a route's `min_tier` must be satisfied by
    /// at least one upstream in its chain — otherwise the route would
    /// always 503 with `NoEligibleUpstream`.
    #[error(
        "route `{route}` requires min_tier={min_tier:?} but no upstream in its chain meets it (chain tiers: {chain_tiers:?})"
    )]
    ImpossibleMinTier {
        route: String,
        min_tier: crate::schema::Tier,
        chain_tiers: Vec<(String, crate::schema::Tier)>,
    },

    /// Layer A rule (Plan 07 P03): a route references a plugin name
    /// that isn't declared under top-level `plugins:`. Phase 7 spec §5.3.
    #[error(
        "route `{route}` references plugin `{plugin}` on hook `{hook}`, but no \
         plugin with that name is declared under top-level `plugins:`"
    )]
    UndeclaredPlugin {
        route: String,
        plugin: String,
        hook: &'static str,
    },

    /// Layer A rule (Plan 07 P03): a plugin sets `timeout_ms: 0`,
    /// which is rejected as misconfiguration (a zero timeout would
    /// cause every invocation to time out immediately). Phase 7 spec §5.3.
    #[error(
        "plugin `{plugin}` has `timeout_ms: 0` on slot `{slot}` (rejected as \
         misconfiguration)"
    )]
    ZeroTimeoutMs { plugin: String, slot: &'static str },

    /// Layer A rule (Plan 07 P03): duplicate plugin names. YAML map
    /// key uniqueness usually catches this at parse time, but
    /// hand-constructed `GatewayConfig` values (test fixtures) may
    /// still slip through. Phase 7 spec §5.3.
    #[error("duplicate plugin name: `{0}`")]
    DuplicatePluginName(String),
}

/// What "baseline" means for reload validation. Built from the running
/// `AppCore` and passed to [`validate_for_reload`]. Spec §5.5 rules
/// 11-14.
#[derive(Debug, Clone)]
pub struct ReloadBaseline {
    /// Names of upstreams declared at startup. Reload may not add or
    /// remove entries (rule 12).
    pub upstream_names: std::collections::BTreeSet<String>,
    /// Server bind/port at startup; immutable across reload (rule 13).
    pub server: crate::schema::ServerConfig,
    /// Admin block at startup; immutable across reload (rule 13).
    pub admin: Option<crate::schema::AdminConfig>,
    /// OTel endpoint at startup; immutable across reload (rule 14).
    pub otel_endpoint: Option<String>,
}

/// Summary returned on successful validation. Used to render the
/// /admin/reload response (spec §5.6).
#[derive(Debug, Clone, Default)]
pub struct ReloadDiff {
    pub routes_total: usize,
    pub routes_added: usize,
    pub routes_removed: usize,
    pub routes_modified: usize,
    pub policies_changed: Vec<String>,
    pub auth_keys_added: usize,
    pub auth_keys_removed: usize,
    pub warnings: Vec<String>,
}

/// New error variants used only by reload validation.
#[derive(Debug, thiserror::Error)]
pub enum ReloadValidationError {
    #[error("upstreams.* set changed: added={added:?}, removed={removed:?}")]
    UpstreamSetChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    #[error("immutable field changed: {field}: {old} → {new}")]
    ImmutableFieldChanged {
        field: &'static str,
        old: String,
        new: String,
    },
    #[error("startup validation error: {0}")]
    StartupError(#[from] ValidationError),
}

const VALID_FRONTENDS: &[&str] = &[
    "anthropic_messages",
    "anthropic",
    "openai_chat",
    "openai",
    "openai_responses",
    "responses",
];

/// Returns true if `name` resolves to a configured upstream block.
///
/// Direct lookup wins. As a legacy back-compat case (carried from v0.3),
/// the virtual name `"copilot"` resolves whenever any
/// `UpstreamConfig::GithubCopilot` block is configured, regardless of its key
/// in `cfg.upstreams`. Operators upgrading from v0.3 may have routes that say
/// `upstream: copilot` while the actual upstream entry is keyed
/// `github_copilot:` (or any other name).
///
/// Both [`validate`] and [`validate_routes`] go through this helper so the two
/// never disagree on what counts as "configured".
fn upstream_is_configured(cfg: &GatewayConfig, name: &str) -> bool {
    if cfg.upstreams.contains_key(name) {
        return true;
    }
    if name == "copilot"
        && cfg
            .upstreams
            .values()
            .any(|u| matches!(u, UpstreamConfig::GithubCopilot(_)))
    {
        return true;
    }
    false
}

// ── Plan 06 P03: cost-aware routing accessor helpers ────────────────────────
//
// Phase 6 cost-aware routing needs to read the new `tier` and `cost` fields
// without exhaustively matching every variant at every call site. The
// canonical helpers live in `crate::upstream_accessors` (re-exported from
// the crate root) and are imported at the top of this module.

pub fn validate(cfg: &GatewayConfig) -> Result<(), ValidationError> {
    if cfg.server.port == 0 {
        return Err(ValidationError::ZeroPort);
    }

    if let Some(admin) = &cfg.admin {
        if admin.port == 0 {
            return Err(ValidationError::ZeroPort);
        }
        if admin.port == cfg.server.port && admin.bind == cfg.server.bind {
            return Err(ValidationError::PortCollision {
                admin: admin.port,
                server: cfg.server.port,
                bind: admin.bind.clone(),
            });
        }
    }

    if let Some(otel) = &cfg.otel {
        if !(0.0..=1.0).contains(&otel.sample_ratio) {
            return Err(ValidationError::InvalidRoute(format!(
                "otel.sample_ratio {} must be in [0.0, 1.0]",
                otel.sample_ratio
            )));
        }
    }

    // P05 §8: bound H7 flush deadline to a sensible upper limit so
    // misconfig can't stall shutdown forever.
    if cfg.shutdown.plugin_flush_secs > 300 {
        return Err(ValidationError::InvalidRoute(format!(
            "shutdown.plugin_flush_secs {} must be <= 300 seconds",
            cfg.shutdown.plugin_flush_secs
        )));
    }

    let mut seen = std::collections::HashSet::new();
    for route in &cfg.routes {
        if !VALID_FRONTENDS.contains(&route.frontend.as_str()) {
            return Err(ValidationError::UnknownFrontend(route.frontend.clone()));
        }
        // Collect every upstream name the route references, regardless of
        // whether it uses the singular or array shape. Validation of "exactly
        // one shape" lives in `validate_routes` below.
        let referenced: Vec<String> = if !route.upstreams.is_empty() {
            route.upstreams.iter().map(|u| u.name.clone()).collect()
        } else if let Some(name) = route.upstream.as_ref() {
            vec![name.clone()]
        } else {
            // No upstream configured at all — surfaced as a clearer error by
            // `validate_routes`. Skip the unknown-upstream check here.
            vec![]
        };
        for name in &referenced {
            if !upstream_is_configured(cfg, name) {
                return Err(ValidationError::UnknownUpstream(name.clone()));
            }
        }
        let key = (route.frontend.clone(), route.model.clone());
        if !seen.insert(key.clone()) {
            return Err(ValidationError::DuplicateAlias(key.0, key.1));
        }
    }

    for (name, upstream) in &cfg.upstreams {
        if let UpstreamConfig::Anthropic(a) = upstream {
            validate_oai_style_upstream(
                name,
                &a.base_url,
                a.api_key.expose(),
                a.request_timeout_secs,
            )?;
            if a.anthropic_version.is_empty() {
                return Err(ValidationError::InvalidUpstream(
                    name.clone(),
                    "anthropic_version must be non-empty".to_string(),
                ));
            }
        } else if let UpstreamConfig::Deepseek(d) = upstream {
            validate_oai_style_upstream(
                name,
                &d.base_url,
                d.api_key.expose(),
                d.request_timeout_secs,
            )?;
        } else if let UpstreamConfig::Gemini(g) = upstream {
            validate_oai_style_upstream(
                name,
                &g.base_url,
                g.api_key.expose(),
                g.request_timeout_secs,
            )?;
        }
    }

    // Phase 4 (Plan 04 P01) per-route validation. Wired into the canonical
    // entry point so `gateway::commands::serve`/`validate_config` (and any
    // other caller of `validate()`) get the new rules for free.
    validate_routes(cfg).map_err(ValidationError::InvalidRoute)?;

    // Phase 4 (Plan 04 P04 T3) auth + rate_limit validation. Reuses
    // `InvalidRoute` as the catch-all "config-shape error" tunnel so the
    // existing CLI surfaces these errors without churning the public enum.
    validate_rate_limit(cfg).map_err(ValidationError::InvalidRoute)?;
    validate_auth(cfg).map_err(ValidationError::InvalidRoute)?;

    // Rule 15 (Plan 06 P03): cost.* fields must be non-negative when
    // declared. `Option<UpstreamCost>` already guarantees both fields are
    // present-or-absent together at the schema layer — we just check the
    // sign here.
    for (name, upstream) in &cfg.upstreams {
        if let Some(cost) = upstream_cost(upstream) {
            if cost.input_per_million_usd < 0.0 {
                return Err(ValidationError::NegativeCost {
                    upstream: name.clone(),
                    field: "input_per_million_usd",
                });
            }
            if cost.output_per_million_usd < 0.0 {
                return Err(ValidationError::NegativeCost {
                    upstream: name.clone(),
                    field: "output_per_million_usd",
                });
            }
        }
    }

    // Rule 17 (Plan 06 P03): every route with `min_tier` set must have at
    // least one upstream in its chain that meets it. Otherwise the route is
    // guaranteed to produce 503 NoEligibleUpstream forever.
    for route in &cfg.routes {
        let Some(min_tier) = route.min_tier else {
            continue;
        };
        // Build the route's effective chain: prefer the array shape if
        // populated, else fall back to the singular shape.
        let chain_names: Vec<&str> = if !route.upstreams.is_empty() {
            route.upstreams.iter().map(|u| u.name.as_str()).collect()
        } else if let Some(u) = route.upstream.as_deref() {
            vec![u]
        } else {
            // No upstream declared — earlier rules already flag this.
            continue;
        };
        let chain_tiers: Vec<(String, Tier)> = chain_names
            .iter()
            .filter_map(|n| {
                // Honor the `copilot` virtual-name fallback (see
                // `upstream_is_configured`): if the literal key isn't in
                // `upstreams`, look for any GithubCopilot block.
                let upstream = cfg.upstreams.get(*n).or_else(|| {
                    if *n == "copilot" {
                        cfg.upstreams
                            .values()
                            .find(|u| matches!(u, UpstreamConfig::GithubCopilot(_)))
                    } else {
                        None
                    }
                });
                upstream.map(|u| ((*n).to_string(), upstream_tier(u)))
            })
            .collect();
        // If we couldn't resolve any chain entry, earlier validation will
        // surface the unknown-upstream error; don't double-fire here.
        if chain_tiers.is_empty() {
            continue;
        }
        let any_meets = chain_tiers.iter().any(|(_, t)| *t >= min_tier);
        if !any_meets {
            return Err(ValidationError::ImpossibleMinTier {
                route: format!("{}/{}", route.frontend, route.model),
                min_tier,
                chain_tiers,
            });
        }
    }

    // Layer A plugin validation (Plan 07 P03). Three rules:
    //   - undeclared plugin reference on a route hook
    //   - timeout_ms == 0
    //   - duplicate plugin names (defensive — YAML usually catches it)
    validate_plugins(cfg)?;

    Ok(())
}

/// Shared validation for OpenAI-style upstream configs. Verifies that the
/// `api_key` and `base_url` are non-empty, the `base_url` uses an http(s)
/// scheme, and the `request_timeout_secs` is greater than zero.
///
/// Anthropic-specific checks (e.g. `anthropic_version` non-empty) are handled
/// at the call site after this helper returns.
fn validate_oai_style_upstream(
    name: &str,
    base_url: &str,
    api_key: &str,
    timeout: u64,
) -> Result<(), ValidationError> {
    if api_key.is_empty() {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "api_key must be non-empty".to_string(),
        ));
    }
    if base_url.is_empty() {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "base_url must be non-empty".to_string(),
        ));
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "base_url must start with http:// or https://".to_string(),
        ));
    }
    if timeout == 0 {
        return Err(ValidationError::InvalidUpstream(
            name.to_string(),
            "request_timeout_secs must be greater than 0".to_string(),
        ));
    }
    Ok(())
}

/// Phase 4 (Plan 04 P01) per-route validation. Enforces:
/// 1. Each route uses **exactly one** upstream shape (singular OR array).
/// 2. Singular form requires both `upstream` and `upstream_model`.
/// 3. Every referenced upstream name resolves to a configured upstream
///    (via [`upstream_is_configured`], which honors the legacy `copilot`
///    virtual name).
/// 4. Retry policy fields are sane (`max_attempts >= 1`,
///    `total_budget_ms >= initial_backoff_ms`, `multiplier > 1.0` and finite).
/// 5. `retry.jitter_pct` is finite and within `0.0..=100.0` (negative or
///    non-finite values would crash `rand::gen_range` on every retry).
/// 6. Breaker policy fields are sane (`failure_threshold_pct` in `1..=100`,
///    `min_requests >= 1`).
///
/// Returns on first failure; rerun after each fix to surface subsequent issues.
///
/// Returns a human-readable `String` so config-load callers can surface the
/// message verbatim. The `validate()` entry point wraps these into
/// `ValidationError::InvalidRoute` and is the canonical config-load call site;
/// callers that already hold a `&GatewayConfig` may invoke `validate_routes`
/// directly.
pub fn validate_routes(cfg: &GatewayConfig) -> Result<(), String> {
    for (i, route) in cfg.routes.iter().enumerate() {
        let has_singular = route.upstream.is_some() || route.upstream_model.is_some();
        let has_array = !route.upstreams.is_empty();

        // Rule 1: exactly one shape per route (singular OR array, never both, never neither).
        match (has_singular, has_array) {
            (true, true) => {
                return Err(format!(
                    "route[{i}] '{}/{}' specifies both `upstream`/`upstream_model` and `upstreams`; pick one form",
                    route.frontend, route.model
                ));
            }
            (false, false) => {
                return Err(format!(
                    "route[{i}] '{}/{}' has no upstream configured; provide either `upstream`+`upstream_model` or `upstreams`",
                    route.frontend, route.model
                ));
            }
            (true, false) => {
                // Rule 2: singular form requires BOTH `upstream` and `upstream_model`.
                if route.upstream.is_none() || route.upstream_model.is_none() {
                    return Err(format!(
                        "route[{i}] '{}/{}' singular form requires both `upstream` and `upstream_model`",
                        route.frontend, route.model
                    ));
                }
            }
            (false, true) => {} // valid array form
        }

        // Rule 3: every referenced upstream name must be configured. Honors the
        // legacy `copilot` virtual name so v0.3 configs keep working — see
        // `upstream_is_configured` for the full back-compat rule.
        let upstream_names: Vec<&str> = if has_array {
            route.upstreams.iter().map(|u| u.name.as_str()).collect()
        } else {
            vec![route.upstream.as_deref().unwrap()]
        };
        for name in upstream_names {
            if !upstream_is_configured(cfg, name) {
                return Err(format!(
                    "route[{i}] '{}/{}' references unknown upstream '{}'",
                    route.frontend, route.model, name
                ));
            }
        }

        // Rule 4: retry policy sanity.
        if route.retry.max_attempts == 0 {
            return Err(format!("route[{i}] retry.max_attempts must be >= 1"));
        }
        if route.retry.total_budget_ms < route.retry.initial_backoff_ms {
            return Err(format!(
                "route[{i}] retry.total_budget_ms ({}) must be >= initial_backoff_ms ({})",
                route.retry.total_budget_ms, route.retry.initial_backoff_ms
            ));
        }
        if !route.retry.multiplier.is_finite() || route.retry.multiplier <= 1.0 {
            return Err(format!(
                "route[{i}] retry.multiplier ({}) must be > 1.0 for exponential backoff",
                route.retry.multiplier
            ));
        }

        // Rule 5: jitter_pct must be a finite, non-negative number <= 100.
        // (rand::gen_range panics on inverted/empty ranges, so a negative or
        // NaN jitter would crash the gateway on every retry attempt.)
        if !route.retry.jitter_pct.is_finite()
            || route.retry.jitter_pct < 0.0
            || route.retry.jitter_pct > 100.0
        {
            return Err(format!(
                "route[{i}] retry.jitter_pct ({}) must be in 0.0..=100.0 (finite, non-negative)",
                route.retry.jitter_pct
            ));
        }

        // Rule 6: breaker policy sanity.
        if !(1..=100).contains(&route.breaker.failure_threshold_pct) {
            return Err(format!(
                "route[{i}] breaker.failure_threshold_pct ({}) must be in 1..=100",
                route.breaker.failure_threshold_pct
            ));
        }
        if route.breaker.min_requests == 0 {
            return Err(format!("route[{i}] breaker.min_requests must be >= 1"));
        }
    }
    Ok(())
}

/// Phase 4 (Plan 04 P04 T3) `rate_limit` block validation. Implements
/// rules 7, 8, 9, and 10 from the §4.4 design:
///   7. `per_route` keys reference existing routes (`<frontend>/<model>`).
///   8. `per_upstream` keys reference configured upstreams.
///   9. `per_key.overrides` keys must start with `sha256:` (no plaintext keys).
///  10. Every bucket has `rate_per_sec > 0` AND `burst >= 1`.
///
/// Returns a human-readable `String` so the canonical `validate()` entry point
/// can surface the message verbatim through `ValidationError::InvalidRoute`.
///
/// Short-circuits on `rate_limit.enabled = false`: if rate limiting is off
/// at the master switch, stale or partially-edited bucket entries should
/// not surface as hard config errors at startup. The fields are still
/// parsed and stored — they're just not validated against the route table
/// or bucket bounds. Operators who want stricter pre-validation should
/// flip `enabled: true`.
pub fn validate_rate_limit(cfg: &GatewayConfig) -> Result<(), String> {
    if !cfg.rate_limit.enabled {
        return Ok(());
    }

    // Rule 7: per_route keys reference existing routes.
    for key in cfg.rate_limit.per_route.keys() {
        let parts: Vec<&str> = key.splitn(2, '/').collect();
        if parts.len() != 2 {
            return Err(format!(
                "rate_limit.per_route key '{key}' must be '<frontend>/<model>'"
            ));
        }
        let exists = cfg
            .routes
            .iter()
            .any(|r| r.frontend == parts[0] && r.model == parts[1]);
        if !exists {
            return Err(format!(
                "rate_limit.per_route key '{key}' references non-existent route"
            ));
        }
    }

    // Rule 8: per_upstream keys reference configured upstreams.
    for key in cfg.rate_limit.per_upstream.keys() {
        if !cfg.upstreams.contains_key(key) {
            return Err(format!(
                "rate_limit.per_upstream key '{key}' references non-existent upstream"
            ));
        }
    }

    // Rule 9: per_key overrides are SHA-256 hashes (sha256:<64 lowercase hex>).
    for key in cfg.rate_limit.per_key.overrides.keys() {
        check_sha256_key(key, "rate_limit.per_key.overrides")?;
    }

    // Rule 10: every bucket has rate_per_sec > 0 AND burst >= 1.
    let validate_bucket = |b: &BucketConfigYaml, ctx: &str| -> Result<(), String> {
        if b.rate_per_sec == 0 {
            return Err(format!("{ctx} rate_per_sec must be > 0"));
        }
        if b.burst == 0 {
            return Err(format!("{ctx} burst must be >= 1"));
        }
        Ok(())
    };
    if let Some(b) = &cfg.rate_limit.per_key.default {
        validate_bucket(b, "rate_limit.per_key.default")?;
    }
    if let Some(b) = &cfg.rate_limit.per_key.anonymous {
        validate_bucket(b, "rate_limit.per_key.anonymous")?;
    }
    for (k, v) in &cfg.rate_limit.per_key.overrides {
        validate_bucket(v, &format!("rate_limit.per_key.overrides[{k}]"))?;
    }
    for (k, v) in &cfg.rate_limit.per_route {
        validate_bucket(v, &format!("rate_limit.per_route[{k}]"))?;
    }
    for (k, v) in &cfg.rate_limit.per_upstream {
        validate_bucket(v, &format!("rate_limit.per_upstream[{k}]"))?;
    }
    if cfg.rate_limit.per_ip.enabled {
        if cfg.rate_limit.per_ip.rate_per_sec == 0 {
            return Err("rate_limit.per_ip.rate_per_sec must be > 0".into());
        }
        if cfg.rate_limit.per_ip.burst == 0 {
            return Err("rate_limit.per_ip.burst must be >= 1".into());
        }
    }

    Ok(())
}

/// Phase 4 (Plan 04 P04 T3) `auth` block validation. Enforces:
///   - `auth.required=true` requires `auth.enabled=true`.
///   - Every `auth.keys` key must be a `sha256:<64 lowercase hex>` value
///     (catches plaintext-key paste mistakes AND typo-shaped hashes).
pub fn validate_auth(cfg: &GatewayConfig) -> Result<(), String> {
    if cfg.auth.required && !cfg.auth.enabled {
        return Err("auth.required=true requires auth.enabled=true".into());
    }
    for key in cfg.auth.keys.keys() {
        check_sha256_key(key, "auth.keys")?;
    }
    Ok(())
}

/// Validate that `key` is shaped `sha256:<64 lowercase hex>`.
///
/// Operators paste these from the recipe in `docs/resilience.md`
/// (`echo -n key | sha256sum | awk '{print "sha256:" $1}'`). We catch
/// three classes of paste mistake here:
///   - Missing `sha256:` prefix (looks like a plaintext API key).
///   - Wrong hex length (truncated or extra chars).
///   - Non-hex characters in the hash (e.g. `sha256:nothex...`).
fn check_sha256_key(key: &str, ctx: &str) -> Result<(), String> {
    let hex = match key.strip_prefix("sha256:") {
        Some(h) => h,
        None => {
            return Err(format!(
                "{ctx} key '{key}' must start with 'sha256:'; \
                 do not paste plaintext API keys here"
            ));
        }
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "{ctx} key '{key}' has malformed hash; expected 'sha256:' \
             followed by 64 lowercase hex characters"
        ));
    }
    Ok(())
}

/// Layer A plugin validation (Plan 07 P03, spec §5.3). Three rules:
///
/// 1. Every plugin name referenced on a route hook is declared
///    under top-level `plugins:`.
/// 2. No `timeout_ms` slot is zero.
/// 3. (Defensive) No duplicate plugin names. YAML map-key
///    uniqueness catches this at parse time; this branch only fires
///    on hand-constructed `GatewayConfig` test fixtures.
///
/// Layer B (unknown plugin kind, factory `instantiate` failure, hook
/// subscription mismatch) lives in the gateway boot path — see P06.
pub fn validate_plugins(cfg: &GatewayConfig) -> Result<(), ValidationError> {
    // Rule 3: duplicate name (defensive against hand-constructed
    // configs that bypass YAML map-key uniqueness). For a `BTreeMap`
    // this can't actually trigger from YAML; left here so an
    // intentionally-crafted test fixture can't sneak past.
    let mut seen = std::collections::HashSet::new();
    for name in cfg.plugins.keys() {
        if !seen.insert(name.clone()) {
            return Err(ValidationError::DuplicatePluginName(name.clone()));
        }
    }

    // Rule 2: timeout_ms == 0.
    for (name, entry) in &cfg.plugins {
        if let Some(t) = &entry.timeout_ms {
            match t {
                crate::plugins::TimeoutMs::Uniform(0) => {
                    return Err(ValidationError::ZeroTimeoutMs {
                        plugin: name.clone(),
                        slot: "uniform",
                    });
                }
                crate::plugins::TimeoutMs::Uniform(_) => {}
                crate::plugins::TimeoutMs::PerHook {
                    default,
                    on_decoded_request,
                    on_resolved,
                    on_stream_event,
                    on_response_complete,
                } => {
                    for (slot, value) in [
                        ("default", default),
                        ("on_decoded_request", on_decoded_request),
                        ("on_resolved", on_resolved),
                        ("on_stream_event", on_stream_event),
                        ("on_response_complete", on_response_complete),
                    ] {
                        if let Some(0) = value {
                            return Err(ValidationError::ZeroTimeoutMs {
                                plugin: name.clone(),
                                slot,
                            });
                        }
                    }
                }
            }
        }
    }

    // Rule 1: undeclared plugin reference on any route hook.
    for route in &cfg.routes {
        let Some(plugins_block) = &route.plugins else {
            continue;
        };
        let route_label = format!("{}/{}", route.frontend, route.model);
        for (hook, plugin_name) in plugins_block.iter_references() {
            if !cfg.plugins.contains_key(plugin_name) {
                return Err(ValidationError::UndeclaredPlugin {
                    route: route_label.clone(),
                    plugin: plugin_name.to_string(),
                    hook,
                });
            }
        }
    }

    Ok(())
}

/// Reload-time validation. Called BY the reload path only; startup uses
/// [`validate`]. Applies rules 1-10 (via `validate`) plus rules 11-14:
///
/// - Rule 11: every route's upstream is declared in the baseline.
/// - Rule 12: candidate.upstreams keys equal baseline.upstream_names.
/// - Rule 13: server.* and admin.* fields equal baseline values.
/// - Rule 14: otel.endpoint equals baseline.otel_endpoint.
pub fn validate_for_reload(
    candidate: &crate::schema::GatewayConfig,
    baseline: &ReloadBaseline,
) -> Result<ReloadDiff, ReloadValidationError> {
    // Rule 13: immutable server/admin
    if candidate.server.bind != baseline.server.bind {
        return Err(ReloadValidationError::ImmutableFieldChanged {
            field: "server.bind",
            old: baseline.server.bind.clone(),
            new: candidate.server.bind.clone(),
        });
    }
    if candidate.server.port != baseline.server.port {
        return Err(ReloadValidationError::ImmutableFieldChanged {
            field: "server.port",
            old: baseline.server.port.to_string(),
            new: candidate.server.port.to_string(),
        });
    }
    match (&baseline.admin, &candidate.admin) {
        (Some(b), Some(c)) => {
            if b.bind != c.bind {
                return Err(ReloadValidationError::ImmutableFieldChanged {
                    field: "admin.bind",
                    old: b.bind.clone(),
                    new: c.bind.clone(),
                });
            }
            if b.port != c.port {
                return Err(ReloadValidationError::ImmutableFieldChanged {
                    field: "admin.port",
                    old: b.port.to_string(),
                    new: c.port.to_string(),
                });
            }
        }
        (None, Some(_)) => {
            return Err(ReloadValidationError::ImmutableFieldChanged {
                field: "admin",
                old: "absent".into(),
                new: "present".into(),
            });
        }
        (Some(_), None) => {
            return Err(ReloadValidationError::ImmutableFieldChanged {
                field: "admin",
                old: "present".into(),
                new: "absent".into(),
            });
        }
        (None, None) => {}
    }

    // Rule 14: immutable otel.endpoint (other otel.* fields can change)
    let candidate_endpoint = candidate.otel.as_ref().and_then(|o| o.endpoint.clone());
    if candidate_endpoint != baseline.otel_endpoint {
        return Err(ReloadValidationError::ImmutableFieldChanged {
            field: "otel.endpoint",
            old: format!("{:?}", baseline.otel_endpoint),
            new: format!("{:?}", candidate_endpoint),
        });
    }

    // Rule 12: upstream set unchanged
    let candidate_names: std::collections::BTreeSet<String> =
        candidate.upstreams.keys().cloned().collect();
    let added: Vec<String> = candidate_names
        .difference(&baseline.upstream_names)
        .cloned()
        .collect();
    let removed: Vec<String> = baseline
        .upstream_names
        .difference(&candidate_names)
        .cloned()
        .collect();
    if !added.is_empty() || !removed.is_empty() {
        return Err(ReloadValidationError::UpstreamSetChanged { added, removed });
    }

    // Rules 1-10 (and Rule 11 implicitly via the existing
    // `validate_routes`/`UnknownUpstream` check).
    validate(candidate)?;

    // Build a minimal diff. Detailed diffing (which routes were added/
    // removed) is left as a future improvement; for v0.5 we report the
    // route count only.
    let diff = ReloadDiff {
        routes_total: candidate.routes.len(),
        ..ReloadDiff::default()
    };
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;
    use crate::secrets::Secret;
    use std::collections::BTreeMap;

    fn minimal_config() -> GatewayConfig {
        GatewayConfig {
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
            upstreams: BTreeMap::new(),
            routes: vec![],
            plugins: BTreeMap::new(),
            auth: AuthConfig::default(),
            rate_limit: RateLimitConfig::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
        }
    }

    #[test]
    fn valid_config_passes() {
        let cfg = minimal_config();
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn zero_port_fails() {
        let mut cfg = minimal_config();
        cfg.server.port = 0;
        assert!(matches!(validate(&cfg), Err(ValidationError::ZeroPort)));
    }

    #[test]
    fn unknown_upstream_fails() {
        let mut cfg = minimal_config();
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("nonexistent".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![],
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::UnknownUpstream(_))
        ));
    }

    #[test]
    fn duplicate_alias_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "upstream1".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "https://api.openai.com".to_string(),
                api_key: Secret::new("key"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("upstream1".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![],
        });
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("upstream1".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![],
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::DuplicateAlias(_, _))
        ));
    }

    #[test]
    fn unknown_frontend_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "upstream1".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "https://api.openai.com".to_string(),
                api_key: Secret::new("key"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        cfg.routes.push(RouteEntry {
            frontend: "unknown_protocol".to_string(),
            model: "gpt-4".to_string(),
            upstream: Some("upstream1".to_string()),
            upstream_model: Some("gpt-4".to_string()),
            upstreams: vec![],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![],
        });
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::UnknownFrontend(_))
        ));
    }

    #[test]
    fn anthropic_upstream_validation_passes() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "anthropic".to_string(),
            UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: Secret::new("sk-ant-test"),
                anthropic_version: "2023-06-01".to_string(),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn anthropic_upstream_empty_api_key_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "anthropic".to_string(),
            UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: Secret::new(""),
                anthropic_version: "2023-06-01".to_string(),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }

    #[test]
    fn anthropic_upstream_bad_base_url_fails() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "anthropic".to_string(),
            UpstreamConfig::Anthropic(AnthropicUpstream {
                base_url: "ftp://api.anthropic.com".to_string(),
                api_key: Secret::new("sk-ant-test"),
                anthropic_version: "2023-06-01".to_string(),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidUpstream(_, _))
        ));
    }

    #[test]
    fn deepseek_upstream_validation_passes() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key: Secret::new("sk-deepseek-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn deepseek_validation_rejects_empty_api_key() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key: Secret::new(""),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "deepseek");
                assert!(
                    msg.contains("api_key"),
                    "expected api_key error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn deepseek_validation_rejects_bad_base_url() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "ftp://api.deepseek.com/v1".to_string(),
                api_key: Secret::new("sk-deepseek-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "deepseek");
                assert!(
                    msg.contains("base_url"),
                    "expected base_url error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn deepseek_validation_rejects_zero_timeout() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "deepseek".to_string(),
            UpstreamConfig::Deepseek(DeepseekUpstream {
                base_url: "https://api.deepseek.com/v1".to_string(),
                api_key: Secret::new("sk-deepseek-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 0,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "deepseek");
                assert!(
                    msg.contains("request_timeout_secs"),
                    "expected request_timeout_secs error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn gemini_upstream_validation_passes() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new("ai-studio-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn gemini_validation_rejects_empty_api_key() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new(""),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "gemini");
                assert!(
                    msg.contains("api_key"),
                    "expected api_key error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn gemini_validation_rejects_bad_base_url() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "ftp://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new("ai-studio-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "gemini");
                assert!(
                    msg.contains("base_url"),
                    "expected base_url error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    #[test]
    fn gemini_validation_rejects_zero_timeout() {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "gemini".to_string(),
            UpstreamConfig::Gemini(GeminiUpstream {
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key: Secret::new("ai-studio-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 0,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidUpstream(name, msg)) => {
                assert_eq!(name, "gemini");
                assert!(
                    msg.contains("request_timeout_secs"),
                    "expected request_timeout_secs error, got: {msg}"
                );
            }
            other => panic!("expected InvalidUpstream, got {other:?}"),
        }
    }

    // ── Plan 04 P01 T1: validate_routes coverage ────────────────────────────

    /// Build a `GatewayConfig` with a single configured upstream `openai` and
    /// one route parsed from the supplied YAML snippet. The helper backs the
    /// 5 validate_routes test cases below.
    fn make_cfg_with_route_yaml(route_yaml: &str) -> GatewayConfig {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "openai".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "https://api.openai.com".to_string(),
                api_key: Secret::new("sk-test"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        let route: RouteEntry = serde_yaml::from_str(route_yaml).expect("route YAML parses");
        cfg.routes.push(route);
        cfg
    }

    #[test]
    fn rejects_route_with_both_shapes() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            upstreams:
              - {name: copilot, model: gpt-4o}
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("specifies both"));
    }

    #[test]
    fn rejects_route_with_no_upstream() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("no upstream configured"));
    }

    #[test]
    fn rejects_unknown_upstream_reference() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: nonexistent, model: gpt-4o}
        "#,
        );
        // make_cfg_with_route_yaml configures only "openai" in cfg.upstreams.
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn accepts_well_formed_array_route() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstreams:
              - {name: openai, model: gpt-4o-2024-11-20}
        "#,
        );
        validate_routes(&cfg).expect("valid config");
    }

    #[test]
    fn rejects_multiplier_le_one() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              multiplier: 1.0
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("multiplier"));
    }

    /// Wiring regression for review-issue #1: an invalid YAML must be rejected
    /// by the canonical `validate()` entry point — not just by direct
    /// `validate_routes` calls. Without this, `agent-shim serve --config` would
    /// happily accept multiplier=1.0 in production.
    #[test]
    fn validate_rejects_invalid_route_via_canonical_entry() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              multiplier: 1.0
        "#,
        );
        match validate(&cfg) {
            Err(ValidationError::InvalidRoute(msg)) => {
                assert!(msg.contains("multiplier"), "got: {msg}");
            }
            other => panic!("expected InvalidRoute, got {other:?}"),
        }
    }

    /// Regression for review-issue #3 (copilot virtual-name consistency):
    /// `validate_routes` must accept `upstream: copilot` whenever any
    /// `github_copilot` upstream block is configured, matching the legacy
    /// behavior of `validate()`. Both validators share `upstream_is_configured`.
    #[test]
    fn validate_routes_accepts_copilot_virtual_name() {
        let mut cfg = minimal_config();
        // Upstream block is keyed `github_copilot` (not `copilot`).
        cfg.upstreams.insert(
            "github_copilot".to_string(),
            UpstreamConfig::GithubCopilot(GithubCopilotUpstream {
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        let route: RouteEntry = serde_yaml::from_str(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: copilot
            upstream_model: gpt-4o
        "#,
        )
        .expect("route YAML parses");
        cfg.routes.push(route);
        validate_routes(&cfg).expect("copilot virtual name should resolve");
        // And the canonical entry point agrees.
        validate(&cfg).expect("validate() should also pass");
    }

    // ── Plan 04 P01 T2 followup: jitter_pct + NaN multiplier rules ──────────

    #[test]
    fn rejects_negative_jitter_pct() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              jitter_pct: -25.0
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("jitter_pct"));
    }

    #[test]
    fn rejects_jitter_pct_above_100() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              jitter_pct: 150.0
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("jitter_pct"));
    }

    #[test]
    fn rejects_nan_multiplier() {
        let cfg = make_cfg_with_route_yaml(
            r#"
            frontend: openai_chat
            model: gpt-4o
            upstream: openai
            upstream_model: gpt-4o
            retry:
              multiplier: .nan
        "#,
        );
        let err = validate_routes(&cfg).unwrap_err();
        assert!(err.contains("multiplier"));
    }

    // ── Plan 04 P04 T3: rate_limit + auth validation ────────────────────────

    #[test]
    fn rejects_per_route_unknown_route() {
        // NOTE: schema rename quirk — the OpenAiCompatible variant deserializes
        // as `open_ai_compatible` (snake_case'd from `OpenAiCompatible`), not
        // `openai_compatible`. The plan's YAML had the wrong tag; corrected
        // here so the YAML parses far enough to exercise the rate_limit rule.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "https://x", api_key: "x", tier: standard}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: true
  per_route:
    "anthropic_messages/claude-opus-4-7": {rate_per_sec: 10, burst: 30}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(validate_rate_limit(&cfg).is_err());
    }

    #[test]
    fn rejects_per_key_override_without_sha256_prefix() {
        let cfg_yaml = r#"
rate_limit:
  enabled: true
  per_key:
    overrides:
      "plaintext-key": {rate_per_sec: 100, burst: 300}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_rate_limit(&cfg).unwrap_err();
        assert!(err.contains("sha256:"));
    }

    #[test]
    fn rejects_zero_burst() {
        let cfg_yaml = r#"
rate_limit:
  enabled: true
  per_key:
    default: {rate_per_sec: 10, burst: 0}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(validate_rate_limit(&cfg).is_err());
    }

    #[test]
    fn rejects_required_without_enabled() {
        let cfg_yaml = r#"
auth:
  enabled: false
  required: true
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(validate_auth(&cfg).is_err());
    }

    // ── Followup tests (rule-8 coverage + sha256 format + disabled bypass +
    //     v0.3 back-compat) — added in the P04 T3 followup ──

    #[test]
    fn rejects_per_upstream_unknown_upstream() {
        // Rule 8: per_upstream key must reference an upstream that exists.
        // The plan has rule-7 coverage (per_route → unknown route) but no
        // companion test for per_upstream, leaving the rule provable only
        // by inspection.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "http://x", api_key: "x", tier: standard}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: true
  per_upstream:
    "ghost-upstream": {rate_per_sec: 10, burst: 30}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_rate_limit(&cfg).unwrap_err();
        assert!(err.contains("ghost-upstream"));
        assert!(err.contains("non-existent upstream"));
    }

    #[test]
    fn rejects_auth_key_without_sha256_prefix() {
        // Auth.keys parallel to rule 9: a plaintext API key pasted into
        // auth.keys (instead of its hash) is the easy paste mistake to
        // catch. The plan tested validate_auth's required→enabled rule
        // but not the sha256: prefix path.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "plaintext-api-key":
      label: "alice"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_auth(&cfg).unwrap_err();
        assert!(err.contains("sha256:"));
    }

    #[test]
    fn rejects_sha256_with_wrong_hex_length() {
        // The original `starts_with("sha256:")` check accepted any prefix.
        // This pins the new format check: 64 lowercase hex chars after
        // the prefix.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "sha256:abc":
      label: "truncated"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_auth(&cfg).unwrap_err();
        assert!(err.contains("malformed hash"));
    }

    #[test]
    fn rejects_sha256_with_non_hex_chars() {
        // `sha256:nothex...` was previously accepted by the prefix-only
        // check. Now rejected.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "sha256:nothex01234567890123456789012345678901234567890123456789012345":
      label: "non-hex"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        let err = validate_auth(&cfg).unwrap_err();
        assert!(err.contains("malformed hash"));
    }

    #[test]
    fn well_formed_sha256_hash_passes() {
        // Positive case: a valid sha256:<64 lowercase hex> validates.
        // Without this, a regression that broke the happy path would
        // only be caught by the more end-to-end smoke tests.
        let cfg_yaml = r#"
auth:
  enabled: true
  keys:
    "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824":
      label: "alice"
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        validate_auth(&cfg).expect("well-formed sha256 hash must validate");
    }

    #[test]
    fn rate_limit_disabled_skips_all_validation() {
        // Master switch off → stale or partially-edited rate-limit fields
        // should NOT surface as hard config errors. Operators flipping
        // `enabled: false` to silence rate limiting in an emergency
        // shouldn't be forced to clean up the rest of the block at the
        // same time.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "http://x", api_key: "x", tier: standard}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
rate_limit:
  enabled: false
  per_route:
    "anthropic_messages/claude-opus-4-7": {rate_per_sec: 0, burst: 0}
  per_upstream:
    "ghost": {rate_per_sec: 10, burst: 30}
  per_key:
    overrides:
      "plaintext-key": {rate_per_sec: 100, burst: 300}
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        validate_rate_limit(&cfg).expect("disabled rate_limit must skip validation entirely");
    }

    #[test]
    fn v03_config_without_auth_or_rate_limit_blocks_parses() {
        // Direct v0.3 back-compat assertion: a config with neither block
        // must parse cleanly via #[serde(default)] AND pass validation.
        // Existing v0.3 fixtures cover this transitively, but the
        // assertion is fragile under future schema changes.
        let cfg_yaml = r#"
upstreams:
  oai: {type: open_ai_compatible, base_url: "http://x", api_key: "x", tier: standard}
routes:
  - frontend: openai_chat
    model: gpt-4o
    upstream: oai
    upstream_model: gpt-4o
    "#;
        let cfg: GatewayConfig = serde_yaml::from_str(cfg_yaml).unwrap();
        validate_rate_limit(&cfg).expect("default rate_limit must validate");
        validate_auth(&cfg).expect("default auth must validate");
        // Both Default impls produce the disabled state.
        assert!(!cfg.auth.enabled);
        assert!(!cfg.auth.required);
        assert!(!cfg.rate_limit.enabled);
    }

    // ── Plan 01 P01 T1: admin port validation ──────────────────────────────

    fn minimal_cfg() -> crate::GatewayConfig {
        serde_yaml::from_str("server: {bind: 127.0.0.1, port: 8787}").unwrap()
    }

    #[test]
    fn admin_port_zero_rejected() {
        let mut cfg = minimal_cfg();
        cfg.admin = Some(crate::AdminConfig {
            bind: "127.0.0.1".into(),
            port: 0,
        });
        assert!(matches!(validate(&cfg), Err(ValidationError::ZeroPort)));
    }

    #[test]
    fn admin_port_equal_to_server_port_rejected() {
        let mut cfg = minimal_cfg();
        cfg.admin = Some(crate::AdminConfig {
            bind: cfg.server.bind.clone(),
            port: cfg.server.port,
        });
        match validate(&cfg) {
            Err(ValidationError::PortCollision {
                admin,
                server,
                bind,
            }) => {
                assert_eq!(admin, cfg.server.port);
                assert_eq!(server, cfg.server.port);
                assert_eq!(bind, cfg.server.bind);
            }
            other => panic!("expected PortCollision, got {other:?}"),
        }
    }

    #[test]
    fn admin_port_different_bind_or_port_ok() {
        let mut cfg = minimal_cfg();
        cfg.admin = Some(crate::AdminConfig {
            bind: "127.0.0.1".into(),
            port: 9100,
        });
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn otel_sample_ratio_out_of_range_rejected() {
        let mut cfg = minimal_cfg();
        cfg.otel = Some(crate::OtelConfig {
            sample_ratio: 1.5,
            ..Default::default()
        });
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn otel_sample_ratio_in_range_ok() {
        let mut cfg = minimal_cfg();
        cfg.otel = Some(crate::OtelConfig {
            sample_ratio: 0.5,
            ..Default::default()
        });
        assert!(validate(&cfg).is_ok());
    }

    // ── Plan 04 P04 T1: validate_for_reload (rules 11-14) ───────────────────

    fn baseline_from(cfg: &GatewayConfig) -> ReloadBaseline {
        ReloadBaseline {
            upstream_names: cfg.upstreams.keys().cloned().collect(),
            server: cfg.server.clone(),
            admin: cfg.admin.clone(),
            otel_endpoint: cfg.otel.as_ref().and_then(|o| o.endpoint.clone()),
        }
    }

    #[test]
    fn reload_rejects_changed_server_port() {
        let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a, tier: standard}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        let baseline = baseline_from(&cfg);
        let mut candidate = cfg.clone();
        candidate.server.port = 9999;
        let err = validate_for_reload(&candidate, &baseline).unwrap_err();
        assert!(matches!(
            err,
            ReloadValidationError::ImmutableFieldChanged {
                field: "server.port",
                ..
            }
        ));
    }

    #[test]
    fn reload_rejects_added_upstream() {
        let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a, tier: standard}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        let baseline = baseline_from(&cfg);
        let mut candidate = cfg.clone();
        candidate.upstreams.insert(
            "n".into(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "http://y/v1".into(),
                api_key: Secret::new("b"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 120,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        let err = validate_for_reload(&candidate, &baseline).unwrap_err();
        assert!(matches!(
            err,
            ReloadValidationError::UpstreamSetChanged { .. }
        ));
    }

    #[test]
    fn reload_accepts_route_change_with_same_upstreams() {
        let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a, tier: standard}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        let baseline = baseline_from(&cfg);
        let mut candidate = cfg.clone();
        // Add a second route alias pointing at the same upstream — fine.
        candidate.routes.push(RouteEntry {
            frontend: "openai_chat".into(),
            model: "y".into(),
            upstream: Some("m".into()),
            upstream_model: Some("y-real".into()),
            upstreams: vec![],
            retry: Default::default(),
            breaker: Default::default(),
            reasoning_effort: None,
            anthropic_beta: None,
            min_tier: None,
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![],
        });
        let diff = validate_for_reload(&candidate, &baseline).expect("ok");
        assert_eq!(diff.routes_total, 2);
    }

    #[test]
    fn reload_accepts_otel_sample_ratio_change() {
        let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
otel: {endpoint: "http://c:4317", sample_ratio: 1.0}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a, tier: standard}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        let baseline = baseline_from(&cfg);
        let mut candidate = cfg.clone();
        candidate.otel.as_mut().unwrap().sample_ratio = 0.5;
        let _diff = validate_for_reload(&candidate, &baseline).expect("ok");
    }

    #[test]
    fn reload_rejects_otel_endpoint_change() {
        let yaml = r#"
server: {bind: 127.0.0.1, port: 8787}
otel: {endpoint: "http://c:4317"}
upstreams:
  m: {type: open_ai_compatible, base_url: http://x/v1, api_key: a, tier: standard}
routes:
  - {frontend: openai_chat, model: x, upstream: m, upstream_model: x}
"#;
        let cfg: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        let baseline = baseline_from(&cfg);
        let mut candidate = cfg.clone();
        candidate.otel.as_mut().unwrap().endpoint = Some("http://other:4317".into());
        let err = validate_for_reload(&candidate, &baseline).unwrap_err();
        assert!(matches!(
            err,
            ReloadValidationError::ImmutableFieldChanged {
                field: "otel.endpoint",
                ..
            }
        ));
    }

    // ── Plan 06 P03 T4: cost / tier / min_tier validation tests ─────────────

    /// Build a single-route GatewayConfig with one OpenAI-compatible upstream
    /// `m` configured to the given tier and cost. Used by the rule-15 and
    /// rule-17 tests below.
    fn cfg_with_one_oai_upstream(input_cost: f64, output_cost: f64, tier: Tier) -> GatewayConfig {
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "m".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "http://x/v1".into(),
                api_key: Secret::new("a"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 120,
                tier,
                cost: Some(UpstreamCost {
                    input_per_million_usd: input_cost,
                    output_per_million_usd: output_cost,
                }),
                p95_latency_budget_ms: None,
            }),
        );
        cfg.routes
            .push(RouteEntry::singular("openai_chat", "x", "m", "x-real"));
        cfg
    }

    #[test]
    fn cost_negative_input_rejected() {
        let cfg = cfg_with_one_oai_upstream(-1.0, 1.0, Tier::Standard);
        let err = validate(&cfg).unwrap_err();
        match err {
            ValidationError::NegativeCost { upstream, field } => {
                assert_eq!(upstream, "m");
                assert_eq!(field, "input_per_million_usd");
            }
            other => panic!("expected NegativeCost, got {other:?}"),
        }
    }

    #[test]
    fn cost_negative_output_rejected() {
        let cfg = cfg_with_one_oai_upstream(1.0, -1.0, Tier::Standard);
        let err = validate(&cfg).unwrap_err();
        match err {
            ValidationError::NegativeCost { upstream, field } => {
                assert_eq!(upstream, "m");
                assert_eq!(field, "output_per_million_usd");
            }
            other => panic!("expected NegativeCost, got {other:?}"),
        }
    }

    #[test]
    fn impossible_min_tier_rejected() {
        let mut cfg = cfg_with_one_oai_upstream(1.0, 1.0, Tier::Economy);
        // Demand Premium when the only upstream is Economy → rule 17 fires.
        cfg.routes[0].min_tier = Some(Tier::Premium);
        let err = validate(&cfg).unwrap_err();
        assert!(
            matches!(err, ValidationError::ImpossibleMinTier { .. }),
            "expected ImpossibleMinTier, got {err:?}"
        );
    }

    #[test]
    fn impossible_min_tier_chain_with_one_match_passes() {
        // Chain has two upstreams: eco (economy) + std (standard). Route
        // demands min_tier=standard → satisfied by std, rule 17 must not fire.
        let mut cfg = minimal_config();
        cfg.upstreams.insert(
            "eco".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "http://eco/v1".into(),
                api_key: Secret::new("a"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Economy,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        cfg.upstreams.insert(
            "std".to_string(),
            UpstreamConfig::OpenAiCompatible(OpenAiCompatibleUpstream {
                base_url: "http://std/v1".into(),
                api_key: Secret::new("a"),
                default_headers: BTreeMap::new(),
                request_timeout_secs: 30,
                tier: Tier::Standard,
                cost: None,
                p95_latency_budget_ms: None,
            }),
        );
        cfg.routes.push(RouteEntry {
            frontend: "openai_chat".into(),
            model: "x".into(),
            upstream: None,
            upstream_model: None,
            upstreams: vec![
                UpstreamRef {
                    name: "eco".into(),
                    model: "eco-real".into(),
                },
                UpstreamRef {
                    name: "std".into(),
                    model: "std-real".into(),
                },
            ],
            reasoning_effort: None,
            anthropic_beta: None,
            retry: RetryConfig::default(),
            breaker: BreakerConfig::default(),
            min_tier: Some(Tier::Standard),
            max_cost_usd: None,
            plugins: None,
            reasoning_mapping: vec![],
        });
        validate(&cfg).expect("chain has std which meets min_tier=standard");
    }

    #[test]
    fn reload_allows_tier_change() {
        // Rule 18 (Plan 06 P03): tier is reloadable, not immutable.
        let cfg = cfg_with_one_oai_upstream(1.0, 2.0, Tier::Standard);
        let baseline = baseline_from(&cfg);
        let mut candidate = cfg.clone();
        if let UpstreamConfig::OpenAiCompatible(c) = candidate.upstreams.get_mut("m").unwrap() {
            c.tier = Tier::Premium;
        }
        validate_for_reload(&candidate, &baseline).expect("tier change must be reloadable");
    }

    #[test]
    fn reload_rejects_tier_change_that_breaks_rule_17() {
        // Reload re-runs validate(candidate). If the tier drop makes the
        // route's min_tier unsatisfiable, rule 17 must fire on reload too.
        let mut cfg = cfg_with_one_oai_upstream(1.0, 2.0, Tier::Premium);
        cfg.routes[0].min_tier = Some(Tier::Premium);
        let baseline = baseline_from(&cfg);

        let mut candidate = cfg.clone();
        if let UpstreamConfig::OpenAiCompatible(c) = candidate.upstreams.get_mut("m").unwrap() {
            c.tier = Tier::Economy; // breaks min_tier=Premium
        }
        let err = validate_for_reload(&candidate, &baseline).unwrap_err();
        assert!(
            matches!(
                err,
                ReloadValidationError::StartupError(ValidationError::ImpossibleMinTier { .. })
            ),
            "expected StartupError(ImpossibleMinTier), got {err:?}"
        );
    }

    // ── Phase 7 P03 Layer A plugin validation ───────────────────────────────

    fn mk_cfg_with_routes(routes: Vec<crate::schema::RouteEntry>) -> crate::schema::GatewayConfig {
        crate::schema::GatewayConfig {
            server: Default::default(),
            logging: Default::default(),
            upstreams: Default::default(),
            routes,
            plugins: Default::default(),
            auth: Default::default(),
            rate_limit: Default::default(),
            copilot: None,
            admin: None,
            metrics: Default::default(),
            otel: None,
            shutdown: Default::default(),
        }
    }

    #[test]
    fn validate_plugins_rejects_undeclared_reference() {
        let mut route = crate::schema::RouteEntry::singular(
            "anthropic_messages",
            "claude-sonnet",
            "anthropic",
            "claude-sonnet",
        );
        route.plugins = Some(crate::plugins::RoutePluginsBlock {
            on_decoded_request: vec!["missing".to_string()],
            ..Default::default()
        });
        let cfg = mk_cfg_with_routes(vec![route]);
        let err = crate::validation::validate_plugins(&cfg).unwrap_err();
        match err {
            ValidationError::UndeclaredPlugin { plugin, hook, .. } => {
                assert_eq!(plugin, "missing");
                assert_eq!(hook, "on_decoded_request");
            }
            other => panic!("expected UndeclaredPlugin, got {other:?}"),
        }
    }

    #[test]
    fn validate_plugins_accepts_declared_reference() {
        let mut route = crate::schema::RouteEntry::singular(
            "anthropic_messages",
            "claude-sonnet",
            "anthropic",
            "claude-sonnet",
        );
        route.plugins = Some(crate::plugins::RoutePluginsBlock {
            on_decoded_request: vec!["compressor".to_string()],
            ..Default::default()
        });
        let mut cfg = mk_cfg_with_routes(vec![route]);
        cfg.plugins.insert(
            "compressor".to_string(),
            crate::plugins::PluginEntry {
                kind: "prompt_compressor".to_string(),
                config: serde_json::json!({}),
                on_error: crate::plugins::OnErrorYaml::Skip,
                timeout_ms: None,
                enabled: true,
            },
        );
        assert!(crate::validation::validate_plugins(&cfg).is_ok());
    }

    #[test]
    fn validate_plugins_rejects_zero_uniform_timeout() {
        let mut cfg = mk_cfg_with_routes(vec![]);
        cfg.plugins.insert(
            "p".to_string(),
            crate::plugins::PluginEntry {
                kind: "prompt_compressor".to_string(),
                config: serde_json::json!({}),
                on_error: crate::plugins::OnErrorYaml::Skip,
                timeout_ms: Some(crate::plugins::TimeoutMs::Uniform(0)),
                enabled: true,
            },
        );
        let err = crate::validation::validate_plugins(&cfg).unwrap_err();
        match err {
            ValidationError::ZeroTimeoutMs { plugin, slot } => {
                assert_eq!(plugin, "p");
                assert_eq!(slot, "uniform");
            }
            other => panic!("expected ZeroTimeoutMs, got {other:?}"),
        }
    }

    #[test]
    fn validate_plugins_rejects_zero_per_hook_timeout() {
        let mut cfg = mk_cfg_with_routes(vec![]);
        cfg.plugins.insert(
            "p".to_string(),
            crate::plugins::PluginEntry {
                kind: "prompt_compressor".to_string(),
                config: serde_json::json!({}),
                on_error: crate::plugins::OnErrorYaml::Skip,
                timeout_ms: Some(crate::plugins::TimeoutMs::PerHook {
                    default: Some(50),
                    on_decoded_request: None,
                    on_resolved: None,
                    on_stream_event: Some(0), // <- this triggers
                    on_response_complete: None,
                }),
                enabled: true,
            },
        );
        let err = crate::validation::validate_plugins(&cfg).unwrap_err();
        match err {
            ValidationError::ZeroTimeoutMs { plugin, slot } => {
                assert_eq!(plugin, "p");
                assert_eq!(slot, "on_stream_event");
            }
            other => panic!("expected ZeroTimeoutMs, got {other:?}"),
        }
    }

    /// Independent regression test that EACH of the 5 PerHook slots is
    /// checked by `validate_plugins`. If a future refactor drops a slot
    /// from the inner array literal, the per-slot test above might still
    /// pass while the other 4 slots silently leak zero timeouts. This
    /// table-driven test brackets the array.
    #[test]
    fn validate_plugins_checks_every_per_hook_slot() {
        let slots: [(&str, crate::plugins::TimeoutMs); 5] = [
            (
                "default",
                crate::plugins::TimeoutMs::PerHook {
                    default: Some(0),
                    on_decoded_request: None,
                    on_resolved: None,
                    on_stream_event: None,
                    on_response_complete: None,
                },
            ),
            (
                "on_decoded_request",
                crate::plugins::TimeoutMs::PerHook {
                    default: None,
                    on_decoded_request: Some(0),
                    on_resolved: None,
                    on_stream_event: None,
                    on_response_complete: None,
                },
            ),
            (
                "on_resolved",
                crate::plugins::TimeoutMs::PerHook {
                    default: None,
                    on_decoded_request: None,
                    on_resolved: Some(0),
                    on_stream_event: None,
                    on_response_complete: None,
                },
            ),
            (
                "on_stream_event",
                crate::plugins::TimeoutMs::PerHook {
                    default: None,
                    on_decoded_request: None,
                    on_resolved: None,
                    on_stream_event: Some(0),
                    on_response_complete: None,
                },
            ),
            (
                "on_response_complete",
                crate::plugins::TimeoutMs::PerHook {
                    default: None,
                    on_decoded_request: None,
                    on_resolved: None,
                    on_stream_event: None,
                    on_response_complete: Some(0),
                },
            ),
        ];
        for (expected_slot, timeout) in slots {
            let mut cfg = mk_cfg_with_routes(vec![]);
            cfg.plugins.insert(
                "p".to_string(),
                crate::plugins::PluginEntry {
                    kind: "prompt_compressor".to_string(),
                    config: serde_json::json!({}),
                    on_error: crate::plugins::OnErrorYaml::Skip,
                    timeout_ms: Some(timeout),
                    enabled: true,
                },
            );
            let err = crate::validation::validate_plugins(&cfg).unwrap_err();
            match err {
                ValidationError::ZeroTimeoutMs { plugin, slot } => {
                    assert_eq!(plugin, "p");
                    assert_eq!(
                        slot, expected_slot,
                        "wrong slot reported for {expected_slot} zero timeout"
                    );
                }
                other => panic!("expected ZeroTimeoutMs for slot {expected_slot}, got {other:?}"),
            }
        }
    }

    // ── Phase 7 P05 T10: shutdown.plugin_flush_secs Layer A validation ──────

    #[test]
    fn validate_rejects_excessive_shutdown_flush_secs() {
        use crate::schema::ShutdownConfig;
        // Build a minimal valid config (port=8787, no upstreams, no routes).
        // Use the test scaffold pattern already present in this mod.
        let mut cfg: GatewayConfig = serde_yaml::from_str(
            r#"
upstreams: {}
routes: []
"#,
        )
        .unwrap();
        cfg.shutdown = ShutdownConfig {
            plugin_flush_secs: 301,
        };
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("plugin_flush_secs"),
            "error message names the bad field, got: {err}"
        );
    }

    #[test]
    fn validate_accepts_max_shutdown_flush_secs() {
        use crate::schema::ShutdownConfig;
        let mut cfg: GatewayConfig = serde_yaml::from_str(
            r#"
upstreams: {}
routes: []
"#,
        )
        .unwrap();
        cfg.shutdown = ShutdownConfig {
            plugin_flush_secs: 300,
        };
        assert!(validate(&cfg).is_ok());
    }
}
