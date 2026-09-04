//! Emitting a synthetic graph as `blaze_query` protos (spec 13.5).
//!
//! Lets the proto parsing and rule hashing stages be benchmarked at any scale
//! without Bazel in the loop.

use cambium_proto::blaze_query::{
    attribute::Discriminator as AttrKind, target::Discriminator, Attribute, Rule, SourceFile,
    Target,
};

use crate::graph::{Kind, SynthGraph};

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

/// Every target in the graph, as Bazel would report it.
pub fn to_targets(g: &SynthGraph) -> Vec<Target> {
    let mut out = Vec::with_capacity(g.num_alive_targets() * 2);
    for pkg in 0..g.packages.len() as u32 {
        if !g.packages[pkg as usize].alive {
            continue;
        }
        for &tid in &g.packages[pkg as usize].targets {
            let t = &g.targets[tid as usize];
            if !t.alive {
                continue;
            }
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
                .chain(t.foreign_srcs.iter().map(|&(_, ft, fs)| g.src_label(ft, fs)))
                .collect();
            let mut rule_input: Vec<String> = deps.iter().chain(srcs.iter()).cloned().collect();
            rule_input.sort();
            rule_input.dedup();

            let mut attribute = vec![
                string_list("deps", deps),
                string_list("srcs", srcs),
                string_attr("salt", t.attr_salt.to_string()),
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
            let _ = Kind::Lib;
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
        let rules = decoded
            .iter()
            .filter(|t| t.rule.is_some())
            .count();
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
