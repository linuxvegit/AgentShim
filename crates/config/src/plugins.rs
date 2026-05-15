//! YAML schema for the plugin system, Phase 7.
//!
//! Three new shapes:
//! - `PluginEntry` — one entry under top-level `plugins:` map.
//! - `TimeoutMs` — accepts either `timeout_ms: 50` (uniform) or
//!   `timeout_ms: { default: 50, on_stream_event: 5 }` (per-hook).
//! - `RoutePluginsBlock` — per-route `plugins:` block holding four
//!   ordered lists, one per hook.
//!
//! Spec §5.1 / §5.2 / §5.4.

use serde::{Deserialize, Serialize};

/// One plugin instance, declared under top-level `plugins:` in
/// `gateway.yaml`. Carries the kind (which Rust impl), an opaque
/// `config:` blob owned by the factory, an `on_error` policy, a
/// `timeout_ms` (uniform or per-hook), and an `enabled` flag.
///
/// The `config` field is intentionally `serde_json::Value` so the
/// config crate stays agnostic to plugin kinds — factories own their
/// internal schemas and surface deserialisation errors at Layer B
/// validation (P06).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    /// Plugin kind name — matches `Plugin::kind_name()` of the
    /// constructed instance. YAML key kept as `type:` for consistency
    /// with `upstreams[].type:` (Q2).
    #[serde(rename = "type")]
    pub kind: String,

    /// Kind-specific config. Opaque to the config crate; the factory
    /// deserialises its own struct from this. Defaults to an empty
    /// JSON object so plugins without config can write
    /// `compressor: { type: prompt_compressor }`.
    #[serde(default = "default_config")]
    pub config: serde_json::Value,

    /// On-error policy. `skip` swallows internal failures; `fail`
    /// propagates them. Defaults to `skip` (spec §5.1).
    #[serde(default)]
    pub on_error: OnErrorYaml,

    /// Per-hook timeouts. `None` = use system defaults (50/50/5/50 ms
    /// per spec §5.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<TimeoutMs>,

    /// `false` suppresses the plugin from any route plan but keeps
    /// config validation honest (plugin still constructed at startup
    /// so config bugs surface, per spec §5.3 rule 7).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_config() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn default_enabled() -> bool {
    true
}

/// YAML form of `agent_shim_plugins::OnError`. Lives here so
/// `agent-shim-config` doesn't have to depend on `agent-shim-plugins`
/// (boundary discipline — config is a leaf-ish crate). The gateway
/// wiring (P06) converts this into `agent_shim_plugins::OnError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnErrorYaml {
    #[default]
    Skip,
    Fail,
}

/// YAML form of per-plugin timeouts. Accepts either a uniform value
/// or a per-hook map.
///
/// ```yaml
/// timeout_ms: 50          # uniform
/// timeout_ms:             # per-hook
///   default: 50
///   on_stream_event: 5
/// ```
///
/// (Spec §5.4.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TimeoutMs {
    /// `timeout_ms: 50` — single u64 applies to all subscribed hooks.
    Uniform(u64),

    /// `timeout_ms: { default: 50, on_stream_event: 5 }` — per-hook
    /// overrides on top of `default`. Each per-hook field is optional;
    /// absent fields inherit `default`. Both `default` and any per-hook
    /// fields are optional at the schema layer — Layer A validation
    /// rejects `timeout_ms == 0` for any populated slot.
    PerHook {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_decoded_request: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_resolved: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_stream_event: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_response_complete: Option<u64>,
    },
}

/// Per-route `plugins:` block — four ordered lists, one per hook.
/// All four default to empty; omitting the whole block yields the
/// default (no plugins on this route, fast path).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutePluginsBlock {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_decoded_request: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_resolved: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_stream_event: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_response_complete: Vec<String>,
}

impl RoutePluginsBlock {
    /// True when every hook list is empty — the registry's fast path
    /// can return identity without further work.
    pub fn is_empty(&self) -> bool {
        self.on_decoded_request.is_empty()
            && self.on_resolved.is_empty()
            && self.on_stream_event.is_empty()
            && self.on_response_complete.is_empty()
    }

