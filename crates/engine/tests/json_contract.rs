//! Pins cambium's JSON contract.
//!
//! Every JSON document the CLI emits is parsed by something that is not
//! cambium — CI glue, and shortly a service that shells out to the binary. The
//! field names, their shapes, and the exit codes are therefore a contract, and
//! `SCHEMA_VERSION` is how a consumer knows which one it is looking at.
//!
//! This test fails if the affected report's shape changes at all. That is the
//! point: the failure is the reminder to bump `SCHEMA_VERSION` and update the
//! list below, so a shape change cannot reach a consumer silently.

use std::collections::BTreeSet;

use cambium_engine::report::{
    AffectedReport, AffectedTarget, DirectReason, Explanation, Stats, Timings, SCHEMA_VERSION,
};
use cambium_engine::EngineError;
use serde_json::Value;

/// The shape of `cambi affected --output json`.
///
/// Version 3 turned `affected` into `{label, depth, test}` objects and added
/// `stats.rbuildfiles_called`; version 4 added `stats.bzl_closure_hits`, and
/// `stats.rbuildfiles_called` is now always false; version 5 added
/// `stats.bazelrc_safe_diffs`, `stats.cold_rebuild` and the optional
/// `cold_rebuild_reason`, the four `stats.repos_*` / `stats.external_direct_targets`
/// counters and the `external_repo` direct reason; version 7 added the four
/// `stats.bzl_witness*` counters, which say what the engine did about a
/// changed `.bzl` the base index's load closure had no node for, plus
/// `stats.bazelrc_flag_nodes`, the `bazelrc_flags` list and the
/// `starlark_flag` direct reason; version 8 added
/// `stats.repos_dirty_by_target` and the `external_repo_target` direct
/// reason, for a repository whose generated targets name a main-repo rule
/// target the walk reached. The version is shared with
/// the other documents, and `index stats` changed shape at version 2 and again
/// at 3.
///
/// Nested objects are dotted; arrays of scalars end in `[]`; arrays of objects
/// recurse through `[]`. Optional fields are listed — the fixture below
/// populates every one of them.
const AFFECTED_SCHEMA: &[&str] = &[
    "affected[].depth",
    "affected[].label",
    "affected[].test",
    "base",
    "cold_rebuild_reason",
    "direct[]",
    "dirty_packages[]",
    "explanations[].direct",
    "explanations[].path[]",
    "explanations[].reason.kind",
    "explanations[].reason.path",
    "explanations[].target",
    "global",
    "global_reason",
    "head",
    "schema",
    "stats.affected_targets",
    "stats.base_nodes",
    "stats.bazelrc_safe_diffs",
    "stats.bzl_closure_hits",
    "stats.bzl_witness_packages",
    "stats.bzl_witness_unavailable",
    "stats.bzl_witnessed_dead",
    "stats.bzl_witnessed_loaded",
    "stats.changed_files",
    "stats.cold_rebuild",
    "stats.direct_targets",
    "stats.external_direct_targets",
    "stats.dirty_packages",
    "stats.head_edges",
    "stats.head_nodes",
    "stats.ignored_files",
    "stats.queried_targets",
    "stats.repos_dirty_by_bzl",
    "stats.repos_dirty_by_target",
    "stats.repos_dirty_by_input",
    "stats.repos_dirty_by_module",
    "stats.repos_label_only",
    "stats.bazelrc_flag_nodes",
    "stats.rbuildfiles_called",
    "bazelrc_flags[]",
    "tests[]",
    "timings_ms.dirty",
    "timings_ms.direct",
    "timings_ms.load",
    "timings_ms.query",
    "timings_ms.splice",
    "timings_ms.total",
    "timings_ms.walk",
];

/// A report with every field populated, including the optional ones.
fn fixture() -> AffectedReport {
    AffectedReport {
        schema: SCHEMA_VERSION,
        base: "aaaa".into(),
        head: "bbbb".into(),
        global: false,
        global_reason: Some("only set when global".into()),
        cold_rebuild_reason: Some("only set when the run re-indexed at head".into()),
        dirty_packages: vec!["lib/core".into()],
        bazelrc_flags: vec!["--@envoy//bazel:crypto".into()],
        direct: vec!["//lib/core:core".into()],
        affected: vec![
            AffectedTarget {
                label: "//app:main".into(),
                depth: 1,
                test: false,
            },
            AffectedTarget {
                label: "//lib/core:core".into(),
                depth: 0,
                test: false,
            },
        ],
        tests: vec!["//app:main_test".into()],
        timings_ms: Timings {
            load: 1,
            dirty: 2,
            query: 3,
            splice: 4,
            direct: 5,
            walk: 6,
            total: 7,
        },
        stats: Stats {
            base_nodes: 1,
            head_nodes: 2,
            head_edges: 3,
            dirty_packages: 4,
            queried_targets: 5,
            direct_targets: 6,
            affected_targets: 7,
            changed_files: 8,
            ignored_files: 9,
            rbuildfiles_called: false,
            bzl_closure_hits: 10,
            bzl_witnessed_dead: 16,
            bzl_witnessed_loaded: 17,
            bzl_witness_packages: 18,
            bzl_witness_unavailable: 19,
            bazelrc_safe_diffs: 11,
            cold_rebuild: true,
            repos_dirty_by_input: 11,
            repos_dirty_by_module: 12,
            repos_dirty_by_bzl: 13,
            repos_dirty_by_target: 20,
            repos_label_only: 15,
            external_direct_targets: 14,
            bazelrc_flag_nodes: 16,
        },
        explanations: Some(vec![Explanation {
            target: "//app:main".into(),
            path: vec!["//app:main".into(), "//lib/core:core".into()],
            direct: "//lib/core:core".into(),
            reason: DirectReason::File {
                path: "lib/core/a.cc".into(),
            },
        }]),
    }
}

