//! Why two tools disagreed.
//!
//! A raw symmetric difference between cambium's answer and bazel-diff's is not
//! actionable: on a real repository it is thousands of labels, almost all of
//! them explained by a handful of known, *expected* differences in what the
//! two tools model. The spec names seven classes and this file adds an eighth
//! (`cambium_extra_bzl`, which the first real run turned up); six are benign
//! and two fail the run:
//!
//! | class | side | meaning |
//! |---|---|---|
//! | `cambium_extra_select` | cambium only | reached through a `select` branch that is not taken in the current configuration; cambium indexes the unconfigured graph, so it keeps every branch |
//! | `cambium_extra_implicit` | cambium only | reached only through an implicit (`$`-prefixed) attribute edge |
//! | `cambium_extra_bzl` | cambium only | the target's own rule is byte-identical at both revisions, but a `.bzl` its package loads changed, so cambium marked the whole loading package direct |
//! | `cambium_extra_visibility` | cambium only | the target's rule changed only in `visibility`/`tags`, which cannot change its outputs but does move a hash |
//! | `bazeldiff_extra_content` | producer only | a changed file whose content bazel-diff hashes but which no target in the index consumes |
//! | `bazeldiff_extra_external` | producer only | an `@repo//...` label; cambium indexes `//...` only |
//! | `cambium_missing` | producer only | **fails**: the other tool reached something cambium did not, with no benign explanation |
//! | `unclassified` | cambium only | **fails**: an extra target nothing explains — either a real over-approximation bug or a missing predicate here |
//!
//! The classifier is a pure function of a [`Facts`] oracle so every class can
//! be tested without a Bazel; [`BazelFacts`] is the implementation that
//! actually asks one.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Which tool reported the label that the other did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// cambium reported it and the producer did not.
    Cambium,
    /// The producer reported it and cambium did not.
    Producer,
}

/// The eight disagreement classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    CambiumExtraSelect,
    CambiumExtraImplicit,
    CambiumExtraVisibility,
    /// A `.bzl` edit marked the whole loading package direct, but this
    /// target's own rule did not move.
    CambiumExtraBzl,
    BazeldiffExtraContent,
    BazeldiffExtraExternal,
    /// A failure: the other tool found something cambium missed.
    CambiumMissing,
    /// A failure: no predicate explained the disagreement.
    Unclassified,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::CambiumExtraSelect => "cambium_extra_select",
            Class::CambiumExtraImplicit => "cambium_extra_implicit",
            Class::CambiumExtraVisibility => "cambium_extra_visibility",
            Class::CambiumExtraBzl => "cambium_extra_bzl",
            Class::BazeldiffExtraContent => "bazeldiff_extra_content",
            Class::BazeldiffExtraExternal => "bazeldiff_extra_external",
            Class::CambiumMissing => "cambium_missing",
            Class::Unclassified => "unclassified",
        }
    }

    /// Whether a label in this class fails the pair.
    pub fn is_failure(self) -> bool {
        matches!(self, Class::CambiumMissing | Class::Unclassified)
    }

    pub const ALL: [Class; 8] = [
        Class::CambiumExtraSelect,
        Class::CambiumExtraImplicit,
        Class::CambiumExtraVisibility,
        Class::CambiumExtraBzl,
        Class::BazeldiffExtraContent,
        Class::BazeldiffExtraExternal,
        Class::CambiumMissing,
        Class::Unclassified,
    ];
}

impl std::fmt::Display for Class {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The questions the classifier asks about a label. Every one of them is
/// answerable from Bazel plus the head index; none of them needs the producer.
pub trait Facts {
    /// The label names a target outside the main repository (`@repo//...`, or
    /// the older `//external:...`).
    fn is_external(&self, label: &str) -> bool;

    /// Some attribute on the path from the change to this target is a
    /// `select`, so a configured build might not reach it.
    fn reachable_via_select(&self, label: &str) -> bool;

    /// The target is reached from the change only through implicit
    /// (rule-supplied, `$`-prefixed) attributes.
    fn reachable_only_via_implicit(&self, label: &str) -> bool;

    /// Between base and head this target's rule changed only in visibility (or
    /// tags): nothing that can change its outputs.
    fn visibility_only_change(&self, label: &str) -> bool;

