//! Re-exports from agent-shim-core::mapping::anthropic_wire so the frontend
//! keeps a stable public surface while the canonical implementation lives
//! in core (where the Anthropic provider can also depend on it without
//! violating the frontend↔provider boundary rule).
pub use agent_shim_core::mapping::anthropic_wire::{
    role_from_anthropic, role_to_anthropic, stop_reason_from_anthropic, stop_reason_to_anthropic,
};
