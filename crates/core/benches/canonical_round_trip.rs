// TODO: Implement canonical round-trip benchmarks using criterion.
//
// Suggested benchmarks:
//   - encode 1000-event CanonicalStream through AnthropicMessages SSE encoder
//   - encode 1000-event CanonicalStream through OpenAiChat SSE encoder
//   - decode a large Anthropic Messages request body
//   - decode a large OpenAI Chat request body
//
// Example skeleton:
//
// use criterion::{criterion_group, criterion_main, Criterion};
//
// fn bench_anthropic_encode(c: &mut Criterion) {
//     c.bench_function("anthropic_encode_1k_deltas", |b| {
//         b.iter(|| { /* … */ });
//     });
// }
//
// criterion_group!(benches, bench_anthropic_encode);
// criterion_main!(benches);
fn main() {}