/// Every field path in a JSON document, in a form that is stable to compare.
fn key_paths(v: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk_value(v, "", &mut out);
    out
}

fn walk_value(v: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                match child {
                    Value::Object(_) | Value::Array(_) => walk_value(child, &path, out),
                    _ => {
                        out.insert(path);
                    }
                }
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            if items
                .iter()
                .all(|i| !matches!(i, Value::Object(_) | Value::Array(_)))
            {
                // An array of scalars is one leaf; its length is not shape.
                out.insert(path);
            } else {
                for item in items {
                    walk_value(item, &path, out);
                }
            }
        }
        _ => {
            out.insert(prefix.to_string());
        }
    }
}

#[test]
fn affected_report_shape_is_pinned() {
    let json = serde_json::to_value(fixture()).expect("report serialises");
    let actual = key_paths(&json);
    let expected: BTreeSet<String> = AFFECTED_SCHEMA.iter().map(|s| s.to_string()).collect();

    let added: Vec<&String> = actual.difference(&expected).collect();
    let removed: Vec<&String> = expected.difference(&actual).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "the affected report's JSON shape changed.\n  \
         added:   {added:?}\n  removed: {removed:?}\n\n\
         This is a contract change: bump SCHEMA_VERSION in crates/engine/src/report.rs, \
         update AFFECTED_SCHEMA here, and update the JSON contract \
         table in the README."
    );
}

#[test]
fn every_document_declares_its_schema() {
    let json = serde_json::to_value(fixture()).unwrap();
    assert_eq!(
        json["schema"], SCHEMA_VERSION,
        "the report must declare the schema version it was written against"
    );
    // A document produced by a cambium from before the field existed has no
    // `schema` key. Parsing must not require it, and must read it as what it
    // is — version 1 — rather than stamping it with today's version.
    let mut without = json.clone();
    without.as_object_mut().unwrap().remove("schema");
    let parsed: AffectedReport =
        serde_json::from_value(without).expect("schema is optional on read");
    assert_eq!(parsed.schema, 1);
}

#[test]
fn a_global_report_still_carries_the_contract() {
    // The global case takes a different construction path in the engine, and
    // its consumers key off the same fields.
    let report = AffectedReport {
        global: true,
        global_reason: Some("MODULE.bazel matches an invalidator".into()),
        ..Default::default()
    };
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["schema"], SCHEMA_VERSION);
    assert_eq!(json["global"], true);
    assert_eq!(report.as_query_expression(false), "//...");
}

#[test]
fn optional_fields_are_omitted_rather_than_null() {
    // Consumers distinguish "absent" from "null"; skip_serializing_if keeps the
    // documents small and the absence meaningful.
    let report = AffectedReport::default();
    let json = serde_json::to_value(&report).unwrap();
    assert!(!json.as_object().unwrap().contains_key("global_reason"));
    assert!(!json.as_object().unwrap().contains_key("explanations"));
}

/// Exit codes are as much a contract as the field names, and are documented in
/// the same README table.
#[test]
fn exit_codes_are_pinned() {
    use std::path::PathBuf;
    assert_eq!(
        EngineError::BaselineUnavailable { base: "x".into() }.exit_code(),
        4,
        "4 means: no baseline and --no-full-build was given"
    );
    assert_eq!(
        EngineError::BazelQuery {
            what: "x".into(),
            code: Some(1),
            stderr: String::new()
        }
        .exit_code(),
        3,
        "3 means: Bazel failed"
    );
    assert_eq!(
        EngineError::Bazel(cambium_bazel::BazelError::NoWorkspace(PathBuf::from(
            "/nowhere"
        )))
        .exit_code(),
        3,
        "no workspace is discovered via Bazel, so it maps like any other Bazel failure"
    );
    // 0 (success) and 2 (global invalidation) are produced by the CLI rather
    // than by an error, and are pinned in crates/cli/src/main.rs.
}
