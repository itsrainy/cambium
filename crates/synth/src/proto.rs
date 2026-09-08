//! Emitting a synthetic graph as `blaze_query` protos (spec 13.5).
//!
//! Lets the proto parsing and rule hashing stages be benchmarked at any scale
//! without Bazel in the loop.

use cambium_proto::blaze_query::{
    attribute::Discriminator as AttrKind, target::Discriminator, Attribute, Rule, SourceFile,
    Target,
};

use crate::graph::SynthGraph;

fn string_list(name: &str, values: Vec<String>) -> Attribute {
    Attribute {
        name: name.to_string(),
        r#type: AttrKind::StringList as i32,
        string_list_value: values,
        explicitly_specified: Some(true),
        ..Default::default()
    }
}

fn string_attr(name: &str, value: String) -> Attribute {
    Attribute {
        name: name.to_string(),
        r#type: AttrKind::String as i32,
        string_value: Some(value),
        explicitly_specified: Some(true),
        ..Default::default()
    }
}

/// The `.bzl` files a package's BUILD file loads, as instantiation frames.
///
/// Mirrors what `emit_package` writes: a package with generated `.bzl` loads
/// those, and a package with none loads the shared `//rules:synth.bzl`.
fn instantiation_stack(g: &SynthGraph, pkg: u32) -> Vec<String> {
    let bzl = &g.packages[pkg as usize].bzl;
    if bzl.is_empty() {
        return vec!["rules/synth.bzl:1:1".to_string()];
    }
    bzl.iter()
        .map(|&b| format!("{}:1:1", g.bzl[b as usize].path))
        .collect()
}

/// The BUILD line a target's macro call would sit on.
///
/// Real Bazel reports `generator_location` as `pkg/BUILD:line:col` for every
/// macro-instantiated rule, and its value is a function of *position in the
/// file*, so inserting or deleting a target shifts every later target's value
/// without changing anything those targets build. Reproducing that here is what
/// lets the synthetic sweeps exercise the attribute — `canonical_rule` drops
/// it, so a shift must move no hash.
///
/// `LINES_PER_TARGET` is a plausible stanza height; the column is 1 because a
/// top-level macro call starts at the margin.
const LINES_PER_TARGET: usize = 7;

fn generator_location(g: &SynthGraph, pkg: u32, ordinal: usize) -> String {
    format!(
        "{}/BUILD.bazel:{}:1",
        g.package_path(pkg),
        ordinal * LINES_PER_TARGET + 1
    )
}

/// Every target in the graph, as Bazel would report it.
pub fn to_targets(g: &SynthGraph) -> Vec<Target> {
    let mut out = Vec::with_capacity(g.num_alive_targets() * 2);
    for pkg in 0..g.packages.len() as u32 {
        if !g.packages[pkg as usize].alive {
            continue;
        }
        let mut ordinal = 0usize;
        for &tid in &g.packages[pkg as usize].targets {
            let t = &g.targets[tid as usize];
            if !t.alive {
                continue;
            }
            let location = generator_location(g, pkg, ordinal);
            ordinal += 1;
            let deps: Vec<String> = t
                .deps
                .iter()
                .filter(|&&d| g.targets[d as usize].alive)
                .map(|&d| g.target_label(d))
                .chain((0..t.ext_deps).map(|k| SynthGraph::ext_label(tid, k)))
                .collect();
            let srcs: Vec<String> = t
                .srcs
                .iter()
                .map(|&s| g.src_label(tid, s))
                .chain(
                    t.foreign_srcs
                        .iter()
                        .map(|&(_, ft, fs)| g.src_label(ft, fs)),
                )
                .collect();
            let mut rule_input: Vec<String> = deps.iter().chain(srcs.iter()).cloned().collect();
            rule_input.sort();
            rule_input.dedup();

            let mut attribute = vec![
                string_list("deps", deps),
                string_list("srcs", srcs),
                string_attr("salt", t.attr_salt.to_string()),
                // The `generator_*` family a macro-made rule carries. Bazel
                // reports all three as explicitly specified even under
                // `--proto:default_values=false`, so a dump without them would
                // not exercise the canonicalisation that drops the location.
                string_attr("generator_location", location),
                string_attr("generator_name", format!("t{tid}")),
                string_attr("generator_function", t.kind.rule_class().to_string()),
            ];
            if t.is_manual() {
                attribute.push(string_list("tags", vec!["manual".to_string()]));
            }
            out.push(Target {
                r#type: Discriminator::Rule as i32,
                rule: Some(Rule {
                    name: g.target_label(tid),
                    rule_class: t.kind.rule_class().to_string(),
                    // Locations are what a real query reports and what the
                    // canonical hash has to strip, so emit them.
                    location: Some(format!("/repo/{}/BUILD.bazel:1:1", g.package_path(pkg))),
                    // The macro frames a real `--proto:instantiation_stack`
                    // query reports. cambium's canonical hash strips them, but
                    // they are how a consumer of this dump learns which `.bzl`
                    // defined a rule — bazel-diff seeds each rule's digest from
                    // exactly these paths — so a dump without them would hide
                    // every `.bzl` edit from anything measured against it.
                    instantiation_stack: instantiation_stack(g, pkg),
                    attribute,
                    rule_input,
                    ..Default::default()
                }),
                ..Default::default()
            });
            for &s in &t.srcs {
                out.push(Target {
                    r#type: Discriminator::SourceFile as i32,
                    source_file: Some(SourceFile {
                        name: g.src_label(tid, s),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
    }
    out
}

/// The same, framed the way `--output=streamed_proto` frames it.
pub fn to_streamed_proto(g: &SynthGraph) -> Vec<u8> {
    cambium_proto::encode_stream(&to_targets(g))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::GraphSpec;

    #[test]
    fn round_trips_through_the_reader() {
        let g = crate::generate(&GraphSpec::preset("tiny").unwrap());
        let bytes = to_streamed_proto(&g);
        let decoded = cambium_proto::decode_all(&bytes[..]).unwrap();
        assert_eq!(decoded.len(), to_targets(&g).len());
        let rules = decoded.iter().filter(|t| t.rule.is_some()).count();
        assert_eq!(rules, g.num_alive_targets());
    }

    #[test]
    fn emitted_protos_carry_the_graph_edges() {
        let g = crate::generate(&GraphSpec::preset("tiny").unwrap());
        let targets = to_targets(&g);
        let r = targets
            .iter()
            .find_map(|t| t.rule.as_ref())
            .expect("at least one rule");
        let id: u32 = r.name.rsplit(":t").next().unwrap().parse().unwrap();
        for &d in &g.targets[id as usize].deps {
            assert!(r.rule_input.contains(&g.target_label(d)));
        }
    }
}
