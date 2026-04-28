// TODO: Implement gateway overhead benchmarks using criterion.
//
// Suggested benchmarks:
//   - end-to-end latency for a single non-streaming request through the gateway
//   - end-to-end throughput for a 1000-chunk streaming response
//   - router resolution time for a table of 100 routes
//
// Example skeleton:
//
// use criterion::{criterion_group, criterion_main, Criterion};
//
// fn bench_route_resolve(c: &mut Criterion) {
//     c.bench_function("route_resolve_100_entries", |b| {
//         b.iter(|| { /* … */ });
//     });
// }
//
// criterion_group!(benches, bench_route_resolve);
// criterion_main!(benches);
fn main() {}