    /// Iterator over all (hook_name, &plugin_name) pairs across the
    /// four lists. Used by Layer A validation to spot undeclared
    /// references and (in P06) by registry construction.
    pub fn iter_references(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let h2 = self
            .on_decoded_request
            .iter()
            .map(|n| ("on_decoded_request", n.as_str()));
        let h3 = self.on_resolved.iter().map(|n| ("on_resolved", n.as_str()));
        let h5 = self
            .on_stream_event
            .iter()
            .map(|n| ("on_stream_event", n.as_str()));
        let h7 = self
            .on_response_complete
            .iter()
            .map(|n| ("on_response_complete", n.as_str()));
        h2.chain(h3).chain(h5).chain(h7)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_error_yaml_default_is_skip() {
        assert_eq!(OnErrorYaml::default(), OnErrorYaml::Skip);
    }

    #[test]
    fn timeout_ms_accepts_uniform_form() {
        let v: TimeoutMs = serde_yaml::from_str("50").unwrap();
        assert_eq!(v, TimeoutMs::Uniform(50));
    }

    #[test]
    fn timeout_ms_accepts_per_hook_form() {
        let yaml = "default: 50\non_stream_event: 5";
        let v: TimeoutMs = serde_yaml::from_str(yaml).unwrap();
        match v {
            TimeoutMs::PerHook {
                default,
                on_stream_event,
                on_decoded_request,
                on_resolved,
                on_response_complete,
            } => {
                assert_eq!(default, Some(50));
                assert_eq!(on_stream_event, Some(5));
                assert_eq!(on_decoded_request, None);
                assert_eq!(on_resolved, None);
                assert_eq!(on_response_complete, None);
            }
            _ => panic!("expected PerHook"),
        }
    }

    #[test]
    fn plugin_entry_minimal_yaml_round_trip() {
        let yaml = "type: prompt_compressor";
        let p: PluginEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(p.kind, "prompt_compressor");
        assert_eq!(p.config, serde_json::json!({}));
        assert_eq!(p.on_error, OnErrorYaml::Skip);
        assert!(p.timeout_ms.is_none());
        assert!(p.enabled);
    }

    #[test]
    fn plugin_entry_full_yaml_round_trip() {
        let yaml = r#"
type: prompt_compressor
config:
  strategy: summarize_old_turns
  keep_last: 4
on_error: fail
timeout_ms: 50
enabled: false
"#;
        let p: PluginEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(p.kind, "prompt_compressor");
        assert_eq!(p.config["strategy"], "summarize_old_turns");
        assert_eq!(p.config["keep_last"], 4);
        assert_eq!(p.on_error, OnErrorYaml::Fail);
        assert_eq!(p.timeout_ms, Some(TimeoutMs::Uniform(50)));
        assert!(!p.enabled);
    }

    #[test]
    fn plugin_entry_rejects_unknown_field() {
        // `deny_unknown_fields` must catch typos.
        let yaml = "type: foo\nbogus_field: 1";
        let r: Result<PluginEntry, _> = serde_yaml::from_str(yaml);
        assert!(
            r.is_err(),
            "deny_unknown_fields should reject `bogus_field`"
        );
    }

    #[test]
    fn route_plugins_block_default_is_empty() {
        let block = RoutePluginsBlock::default();
        assert!(block.is_empty());
        assert_eq!(block.iter_references().count(), 0);
    }

    #[test]
    fn route_plugins_block_iter_references_walks_all_hooks() {
        let block = RoutePluginsBlock {
            on_decoded_request: vec!["a".to_string()],
            on_resolved: vec!["b".to_string()],
            on_stream_event: vec!["c".to_string()],
            on_response_complete: vec!["d".to_string()],
        };
        let pairs: Vec<(&str, &str)> = block.iter_references().collect();
        assert_eq!(
            pairs,
            vec![
                ("on_decoded_request", "a"),
                ("on_resolved", "b"),
                ("on_stream_event", "c"),
                ("on_response_complete", "d"),
            ]
        );
    }

    #[test]
    fn route_plugins_block_rejects_unknown_hook() {
        // `deny_unknown_fields` must reject typo-hooks.
        let yaml = "on_stream_evnt: [foo]"; // typo: evnt
        let r: Result<RoutePluginsBlock, _> = serde_yaml::from_str(yaml);
        assert!(
            r.is_err(),
            "deny_unknown_fields should reject `on_stream_evnt`"
        );
    }
}
