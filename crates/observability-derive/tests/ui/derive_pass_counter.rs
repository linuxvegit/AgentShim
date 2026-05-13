//! A well-formed counter: name + kind + help all supplied via the
//! `#[metric(...)]` attribute. Must compile without error AND register
//! a MetricDescriptor in the catalog's distributed_slice.

use agent_shim_observability::metrics::catalog::MetricKind;
use agent_shim_observability_derive::Metric;

#[derive(Metric)]
#[metric(name = "agent_shim_test_counter", kind = "counter", help = "Test counter")]
pub struct TestCounter;

fn main() {
    assert_eq!(TestCounter::NAME, "agent_shim_test_counter");
    assert_eq!(TestCounter::KIND, MetricKind::Counter);
    assert_eq!(TestCounter::HELP, "Test counter");
}