    /// A `.bzl` file changed in this pair and this target's own rule proto is
    /// byte-identical at base and head.
    ///
    /// cambium marks *every* rule in a package whose `.bzl` load closure moved
    /// as direct, because a macro can change what a rule builds without moving
    /// a single byte of its proto (see `engine::direct_set`). The price is
    /// that the rules in the package which the macro does not touch — a
    /// hand-written `config_setting` next to a macro-generated genrule — are
    /// reported too. That is over-approximation, not unsoundness, and on a
    /// real repository it is the single largest source of cambium-side extras.
    fn bzl_closure_only(&self, label: &str) -> bool;

    /// The label corresponds to a path that changed in this pair but that no
    /// target in the head index consumes — a file bazel-diff hashes and
    /// cambium correctly has no consumer for.
    fn changed_path_without_consumer(&self, label: &str) -> bool;
}

/// Classifies one label of the symmetric difference.
///
/// Order matters only in that the cheap, certain predicates come first:
/// `external` before `content`, and on cambium's side `select` before
/// `implicit` before `visibility`, so a target explained by more than one is
/// attributed to the most specific reason. Anything left over is a failure —
/// deliberately, because a silent "other" bucket is how an unsoundness gets
/// shipped.
pub fn classify(label: &str, side: Side, facts: &dyn Facts) -> Class {
    match side {
        Side::Cambium => {
            if facts.reachable_via_select(label) {
                Class::CambiumExtraSelect
            } else if facts.reachable_only_via_implicit(label) {
                Class::CambiumExtraImplicit
            } else if facts.visibility_only_change(label) {
                Class::CambiumExtraVisibility
            } else if facts.bzl_closure_only(label) {
                Class::CambiumExtraBzl
            } else {
                Class::Unclassified
            }
        }
        Side::Producer => {
            if facts.is_external(label) {
                Class::BazeldiffExtraExternal
            } else if facts.changed_path_without_consumer(label) {
                Class::BazeldiffExtraContent
            } else {
                Class::CambiumMissing
            }
        }
    }
}

// ----------------------------------------------------------------- BazelFacts

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use cambium_bazel::Bazel;
use cambium_index::IndexRef;
use cambium_proto::blaze_query::{attribute::Discriminator, Rule, Target};

/// A [`Facts`] implementation backed by Bazel and the head index.
///
/// It caches every rule proto it fetches: a pair's symmetric difference asks
/// about the same handful of packages repeatedly, and each miss is a Bazel
/// query.
///
/// **On `cambium_extra_implicit`.** The index is built with `--noimplicit_deps`
/// and records no edge kind, so there is no "implicit" edge to test directly.
/// The heuristic used instead: fetch the target's own rule and split its
/// declared dependency labels into explicit ones (attributes whose names do
/// not start with `$` or `:`) and the full `rule_input` set, which Bazel
/// populates from *all* attributes including the implicit ones. If none of the
/// explicit dependency labels is in the direct set but some other `rule_input`
/// is, the only way the change reaches this target is an implicit attribute.
/// That is an approximation in one direction — it only looks one hop back — so
/// it is conservative: a multi-hop implicit path lands in `unclassified` and
/// gets a human's attention rather than being waved through.
pub struct BazelFacts<'a> {
    head: &'a Bazel,
    /// A Bazel rooted at the base worktree, for the visibility comparison.
    /// `None` disables that class (it then reads as `unclassified`, which is
    /// the safe direction).
    base: Option<Bazel>,
    index: IndexRef<'a>,
    /// cambium's depth-0 set for this pair.
    direct: BTreeSet<String>,
    /// Workspace-relative paths `git diff --name-only base..head` reported.
    changed_paths: BTreeSet<String>,
    cache: RefCell<HashMap<String, Option<Rule>>>,
    base_cache: RefCell<HashMap<String, Option<Rule>>>,
}

