//! Phase 4 lands the real SCM entry point here. This stub exists so the
//! `commands::service::run` dispatch matches all `ServiceCommand` variants.

use std::path::Path;

pub fn run(_config: &Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "`agent-shim service run` is the SCM entry point and is not user-invokable; \
         it will be implemented in Phase 4"
    )
}
