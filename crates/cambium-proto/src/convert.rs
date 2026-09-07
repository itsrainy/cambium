//! `blaze_query.Target` -> index rows.

use cambium_index::{label, NodeKind, TargetRow};

use crate::blaze_query::{target::Discriminator, Target};
use crate::hash::rule_hash;

/// Knobs that come from `cambium.toml` (spec 9).
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    /// Rule classes ending in one of these are tests.
    pub test_rule_suffixes: Vec<String>,
    /// Tags that mark a target for exclusion from the default output.
    pub exclude_tags: Vec<String>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            test_rule_suffixes: vec!["_test".to_string()],
            exclude_tags: vec!["manual".to_string()],
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
/// packaging tree. Verified against Bazel 7.7.1 and 8.7.0.
fn rule_inputs(r: &crate::blaze_query::Rule) -> Vec<String> {
    use crate::blaze_query::attribute::Discriminator as AttrKind;
    let mut inputs = r.rule_input.clone();
    for a in &r.attribute {
        if a.name != "build_setting_default" || a.r#type != AttrKind::Label as i32 {
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
    let package = label::main_repo_package(&row.label)
        .unwrap_or_default()
        .to_string();
    Some(ParsedTarget { package, row })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blaze_query::{
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
    fn extracts_package_and_inputs() {
        let p = convert(&tagged_rule("cc_library", &[]), &ConvertOptions::default()).unwrap();
        assert_eq!(p.package, "lib/core");
        assert_eq!(p.row.inputs, vec!["//lib/core:a.cc"]);
        assert!(!p.row.is_test);
        assert!(!p.row.manual);
    }

    /// Bazel reports `label_flag` with an empty `rule_input` under
    /// `--noimplicit_deps`; the dependency lives in `build_setting_default`.
    #[test]
    fn a_build_setting_default_is_a_dependency() {
        let t = Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(Rule {
                name: "//distribution:envoy-binary".into(),
                rule_class: "label_flag".into(),
                rule_input: Vec::new(),
                attribute: vec![Attribute {
                    name: "build_setting_default".into(),
                    r#type: AttrKind::Label as i32,
                    string_value: Some("//source/exe:envoy-static".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = convert(&t, &ConvertOptions::default()).unwrap();
        assert_eq!(p.row.inputs, vec!["//source/exe:envoy-static"]);
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
