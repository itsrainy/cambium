//! A BUILD-line shift must not make a single target direct.
//!
//! `generator_location` is the `pkg/BUILD:line:col` of the macro call that
//! instantiated a rule, so inserting one line at the top of a BUILD file moves
//! it for every target below — on the `bazel/bazel` corpus that produced
//! thousands of `rule definition changed` direct targets on commits that
//! changed nothing anyone builds. `canonical_rule` drops the attribute; this
//! test drives the whole engine over it rather than just the hash function.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::query::{no_files, parse_stream};
use cambium_engine::{AffectedRequest, Config, Engine, PackageQuerier};
use cambium_index::PackageUpdate;
use cambium_synth::{generate, GraphSpec};

/// Serves precomputed package rows, so the test can hand the engine protos it
/// built itself rather than a `SynthGraph` (whose rows never go through
/// `canonical_rule`).
struct Canned(BTreeMap<String, PackageUpdate>);

impl Canned {
    fn new(updates: Vec<PackageUpdate>) -> Self {
        Canned(
            updates
                .into_iter()
                .map(|u| (u.package.clone(), u))
                .collect(),
        )
    }

    fn packages(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}

impl PackageQuerier for Canned {
    fn query(&self, packages: &[String]) -> cambium_engine::Result<Vec<PackageUpdate>> {
        Ok(packages
            .iter()
            .map(|p| {
                self.0.get(p).cloned().unwrap_or(PackageUpdate {
                    package: p.clone(),
                    ..Default::default()
                })
            })
            .collect())
    }

    fn universe(&self) -> cambium_engine::Result<cambium_engine::query::Universe> {
        Ok(self.0.values().cloned().collect::<Vec<_>>().into())
    }

    fn version(&self) -> String {
        "canned".to_string()
    }
}

fn engine() -> Engine {
    let dir = std::env::temp_dir().join("cambium-tests");
    let _ = std::fs::create_dir_all(&dir);
    Engine::with_parts(
        Config::default(),
        cambium_diff::Repo::new(dir.clone()),
        cambium_bazel::Bazel::at(dir, Some("/nonexistent/bazel")),
    )
}

#[test]
fn shifting_every_generator_location_affects_nothing() {
    let g = generate(&GraphSpec::preset("small").unwrap());
    let opts = cambium_proto::ConvertOptions::default();

    let base_bytes = cambium_synth::proto::to_streamed_proto(&g);
    let base_updates = parse_stream(&base_bytes, &opts, &no_files()).unwrap();

    // The head revision: the same graph, with a line inserted at the top of
    // every BUILD file. Nothing about any rule changes but its recorded line.
    let mut targets = cambium_synth::proto::to_targets(&g);
    let mut shifted = 0usize;
    for t in &mut targets {
        let Some(rule) = t.rule.as_mut() else {
            continue;
        };
        for a in &mut rule.attribute {
            if a.name == "generator_location" {
                let old = a.string_value.clone().expect("a location string");
                let (file, rest) = old.split_once(':').expect("pkg/BUILD:line:col");
                let (line, col) = rest.split_once(':').expect("line:col");
                let line: usize = line.parse().unwrap();
                a.string_value = Some(format!("{file}:{}:{col}", line + 1));
                shifted += 1;
            }
        }
    }
    assert!(shifted > 0, "the synth emitter must emit the attribute");
    let head_bytes = cambium_proto::encode_stream(&targets);
    let head_updates = parse_stream(&head_bytes, &opts, &no_files()).unwrap();

    // The rows themselves first: identical hashes, label for label.
    let base_hashes: BTreeMap<&str, [u8; 32]> = base_updates
        .iter()
        .flat_map(|u| u.targets.iter())
        .map(|t| (t.label.as_str(), t.rule_hash))
        .collect();
    for t in head_updates.iter().flat_map(|u| u.targets.iter()) {
        assert_eq!(
            base_hashes.get(t.label.as_str()),
            Some(&t.rule_hash),
            "{} moved on a line shift",
            t.label
        );
    }

    // And then the engine, told that every BUILD file changed.
    let e = engine();
    let base_q = Canned::new(base_updates);
    let head_q = Canned::new(head_updates);
    let index = e.full_build_with("c0", &base_q).unwrap();
    let changes: Vec<Change> = base_q
        .packages()
        .map(|p| Change::new(Status::Modified, format!("{p}/BUILD.bazel")))
        .collect();
    let out = e
        .compute(
            index.view(),
            &changes,
            &head_q,
            &AffectedRequest {
                base: "base".into(),
                head: "head".into(),
                include_manual: true,
                explain: false,
                no_full_build: true,
                materialize: true,
                publish_base: false,
                max_depth: None,
            },
        )
        .unwrap();
    assert!(
        !out.report.global,
        "a BUILD-file edit is not a global invalidation"
    );
    assert!(
        out.report.direct.is_empty(),
        "line shifts made {} targets direct: {:?}",
        out.report.direct.len(),
        &out.report.direct[..out.report.direct.len().min(5)]
    );
    assert!(out.report.affected.is_empty());
}
