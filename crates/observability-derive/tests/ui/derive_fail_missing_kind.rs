use agent_shim_observability_derive::Metric;

#[derive(Metric)]
#[metric(name = "agent_shim_test", help = "Test")]
pub struct MissingKind;

fn main() {}
