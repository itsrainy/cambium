//! `blaze_query.Target` -> index rows.

use cambium_index::{label, NodeKind, TargetRow};

use cambium_proto::blaze_query::{target::Discriminator, Target};
use cambium_proto::hash::rule_hash;

/// Knobs that come from `cambium.toml` (spec 9).
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    /// Rule classes ending in one of these are tests.
    pub test_rule_suffixes: Vec<String>,
    /// Tags that mark a target for exclusion from the default output.
    pub exclude_tags: Vec<String>,
    /// The in-tree external repositories this index holds the packages of:
    /// the confirmed repositories the universe query was widened by
    /// (`crate::universe_expression`). A target of one of them is filed under
    /// an external package key; a target of any other repository is still
    /// dropped, because nothing asked for its package and a key without rows
    /// is a claim the index does not keep.
    pub indexed_repos: Vec<String>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            test_rule_suffixes: vec!["_test".to_string()],
            exclude_tags: vec!["manual".to_string()],
            indexed_repos: Vec::new(),
        }
    }
}

impl ConvertOptions {
    fn is_test_class(&self, class: &str) -> bool {
        class == "test_suite" || self.test_rule_suffixes.iter().any(|s| class.ends_with(s))
    }
}

/// The labels a rule depends on.
///
/// Normally that is exactly `rule_input`, but a build setting is a special
/// case: Bazel routes `label_flag` / `label_setting`'s `build_setting_default`
/// through the rule's *implicit* `:alias` attribute, so `--noimplicit_deps`
/// (which we pass, to keep toolchains out of the graph) strips the edge and
/// `rule_input` comes back empty even though the label is a real, explicitly
/// written dependency. Without this, everything reachable only through a
/// `label_flag` is missing from the affected set — envoy's
/// `//distribution:envoy-binary -> //source/exe:envoy-static` and its whole
/// packaging tree.
///
/// **The attribute's declared type is `STRING`, not `LABEL`.** That is the
/// whole reason this was still broken after it was first written: the proto
/// Bazel emits for
///
/// ```text
/// label_flag(name = "flag", build_setting_default = "//src:lib")
/// ```
///
/// carries `type: STRING, string_value: "//src:lib"` — identical under 7.7.1
/// and 8.7.0, and identical for `label_setting`. A `type == LABEL` test
/// therefore never fires on a real repository, only on a hand-built proto,
/// which is exactly what the first test asserted against. So the type filter
/// is gone and the *value* decides: an absolute label (`//…` or `@…`) is
/// taken as an edge.
///
/// That admits one false positive — a `string_flag` whose default happens to
/// spell a label — and that is the direction this crate is allowed to be
/// wrong in: an edge too many is over-approximation, an edge too few is
/// unsoundness.
fn rule_inputs(r: &cambium_proto::blaze_query::Rule) -> Vec<String> {
    let mut inputs = r.rule_input.clone();
    for a in &r.attribute {
        if a.name != "build_setting_default" {
            continue;
        }
        let Some(v) = a.string_value.as_deref() else {
            continue;
        };
        // Bazel writes build-setting defaults as absolute labels; anything
        // else is not something we can resolve here.
        if (v.starts_with("//") || v.starts_with('@')) && !inputs.iter().any(|i| i == v) {
            inputs.push(v.to_string());
        }
    }
    inputs
}

/// A parsed target, with the package that owns it.
#[derive(Clone, Debug)]
pub struct ParsedTarget {
    pub package: String,
    pub row: TargetRow,
}

