# Benchmark Snapshot

> Last run: 2026-05-22 on CPC-xumou-BHC5H / AMD EPYC 7763 64-Core Processor.

## empty_registry/run_on_decoded_request/empty

Median: 827.69 ns

## Methodology

See `empty_registry_overhead.rs` and the P07 design spec §7.

The "zero-overhead empty registry" claim in CHANGELOG v0.7.0 rests on
this single microbench plus the early-return code path in
`crates/plugins/src/registry.rs::run_on_decoded_request` (and siblings).

The benchmark is NOT part of CI gates — `bench_results.md` is a snapshot
record for human reference, not a regression guard. CI only verifies the
bench compiles via `cargo bench --no-run` (P07 T13).
