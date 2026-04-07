// One #[test] function per binary in `data/`. The test set is generated
// at build time by `build.rs` and included from $OUT_DIR/generated_tests.rs.

include!(concat!(env!("OUT_DIR"), "/generated_tests.rs"));
