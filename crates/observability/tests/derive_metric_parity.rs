//! Structural-parity gate for the metric catalog. Plan v0.6.1 P02 (M-6).
//!
//! The fixture `tests/v060_metrics_baseline.json` captures the
//! `(name, kind, help)` triples of every metric registered at the v0.6.0
//! release point (master `b90b7f3`). After the P02 refactor introduces
//! `#[derive(Metric)]`, the runtime catalog MUST yield the same set —
//! same names, same kinds, same help strings — sorted by name.
//!
//! If a metric is added or removed in a later release, regenerate the
//! fixture in the same release's P05/P-docs plan, NOT here.

use std::collections::BTreeSet;

use agent_shim_observability::metrics::catalog::{iter_descriptors, MetricKind};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
struct MetricRow {
    name: String,
    kind: String,
    help: String,
}

fn kind_label(k: MetricKind) -> &'static str {
    match k {
        MetricKind::Counter => "counter",
        MetricKind::Histogram => "histogram",
        MetricKind::Gauge => "gauge",
    }
}

#[test]
fn registered_metrics_match_v060_baseline() {
    let actual: BTreeSet<MetricRow> = iter_descriptors()
        .into_iter()
        .map(|d| MetricRow {
            name: d.name.to_string(),
            kind: kind_label(d.kind).to_string(),
            help: d.help.to_string(),
        })
        .collect();

    let golden_text = include_str!("v060_metrics_baseline.json");
    let golden: BTreeSet<MetricRow> =
        serde_json::from_str(golden_text).expect("baseline JSON parses");

    if actual != golden {
        let only_actual: Vec<&MetricRow> = actual.difference(&golden).collect();
        let only_golden: Vec<&MetricRow> = golden.difference(&actual).collect();
        panic!(
            "metric registration drifted from v0.6.0 baseline\n\
             present in current build but not baseline ({}): {:#?}\n\
             present in baseline but not current build ({}): {:#?}",
            only_actual.len(),
            only_actual,
            only_golden.len(),
            only_golden
        );
    }
}
