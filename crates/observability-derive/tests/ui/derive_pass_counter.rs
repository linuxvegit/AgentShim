use agent_shim_observability_derive::Metric;

#[derive(Metric)]
#[metric(name = "agent_shim_test_counter", kind = "counter", help = "Test counter")]
pub struct TestCounter;

fn main() {
    let _name: &'static str = TestCounter::NAME;
    let _help: &'static str = TestCounter::HELP;
    // KIND is an enum value defined by the host crate; not testable in
    // T3 (the host crate's catalog::MetricKind doesn't exist yet).
    // Wired up in T4.
}