impl<'a> BazelFacts<'a> {
    pub fn new(
        head: &'a Bazel,
        index: IndexRef<'a>,
        direct: impl IntoIterator<Item = String>,
        changed_paths: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            head,
            base: None,
            index,
            direct: direct.into_iter().collect(),
            changed_paths: changed_paths.into_iter().collect(),
            cache: RefCell::new(HashMap::new()),
            base_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Adds the base-side Bazel, enabling `cambium_extra_visibility`.
    pub fn with_base(mut self, base: Bazel) -> Self {
        self.base = Some(base);
        self
    }

    /// `git diff --name-only base..head` in `repo`.
    pub fn changed_paths(repo: &Path, base: &str, head: &str) -> Result<BTreeSet<String>> {
        let out = crate::producer::git(repo, &["diff", "--name-only", &format!("{base}..{head}")])?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn rule(
        &self,
        bazel: &Bazel,
        cache: &RefCell<HashMap<String, Option<Rule>>>,
        label: &str,
    ) -> Option<Rule> {
        if let Some(hit) = cache.borrow().get(label) {
            return hit.clone();
        }
        let got = query_rule(bazel, label);
        cache.borrow_mut().insert(label.to_string(), got.clone());
        got
    }

    fn head_rule(&self, label: &str) -> Option<Rule> {
        self.rule(self.head, &self.cache, label)
    }
}

/// One rule's proto at a revision, or `None` when Bazel cannot produce it.
///
/// `--proto:flatten_selects=false` is the point of the call: flattened, an
/// attribute's `select` branches are merged into one list and the
/// `selector_list` field is gone, so there is no way to tell a `select`ed dep
/// from an unconditional one.
fn query_rule(bazel: &Bazel, label: &str) -> Option<Rule> {
    let out = bazel
        .command("query")
        .args([
            label,
            "--output=streamed_proto",
            "--proto:flatten_selects=false",
            "--proto:locations=false",
            "--noimplicit_deps",
            "--order_output=no",
            "--keep_going",
            "--noshow_progress",
            "--ui_event_filters=-info",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        tracing::debug!(label, "bazel query for the rule proto failed");
        return None;
    }
    let targets: Vec<Target> = cambium_proto::decode_all(out.stdout.as_slice()).ok()?;
    targets.into_iter().find_map(|t| t.rule)
}

/// The labels a rule names in attributes the BUILD file wrote, and whether any
/// of those attributes is a `select`.
///
/// Both selector branches and plain values count as declared labels: cambium's
/// edges are the union over branches, so the comparison has to look at the
/// same union.
pub fn explicit_dep_labels(rule: &Rule) -> (BTreeSet<String>, bool) {
    let mut labels = BTreeSet::new();
    let mut any_select = false;
    for a in &rule.attribute {
        if a.name.starts_with('$') || a.name.starts_with(':') {
            continue; // implicit, rule-supplied
        }
        // Under a selector list the attribute's own discriminator is
        // SELECTOR_LIST; the type it evaluates to lives on the list.
        let discriminator = match &a.selector_list {
            Some(sel) => sel.r#type.unwrap_or(a.r#type),
            None => a.r#type,
        };
        let is_label = matches!(
            Discriminator::try_from(discriminator),
            Ok(Discriminator::Label)
                | Ok(Discriminator::LabelList)
                | Ok(Discriminator::LabelKeyedStringDict)
        );
        if !is_label {
            continue;
        }
        if let Some(sel) = &a.selector_list {
            any_select = true;
            for s in &sel.elements {
                for e in &s.entries {
                    labels.extend(e.string_list_value.iter().cloned());
                    labels.extend(e.string_value.iter().cloned());
                }
            }
            continue;
        }
        labels.extend(a.string_list_value.iter().cloned());
        labels.extend(a.string_value.iter().cloned());
    }
    (labels, any_select)
}

/// A rule stripped of everything that cannot change its outputs, for the
/// visibility comparison: the canonical form used for hashing, minus
/// `visibility` and `tags`.
pub fn rule_without_visibility(rule: &Rule) -> Rule {
    let mut r = cambium_proto::canonical_rule(rule);
    r.attribute
        .retain(|a| a.name != "visibility" && a.name != "tags");
    r
}

/// `//pkg:name` -> `pkg/name`, the workspace-relative path of a source label.
/// `None` for anything that is not a main-repository label.
pub fn label_to_path(label: &str) -> Option<String> {
    let rest = label.strip_prefix("//")?;
    let (pkg, name) = rest.split_once(':').unwrap_or((rest, ""));
    if name.is_empty() {
        return None;
    }
    Some(if pkg.is_empty() {
        name.to_string()
    } else {
        format!("{pkg}/{name}")
    })
}

impl Facts for BazelFacts<'_> {
    fn is_external(&self, label: &str) -> bool {
        label.starts_with('@') || label.starts_with("//external:")
    }

    fn reachable_via_select(&self, label: &str) -> bool {
        // The target itself, or any of its direct dependencies that is in the
        // direct set, declares a `select`ed dependency attribute. One hop is
        // enough in practice: an extra target is reached *because* the edge
        // into it survived configuration, and that edge is the one being
        // tested.
        match self.head_rule(label) {
            Some(rule) => explicit_dep_labels(&rule).1,
            None => false,
        }
    }

    fn reachable_only_via_implicit(&self, label: &str) -> bool {
        let Some(rule) = self.head_rule(label) else {
            return false;
        };
        let (explicit, _) = explicit_dep_labels(&rule);
        let touches_direct = |set: &BTreeSet<String>| set.iter().any(|l| self.direct.contains(l));
        let all: BTreeSet<String> = rule.rule_input.iter().cloned().collect();
        !touches_direct(&explicit) && touches_direct(&all)
    }

    fn visibility_only_change(&self, label: &str) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            return false;
        };
        use prost::Message as _;
        let (hb, bb) = (
            rule_without_visibility(&h).encode_to_vec(),
            rule_without_visibility(&b).encode_to_vec(),
        );
        // Equal once visibility and tags are gone, but not equal with them:
        // that is exactly a visibility-only edit.
        hb == bb
            && cambium_proto::canonical_rule(&h).encode_to_vec()
                != cambium_proto::canonical_rule(&b).encode_to_vec()
    }

