#!/usr/bin/env bash
# Regenerate provider fixture files by replaying canonical request bodies
# against the real upstream APIs.
#
# This is a developer tool, NOT a CI step. It exists so that maintainers
# can refresh `crates/protocol-tests/fixtures/<provider>/*.upstream.{sse,json}`
# captures whenever a provider ships a wire-format change (e.g. a new
# tool-call event variant, a new usage field, a renamed reasoning event).
# Manual replay > drifting fixtures.
#
# Usage:
#   scripts/regen-fixtures.sh                 # all providers
#   scripts/regen-fixtures.sh anthropic       # one provider
#   scripts/regen-fixtures.sh deepseek
#   scripts/regen-fixtures.sh gemini
#
# Required environment variables, by mode:
#   anthropic | all → ANTHROPIC_API_KEY
#   deepseek  | all → DEEPSEEK_API_KEY
#   gemini    | all → GEMINI_API_KEY
#
# What this script does NOT do:
#   - Run any tests after regeneration. You must follow up with
#     `cargo nextest run --workspace` to confirm the new fixtures are
#     consistent with the rest of the gateway.
#   - Touch capture credentials. The script reads keys from env; it never
#     writes them anywhere on disk.
#   - Re-encode fixtures through the gateway. The fixtures are pure
#     captures — encode-side tests live separately and exercise the
#     gateway's encoder against in-tree fixtures.

set -euo pipefail

mode="${1:-all}"

require_env() {
    local var="$1"
    if [[ -z "${!var:-}" ]]; then
        echo "error: $var is required for mode '$mode'" >&2
        exit 1
    fi
}

regen_anthropic() {
    require_env ANTHROPIC_API_KEY
    echo "→ regenerating Anthropic fixtures…"
    # TODO: hand-rolled curl invocations against api.anthropic.com saving
    # SSE bodies into crates/protocol-tests/fixtures/anthropic/*.upstream.sse,
    # then re-run the gateway against those captures to refresh
    # expected/* fixtures. The exact requests to replay are documented
    # alongside each fixture file.
    echo "  (not yet implemented — see fixtures/anthropic/README.md)"
}

regen_deepseek() {
    require_env DEEPSEEK_API_KEY
    echo "→ regenerating DeepSeek fixtures…"
    # TODO: curl `${DEEPSEEK_BASE_URL:-https://api.deepseek.com}/v1/chat/completions`
    # for both deepseek-chat and deepseek-reasoner with the canonical
    # text-streaming + tool-call + reasoning fixtures. Save raw SSE bodies
    # into crates/protocol-tests/fixtures/deepseek/.
    echo "  (not yet implemented — see fixtures/deepseek/README.md)"
}

regen_gemini() {
    require_env GEMINI_API_KEY
    echo "→ regenerating Gemini fixtures…"
    # TODO: curl Generate Content streaming endpoint with
    # ?alt=sse OFF (we use the JSON-array framing the gateway parses)
    # for gemini-2.0-flash and gemini-2.5-flash-thinking. Save into
    # crates/protocol-tests/fixtures/gemini/.
    echo "  (not yet implemented — see fixtures/gemini/README.md)"
}

case "$mode" in
    anthropic)
        regen_anthropic
        ;;
    deepseek)
        regen_deepseek
        ;;
    gemini)
        regen_gemini
        ;;
    all)
        regen_anthropic
        regen_deepseek
        regen_gemini
        ;;
    *)
        echo "unknown mode: $mode" >&2
        echo "usage: $0 [anthropic|deepseek|gemini|all]" >&2
        exit 1
        ;;
esac

echo
echo "✓ Fixture regeneration finished. Run 'cargo nextest run --workspace' to confirm."
