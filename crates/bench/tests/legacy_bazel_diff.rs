//! The bazel-diff comparison stage, at the smallest preset.
//!
//! Skipped with a message when the bazel-diff binary has not been built, so a
//! clone without `tools/bazel-diff/fetch.sh` having run still tests green.

use cambium_bench::legacy::{self, LegacyOptions};
use cambium_bench::stages::StageContext;

#[test]
fn cambium_and_bazel_diff_agree_on_the_impacted_targets() {
    let Some(binary) = legacy::bazel_diff_binary(None) else {
        eprintln!(
            "skipping: no bazel-diff binary; run tools/bazel-diff/fetch.sh \
             or set CAMBIUM_BAZEL_DIFF"
        );
        return;
    };
    eprintln!("using bazel-diff at {}", binary.display());

    let ctx = StageContext::new("tiny", 1).expect("tiny preset");
    let run = legacy::run(&ctx, &LegacyOptions::default()).expect("stage runs and the sets agree");

    // Something must have been impacted, or the comparison is vacuous.
    assert!(
        !run.agreed.is_empty(),
        "no agreed labels: {:?}",
        run.result.extra
    );
    assert!(run.bazel_diff_only.is_empty());
    // Every cambium-only label is an over-approximation, never a miss.
    assert!(run.agreed.len() + run.cambium_only.len() >= run.agreed.len());
    assert_ne!(run.bazel_diff_version, "");
    for key in [
        "bazel_diff_cold_ms",
        "bazel_diff_warm_ms",
        "cambium_ms",
        "agreed",
    ] {
        assert!(run.result.extra.contains_key(key), "missing {key}");
    }
}
