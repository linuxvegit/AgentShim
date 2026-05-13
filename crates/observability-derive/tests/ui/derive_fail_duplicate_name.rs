use agent_shim_observability_derive::Metric;

#[derive(Metric)]
#[metric(name = "agent_shim_test", name = "agent_shim_test_dup", kind = "counter", help = "Test")]
pub struct DuplicateName;

fn main() {}