/// Converts one `Target` message. Returns `None` for message kinds we do not
/// index (environment groups carry no build edges).
pub fn convert(t: &Target, opts: &ConvertOptions) -> Option<ParsedTarget> {
    let row = match Discriminator::try_from(t.r#type).ok()? {
        Discriminator::Rule => {
            let r = t.rule.as_ref()?;
            let tags = r
                .attribute
                .iter()
                .find(|a| a.name == "tags")
                .map(|a| a.string_list_value.as_slice())
                .unwrap_or(&[]);
            let manual = opts
                .exclude_tags
                .iter()
                .any(|x| tags.iter().any(|t| t == x));
            // A `*_test` class or an explicit `test` tag both make a target a
            // test as far as the output filter is concerned (spec 6.7).
            let is_test = opts.is_test_class(&r.rule_class) || tags.iter().any(|t| t == "test");
            TargetRow {
                label: r.name.clone(),
                kind: NodeKind::Rule,
                rule_class: Some(r.rule_class.clone()),
                rule_hash: rule_hash(r),
                is_test,
                manual,
                inputs: rule_inputs(r),
                generating_rule: None,
            }
        }
        Discriminator::SourceFile => {
            let f = t.source_file.as_ref()?;
            TargetRow::source_file(f.name.clone())
        }
        Discriminator::GeneratedFile => {
            let f = t.generated_file.as_ref()?;
            TargetRow {
                label: f.name.clone(),
                kind: NodeKind::GeneratedFile,
                rule_class: None,
                rule_hash: [0; 32],
                is_test: false,
                manual: false,
                inputs: Vec::new(),
                generating_rule: Some(f.generating_rule.clone()),
            }
        }
        Discriminator::PackageGroup => {
            let g = t.package_group.as_ref()?;
            TargetRow {
                label: g.name.clone(),
                kind: NodeKind::PackageGroup,
                rule_class: None,
                rule_hash: [0; 32],
                is_test: false,
                manual: false,
                inputs: Vec::new(),
                generating_rule: None,
            }
        }
        Discriminator::EnvironmentGroup => return None,
    };
    // A label of a repository this index does not hold has no package key,
    // and is dropped. Falling back to `""` here would file every external
    // target under the *root* package, which is a real package with real rows.
    let package = label::package_key(&row.label, &opts.indexed_repos)?;
    Some(ParsedTarget { package, row })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cambium_proto::blaze_query::{
        attribute::Discriminator as AttrKind, Attribute, GeneratedFile, Rule,
    };

    fn tagged_rule(class: &str, tags: &[&str]) -> Target {
        Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(Rule {
                name: "//lib/core:t".into(),
                rule_class: class.into(),
                rule_input: vec!["//lib/core:a.cc".into()],
                attribute: vec![Attribute {
                    name: "tags".into(),
                    r#type: AttrKind::StringList as i32,
                    string_list_value: tags.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn skips_targets_of_repositories_this_index_does_not_hold() {
        // No repository was widened by, so `@dep//pkg:t` has no key; it must
        // be dropped, not filed under the root package.
        let opts = ConvertOptions::default();
        for label in ["@dep//pkg:t", "@@dep~1.0//pkg:t"] {
            let mut t = tagged_rule("cc_library", &[]);
            t.rule.as_mut().unwrap().name = label.into();
            assert!(convert(&t, &opts).is_none(), "{label} should be skipped");
        }
    }

    #[test]
    fn keeps_targets_of_an_indexed_repository_under_an_external_key() {
        let opts = ConvertOptions {
            indexed_repos: vec!["tsl".to_string()],
            ..ConvertOptions::default()
        };
        let mut t = tagged_rule("cc_library", &[]);
        t.rule.as_mut().unwrap().name = "@tsl//tsl/platform:macros".into();
        let p = convert(&t, &opts).unwrap();
        assert_eq!(p.package, "@tsl//tsl/platform");
        assert_eq!(p.row.label, "@tsl//tsl/platform:macros");
        // A repository next to it that was not widened by stays a leaf.
        let mut u = tagged_rule("cc_library", &[]);
        u.rule.as_mut().unwrap().name = "@other//pkg:t".into();
        assert!(convert(&u, &opts).is_none());
    }

    #[test]
    fn extracts_package_and_inputs() {
        let p = convert(&tagged_rule("cc_library", &[]), &ConvertOptions::default()).unwrap();
        assert_eq!(p.package, "lib/core");
        assert_eq!(p.row.inputs, vec!["//lib/core:a.cc"]);
        assert!(!p.row.is_test);
        assert!(!p.row.manual);
    }

    fn build_setting(class: &str, ty: AttrKind, value: &str) -> Target {
        Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(Rule {
                name: "//distribution:envoy-binary".into(),
                rule_class: class.into(),
                rule_input: Vec::new(),
                attribute: vec![Attribute {
                    name: "build_setting_default".into(),
                    r#type: ty as i32,
                    string_value: Some(value.into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Bazel reports `label_flag` with an empty `rule_input` under
    /// `--noimplicit_deps`; the dependency lives in `build_setting_default`.
    ///
    /// **The type is `STRING`.** Captured from a real
    /// `bazel query --output=streamed_proto` on a two-package workspace under
    /// both 7.7.1 and 8.7.0: the attribute comes back
    /// `name: "build_setting_default", type: STRING (2),
    /// string_value: "//src:lib"`. The earlier version of this test built the
    /// attribute as `LABEL` and passed while envoy's whole `//distribution`
    /// tree stayed unreachable, so the type is asserted here explicitly.
    #[test]
    fn a_build_setting_default_is_a_dependency() {
        for class in ["label_flag", "label_setting"] {
            let t = build_setting(class, AttrKind::String, "//source/exe:envoy-static");
            let p = convert(&t, &ConvertOptions::default()).unwrap();
            assert_eq!(
                p.row.inputs,
                vec!["//source/exe:envoy-static"],
                "{class} reports its default as a STRING"
            );
        }
    }

    /// The value, not the declared type, decides. A `LABEL`-typed one is still
    /// taken (older Bazels, and hand-written protos), and a default that is
    /// not a label at all is not an edge.
    #[test]
    fn only_label_shaped_defaults_become_edges() {
        let o = ConvertOptions::default();
        let labelled = build_setting("label_flag", AttrKind::Label, "@repo//pkg:t");
        assert_eq!(
            convert(&labelled, &o).unwrap().row.inputs,
            vec!["@repo//pkg:t"]
        );
        let stringly = build_setting("string_flag", AttrKind::String, "fastbuild");
        assert!(convert(&stringly, &o).unwrap().row.inputs.is_empty());
    }

    #[test]
    fn recognises_tests_and_manual() {
        let o = ConvertOptions::default();
        assert!(
            convert(&tagged_rule("cc_test", &[]), &o)
                .unwrap()
                .row
                .is_test
        );
        assert!(
            convert(&tagged_rule("test_suite", &[]), &o)
                .unwrap()
                .row
                .is_test
        );
        assert!(
            convert(&tagged_rule("sh_binary", &["test"]), &o)
                .unwrap()
                .row
                .is_test
        );
        assert!(
            convert(&tagged_rule("cc_test", &["manual"]), &o)
                .unwrap()
                .row
                .manual
        );
    }

    #[test]
    fn generated_files_carry_their_generator() {
        let t = Target {
            r#type: Discriminator::GeneratedFile as i32,
            generated_file: Some(GeneratedFile {
                name: "//p:out.h".into(),
                generating_rule: "//p:gen".into(),
                location: None,
            }),
            ..Default::default()
        };
        let p = convert(&t, &ConvertOptions::default()).unwrap();
        assert_eq!(p.row.kind, cambium_index::NodeKind::GeneratedFile);
        assert_eq!(p.row.generating_rule.as_deref(), Some("//p:gen"));
    }
}
