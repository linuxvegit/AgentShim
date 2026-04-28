# Configuration

## YAML Shape

```yaml
server:
  host: "127.0.0.1"
  port: 8787

routes:
  - frontend: anthropic_messages       # or openai_chat
    path_prefix: /v1/messages
    provider: deepseek_main

providers:
  deepseek_main:
    kind: openai_compatible
    base_url: "https://api.deepseek.com/v1"
    api_key: !secret DEEPSEEK_API_KEY   # pulled from env
    model: deepseek-chat
```

## Environment Overlay

Every config key can be overridden via environment variable using the
`AGENT_SHIM__` prefix with double-underscore as separator:

```
AGENT_SHIM__SERVER__PORT=9090
AGENT_SHIM__PROVIDERS__DEEPSEEK_MAIN__API_KEY=sk-...
```

Figment merges sources in priority order: defaults < YAML file < env vars.

## deny_unknown_fields

The config structs are annotated with `#[serde(deny_unknown_fields)]`.
Unrecognised keys cause a hard startup error, preventing silent typos.

## Secret Handling

API keys should be stored in environment variables and referenced via
the `!secret ENV_VAR_NAME` YAML tag **or** via the env overlay.
Never commit plaintext keys to the config file.
The `Secret<String>` wrapper in `agent-shim-config` prevents the value
from appearing in `Debug` output or structured logs.
