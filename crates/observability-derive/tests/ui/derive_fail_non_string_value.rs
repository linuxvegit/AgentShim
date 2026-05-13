use agent_shim_observability_derive::Metric;

#[derive(Metric)]
#[metric(name = "agent_shim_test", kind = 42, help = "Test")]
pub struct NonStringKind;

fn main() {}