    fn bzl_closure_only(&self, label: &str) -> bool {
        if !self.changed_paths.iter().any(|p| p.ends_with(".bzl")) {
            return false;
        }
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            return false;
        };
        use prost::Message as _;
        // Identical proto at both ends: nothing about this rule changed, so
        // the only reason cambium could have reported it is its package's
        // load closure.
        cambium_proto::canonical_rule(&h).encode_to_vec()
            == cambium_proto::canonical_rule(&b).encode_to_vec()
    }

    fn changed_path_without_consumer(&self, label: &str) -> bool {
        let Some(path) = label_to_path(label) else {
            return false;
        };
        if !self.changed_paths.contains(&path) {
            return false;
        }
        match self.index.id_of(label) {
            Some(id) => self.index.file_consumers(id).is_empty(),
            // The index does not know the file at all, so nothing consumes it.
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built oracle: every predicate is a membership test, so a test
    /// says exactly which fact it is exercising.
    #[derive(Default)]
    struct Hand {
        external: BTreeSet<String>,
        select: BTreeSet<String>,
        implicit: BTreeSet<String>,
        visibility: BTreeSet<String>,
        bzl: BTreeSet<String>,
        content: BTreeSet<String>,
    }

    impl Hand {
        fn with(mut f: impl FnMut(&mut Hand)) -> Hand {
            let mut h = Hand::default();
            f(&mut h);
            h
        }
    }

    impl Facts for Hand {
        fn is_external(&self, l: &str) -> bool {
            self.external.contains(l)
        }
        fn reachable_via_select(&self, l: &str) -> bool {
            self.select.contains(l)
        }
        fn reachable_only_via_implicit(&self, l: &str) -> bool {
            self.implicit.contains(l)
        }
        fn visibility_only_change(&self, l: &str) -> bool {
            self.visibility.contains(l)
        }
        fn bzl_closure_only(&self, l: &str) -> bool {
            self.bzl.contains(l)
        }
        fn changed_path_without_consumer(&self, l: &str) -> bool {
            self.content.contains(l)
        }
    }

    fn one(field: fn(&mut Hand) -> &mut BTreeSet<String>, side: Side) -> Class {
        let h = Hand::with(|h| {
            field(h).insert("//a:b".to_string());
        });
        classify("//a:b", side, &h)
    }

    #[test]
    fn a_selected_dependency_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.select, Side::Cambium),
            Class::CambiumExtraSelect
        );
    }

    #[test]
    fn an_implicit_only_path_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.implicit, Side::Cambium),
            Class::CambiumExtraImplicit
        );
    }

    #[test]
    fn a_visibility_only_edit_explains_a_cambium_extra() {
        assert_eq!(
            one(|h| &mut h.visibility, Side::Cambium),
            Class::CambiumExtraVisibility
        );
    }

    #[test]
    fn an_unmoved_rule_in_a_bzl_dirty_package_explains_a_cambium_extra() {
        assert_eq!(one(|h| &mut h.bzl, Side::Cambium), Class::CambiumExtraBzl);
        assert!(!Class::CambiumExtraBzl.is_failure());
    }

    #[test]
    fn an_external_label_explains_a_producer_extra() {
        assert_eq!(
            one(|h| &mut h.external, Side::Producer),
            Class::BazeldiffExtraExternal
        );
    }

    #[test]
    fn an_unconsumed_changed_file_explains_a_producer_extra() {
        assert_eq!(
            one(|h| &mut h.content, Side::Producer),
            Class::BazeldiffExtraContent
        );
    }

    #[test]
    fn an_unexplained_producer_extra_is_unsoundness() {
        let c = classify("//a:b", Side::Producer, &Hand::default());
        assert_eq!(c, Class::CambiumMissing);
        assert!(c.is_failure());
    }

    #[test]
    fn an_unexplained_cambium_extra_is_unclassified_and_fails() {
        let c = classify("//a:b", Side::Cambium, &Hand::default());
        assert_eq!(c, Class::Unclassified);
        assert!(c.is_failure());
    }

    #[test]
    fn the_benign_classes_do_not_fail_a_pair() {
        for c in Class::ALL.iter().filter(|c| !c.is_failure()) {
            assert!(c.name().contains("extra"), "{c}");
        }
        assert_eq!(Class::CambiumMissing.to_string(), "cambium_missing");
    }

    #[test]
    fn side_decides_which_predicates_apply() {
        // A label that is external *and* selected: the side settles it, so a
        // producer-only external label is never filed as a cambium extra.
        let h = Hand::with(|h| {
            h.external.insert("//a:b".into());
            h.select.insert("//a:b".into());
        });
        assert_eq!(
            classify("//a:b", Side::Producer, &h),
            Class::BazeldiffExtraExternal
        );
        assert_eq!(
            classify("//a:b", Side::Cambium, &h),
            Class::CambiumExtraSelect
        );
    }

    #[test]
    fn labels_map_to_workspace_paths() {
        assert_eq!(
            label_to_path("//lib/core:a.cc").as_deref(),
            Some("lib/core/a.cc")
        );
        assert_eq!(label_to_path("//:top.txt").as_deref(), Some("top.txt"));
        assert_eq!(label_to_path("@ext//a:b"), None);
        assert_eq!(label_to_path("//lib/core"), None, "a package is not a file");
    }

    fn attr(name: &str, values: &[&str]) -> cambium_proto::blaze_query::Attribute {
        cambium_proto::blaze_query::Attribute {
            name: name.into(),
            r#type: Discriminator::LabelList as i32,
            string_list_value: values.iter().map(|s| s.to_string()).collect(),
            explicitly_specified: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn selected_dep_attributes_are_seen_unflattened() {
        use cambium_proto::blaze_query::{
            attribute::Selector, attribute::SelectorEntry, attribute::SelectorList,
        };
        let mut deps = attr("deps", &[]);
        deps.r#type = Discriminator::SelectorList as i32;
        deps.selector_list = Some(SelectorList {
            r#type: Some(Discriminator::LabelList as i32),
            elements: vec![Selector {
                entries: vec![SelectorEntry {
                    label: Some("//conditions:default".into()),
                    string_list_value: vec!["//lib:a".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        });
        let rule = Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            attribute: vec![deps, attr("srcs", &["//p:s.cc"])],
            ..Default::default()
        };
        let (labels, any_select) = explicit_dep_labels(&rule);
        assert!(
            any_select,
            "the select survives --proto:flatten_selects=false"
        );
        assert!(labels.contains("//lib:a") && labels.contains("//p:s.cc"));
    }

    #[test]
    fn implicit_attributes_are_not_explicit_dependencies() {
        let rule = Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            attribute: vec![
                attr("$cc_toolchain", &["//tools:cc"]),
                attr("deps", &["//lib:a"]),
            ],
            rule_input: vec!["//tools:cc".into(), "//lib:a".into()],
            ..Default::default()
        };
        let (labels, _) = explicit_dep_labels(&rule);
        assert!(labels.contains("//lib:a"));
        assert!(
            !labels.contains("//tools:cc"),
            "$-prefixed attrs are implicit"
        );
    }

    #[test]
    fn visibility_and_tags_drop_out_of_the_comparable_rule() {
        let a = Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            attribute: vec![
                attr("deps", &["//lib:a"]),
                attr("visibility", &["//x:__pkg__"]),
            ],
            ..Default::default()
        };
        let mut b = a.clone();
        b.attribute[1] = attr("visibility", &["//visibility:public"]);
        use prost::Message as _;
        assert_eq!(
            rule_without_visibility(&a).encode_to_vec(),
            rule_without_visibility(&b).encode_to_vec()
        );
        assert_ne!(
            cambium_proto::canonical_rule(&a).encode_to_vec(),
            cambium_proto::canonical_rule(&b).encode_to_vec(),
            "the hash does move, which is why cambium reports the target"
        );
    }
}
