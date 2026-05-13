//! Compile-test the `#[derive(Metric)]` macro.
//!
//! Each `.rs` file in `tests/ui/` is its own compile fixture. Pass
//! fixtures must compile cleanly; fail fixtures must error with the
//! diagnostic captured in the matching `.stderr` file.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/derive_pass_counter.rs");
    t.compile_fail("tests/ui/derive_fail_missing_name.rs");
    t.compile_fail("tests/ui/derive_fail_missing_kind.rs");
}
