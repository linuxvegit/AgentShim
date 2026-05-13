use agent_shim_observability_derive::Metric;

#[derive(Metric)]
#[metric(kind = "counter", help = "Test")]
pub struct MissingName;

fn main() {}
