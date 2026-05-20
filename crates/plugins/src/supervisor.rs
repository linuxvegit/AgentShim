//! `PluginSupervisor` — owns the JoinSet of H7 (on_response_complete)
//! futures and provides bounded-deadline shutdown flush.
//!
//! Spec §6.8 + §7.4 P05.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;

/// Owns the H7 task lifecycle: per-spawn JoinSet membership +
/// pending-attribution counter for shutdown drop reporting.
///
/// Locking: `std::sync::Mutex` (NOT tokio::sync::Mutex) because every
/// critical section is nanosecond-scale (a single HashMap mutation or
/// JoinSet::spawn / std::mem::take) and is never held across an .await.
/// Clippy `await_holding_lock` confirms safety.
pub struct PluginSupervisor {
    tasks: std::sync::Mutex<JoinSet<()>>,
    /// plugin_name → pending count. `spawn_h7` increments; the spawned
    /// task body decrements on completion.
    pending: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl PluginSupervisor {
    pub fn new() -> Self {
        Self {
            tasks: std::sync::Mutex::new(JoinSet::new()),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Spawn an H7 future. Sync function. Increments the pending counter
    /// for `plugin_name`; the spawned task decrements on completion.
    pub fn spawn_h7(&self, plugin_name: String, fut: impl Future<Output = ()> + Send + 'static) {
        *self
            .pending
            .lock()
            .unwrap()
            .entry(plugin_name.clone())
            .or_insert(0) += 1;
        let pending = Arc::clone(&self.pending);
        self.tasks.lock().unwrap().spawn(async move {
            fut.await;
            let mut p = pending.lock().unwrap();
            if let Some(cnt) = p.get_mut(&plugin_name) {
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    p.remove(&plugin_name);
                }
            }
        });
    }

    /// Wait for pending H7 tasks until `deadline` elapses. Returns
    /// `Vec<(plugin_name, dropped_count)>` for tasks that did not
    /// complete in time. Tasks remaining in the JoinSet on drop are
    /// aborted by Tokio.
    ///
    /// Safety: takes the JoinSet out of the mutex via `std::mem::take`,
    /// then drives `join_next()` under a `tokio::time::timeout`. The
    /// mutex is briefly held during the swap, never across .await.
    /// After axum drain, no new H7 spawns occur — this is safe.
    pub async fn flush_pending_h7(&self, deadline: Duration) -> Vec<(String, u64)> {
        // Take the JoinSet out of the Mutex for the flush duration.
        // The mutex now holds an empty JoinSet; if any spawn happens
        // while we are flushing (it shouldn't — gateway calls this only
        // after axum drain), that spawn lands in the empty replacement
        // and would be missed by THIS flush. P05 §6.8.
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());

        let _ = tokio::time::timeout(deadline, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;

        // Any remaining `pending` entries are tasks that did not finish
        // in time. Snapshot + return + clear.
        let mut p = self.pending.lock().unwrap();
        let dropped: Vec<(String, u64)> = p
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();
        p.clear();
        // tasks drops on function exit -> aborts in-flight survivors.
        drop(tasks);
        dropped
    }
}

impl std::fmt::Debug for PluginSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSupervisor").finish_non_exhaustive()
    }
}

impl Default for PluginSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_then_flush_completes_within_deadline() {
        let sup = PluginSupervisor::new();
        for i in 0..3 {
            sup.spawn_h7(format!("p{i}"), async {});
        }
        let dropped = sup.flush_pending_h7(Duration::from_secs(1)).await;
        assert!(
            dropped.is_empty(),
            "fast tasks completed; dropped must be empty, got {dropped:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_then_flush_drops_slow_tasks_returns_attribution() {
        let sup = PluginSupervisor::new();
        sup.spawn_h7("slow_plugin".to_string(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let dropped = sup.flush_pending_h7(Duration::from_millis(10)).await;
        assert_eq!(
            dropped,
            vec![("slow_plugin".to_string(), 1)],
            "single slow task dropped, attributed to plugin"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_concurrent_same_plugin_returns_aggregated_count() {
        let sup = PluginSupervisor::new();
        for _ in 0..5 {
            sup.spawn_h7("slow_plugin".to_string(), async {
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
        }
        let dropped = sup.flush_pending_h7(Duration::from_millis(10)).await;
        assert_eq!(
            dropped,
            vec![("slow_plugin".to_string(), 5)],
            "5 same-plugin spawns aggregate to count=5"
        );
    }

    #[tokio::test]
    async fn spawn_many_fast_then_flush_handles_all() {
        let sup = PluginSupervisor::new();
        for i in 0..100 {
            sup.spawn_h7(format!("plug{i}"), async {});
        }
        let dropped = sup.flush_pending_h7(Duration::from_secs(2)).await;
        assert!(
            dropped.is_empty(),
            "100 fast tasks all completed within deadline"
        );
    }
}
