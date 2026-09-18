//! What `cambi explain` reads off a run: the path an affected target sits on,
//! the reason at the end of it, and — when the run went global — the
//! diagnostic that names the cause.
//!
//! The rows come from the synthetic source, so none of this needs Bazel.

use cambium_engine::{AffectedRequest, Code, DirectReason, GlobalReason, SynthQuerier};
use cambium_synth::{generate, GraphSpec, Mutation};
use cambium_testkit::{engine, req};

fn request() -> AffectedRequest {
    AffectedRequest {
        explain: true,
        materialize: true,
        ..req()
    }
}

/// A direct target's explanation: the path is the target itself, and the reason
/// names the source file that moved. This is the block `cambi explain
/// //foo:bar` prints.
#[test]
fn a_direct_target_is_explained_by_the_file_that_changed() {
    let e = engine();
    let before = generate(&GraphSpec::preset("small").unwrap());
    let base = e
        .full_build_with("c0", &SynthQuerier::new(&before))
        .unwrap();

    let t = (0..before.targets.len() as u32)
        .find(|&i| !before.targets[i as usize].srcs.is_empty())
        .expect("a target with a source");
    let slot = before.targets[t as usize].srcs[0];
    let mut after = before.clone();
    let changes = cambium_synth::apply_one(&mut after, &Mutation::ModifySource { target: t, slot });
    let changed = changes[0].path.clone();

    let report = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&after),
            &request(),
        )
        .unwrap()
        .report;

    let direct = report.direct.first().expect("a direct target").clone();
    let explanations = report.explanations.expect("explanations were asked for");
    let own = explanations
        .iter()
        .find(|x| x.target == direct)
        .unwrap_or_else(|| panic!("no explanation for the direct target {direct}"));
    assert_eq!(
        own.path,
        vec![direct.clone()],
        "a direct target is its own path"
    );
    assert_eq!(own.direct, direct);
    assert_eq!(
        own.reason,
        DirectReason::File {
            path: changed.clone()
        }
    );
    assert_eq!(
        own.reason.describe(),
        format!("source file {changed} changed"),
        "the line `explain` prints after `because`"
    );

    // A consumer further out is explained by the path back to that target.
    if let Some(hop) = explanations.iter().find(|x| x.path.len() > 1) {
        assert_eq!(hop.path[0], hop.target);
        assert_eq!(hop.path.last().unwrap(), &hop.direct);
        assert_eq!(hop.reason, own.reason, "the same change is behind both");
    }

    // And a target the change never reached has no explanation at all, which
    // is what `explain` answers "is not affected" from — it is in the head
    // index, so the label is not a typo either.
    let head = e
        .compute(
            base.view(),
            &changes,
            &SynthQuerier::new(&after),
            &request(),
        )
        .unwrap()
        .index
        .expect("a materialised head index");
    let untouched = (0..head.view().num_nodes())
        .map(|i| head.view().label(i).to_string())
        .find(|l| !report.affected.iter().any(|a| &a.label == l))
        .expect("a label the change did not reach");
    assert!(
        !explanations.iter().any(|x| x.target == untouched),
        "{untouched} is not affected, so nothing explains it"
    );
    assert!(head.view().id_of(&untouched).is_some());
}

/// With no label, `explain` tells the run's story, and the story of a global
/// run is the diagnostic the engine already emitted for the cause.
#[test]
fn a_global_run_leaves_behind_the_diagnostic_that_explains_it() {
    let e = engine();
    let g = generate(&GraphSpec::preset("tiny").unwrap());
    let base = e.full_build_with("c0", &SynthQuerier::new(&g)).unwrap();

    // `.bazelrc` is an invalidator by default, and with no checkout to read
    // the two texts from, the screen cannot prove the edit harmless.
    let changes = vec![cambium_diff::Change {
        path: ".bazelrc".into(),
        status: cambium_diff::Status::Modified,
    }];
    let report = e
        .compute(base.view(), &changes, &SynthQuerier::new(&g), &request())
        .unwrap()
        .report;

    assert!(report.global, "{report:?}");
    assert!(
        report.global_reason.is_some(),
        "the reason is in the report"
    );
    let diagnostics = e.diagnostics().drain();
    let cause = diagnostics
        .iter()
        .find(|d| GlobalReason::is_cause(d.code))
        .unwrap_or_else(|| panic!("a global run should name its cause: {diagnostics:?}"));
    assert_eq!(cause.code, Code::W004BazelrcGlobal);
    assert!(
        cause.help.is_some(),
        "the cause carries the fix `explain` prints: {cause:?}"
    );
}
