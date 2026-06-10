//! Regression: Claude Code -> claude-opus-4-8 with SessionStart hook
//! injection as a tail-positioned role:"system" message in the
//! `messages` array used to fail with HTTP 400
//! `unknown role: system`. The 2026-06-10 spec authorises this role;
//! this test pins that fix.

use std::fs;

#[test]
fn opus_4_8_session_start_hook_dump_decodes() {
    let path = r"C:\ProgramData\agent-shim\logs\decode-failures\anthropic-messages-13c83365.json";
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("regression dump not present at {path}; skipping");
            return;
        }
    };
    let req = agent_shim_frontends::anthropic_messages::decode::decode(&bytes)
        .expect("decoder must accept the SessionStart-hook dump");

    // The dump's messages array ends with a role:"system" entry that the
    // pre-fix decoder rejected. Confirm the tail message decoded as a
    // positional System message (prelude_phase ended before it).
    assert!(
        !req.messages.is_empty(),
        "decoded request must contain at least one message"
    );

    let last = req.messages.last().expect("at least one message present");
    assert_eq!(
        last.role,
        agent_shim_core::MessageRole::System,
        "tail message must decode as positional System role"
    );
}
