//! Canonical rule hashing (spec 6.6, 7.3).
//!
//! The hash must change when anything that could change a target's outputs
//! changes, and must *not* change for cosmetic edits — a moved line, a
//! reordered `tags` list, a default value that Bazel started reporting. So the
//! rule proto is canonicalised before hashing: positional and diagnostic
//! fields are cleared, attributes are sorted, order-insensitive list values are
//! sorted, and attributes Bazel filled in from the rule definition are dropped
//! (those are a function of the `.bzl` closure, which is compared separately at
//! package level).

use prost::Message;

use crate::blaze_query::Rule;

/// Attributes whose list order carries no meaning.
const ORDER_INSENSITIVE: &[&str] = &["tags", "visibility", "features"];

/// Attributes Bazel reports that describe *where* a rule was written rather
/// than *what* it builds.
///
/// `generator_location` is the `pkg/BUILD:line:col` of the macro call that
/// instantiated the rule, so every rule below an inserted line in a BUILD file
/// gets a new value on a commit that changed nothing about it. On the
/// `bazel/bazel` subject that alone produced thousands of `rule definition
/// changed` direct targets per line-shifting commit. bazel-diff drops it
/// (`DEFAULT_IGNORED_ATTRS`) and so does Uber's Tango (`sourcehasher.go`);
/// cambium now does too.
///
/// `generator_name` and `generator_function` are deliberately *kept*: they name
/// the macro and the symbol that made the rule, they do not move when a line
/// does, and a rule that switched macros really is a different rule.
///
/// Note that these attributes survive `--proto:default_values=false`: for a
/// macro-instantiated rule Bazel reports the whole `generator_*` family as
/// explicitly specified, so `explicitly_specified` retention does not filter
/// them out and this list has to.
const LOCATION_ATTRS: &[&str] = &["generator_location"];

/// Version of the canonicalisation this module implements.
///
/// It is mixed into the engine's config hash (`cambium_engine::Config::hash`),
/// which is stamped into every index and checked before a splice. Bumping it
/// therefore makes every index built by an older cambium incompatible with this
/// one, so hashes computed under two different canonicalisations can never be
/// compared against each other. Bump it whenever [`canonical_rule`] starts or
/// stops keeping a field, or whenever the *edges* an index records change:
/// both decide whether an old index can be spliced into a new one.
///
/// - 1: the original canonicalisation.
/// - 2: `generator_location` dropped.
/// - 3: `build_setting_default` became a dependency edge
///   (see [`crate::convert`]), so older indexes are missing every edge out of
///   a `label_flag` or `label_setting`.
/// - 4: that edge actually fires. Version 3 gated on the attribute's type
///   being `LABEL`; Bazel types `build_setting_default` as `STRING`, so no
///   index built under 3 has the edge either.
pub const RULE_HASH_VERSION: u32 = 4;

/// Returns the canonical form of `rule` used for hashing.
pub fn canonical_rule(rule: &Rule) -> Rule {
    let mut r = rule.clone();

    // Positional / diagnostic fields: pure noise for our purposes.
    r.location = None;
    r.rule_class_key = None;
    r.instantiation_stack.clear();
    r.definition_stack.clear();
    r.default_setting.clear();
    r.configured_rule_input.clear();
    r.deprecated_public_by_default = None;
    r.deprecated_is_skylark = None;

    // Keep only attributes the BUILD file actually set. `explicitly_specified`
    // is absent when Bazel was asked for `--proto:default_values=false`, in
    // which case every attribute present was specified.
    r.attribute
        .retain(|a| a.explicitly_specified.unwrap_or(true));

    // Positional, but carried as an attribute rather than in `location`.
    r.attribute
        .retain(|a| !LOCATION_ATTRS.contains(&a.name.as_str()));

    for a in &mut r.attribute {
        a.explicitly_specified = None;
        a.nodep = None;
        if ORDER_INSENSITIVE.contains(&a.name.as_str()) {
            a.string_list_value.sort();
            a.string_list_value.dedup();
        }
    }
    r.attribute.sort_by(|a, b| a.name.cmp(&b.name));
    r
}

/// blake3 of the canonicalised rule proto.
///
/// `rule_input` and `rule_output` are part of the encoding, so an edge change
/// moves the hash without any extra bookkeeping.
pub fn rule_hash(rule: &Rule) -> [u8; 32] {
    let c = canonical_rule(rule);
    let mut buf = Vec::with_capacity(c.encoded_len());
    c.encode(&mut buf).expect("Vec never fails to grow");
    *blake3::hash(&buf).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blaze_query::{attribute::Discriminator, Attribute};

    fn attr(name: &str, values: &[&str]) -> Attribute {
        Attribute {
            name: name.to_string(),
            r#type: Discriminator::StringList as i32,
            string_list_value: values.iter().map(|s| s.to_string()).collect(),
            explicitly_specified: Some(true),
            ..Default::default()
        }
    }

    fn base() -> Rule {
        Rule {
            name: "//p:t".into(),
            rule_class: "cc_library".into(),
            rule_input: vec!["//p:a.cc".into(), "//p:dep".into()],
            attribute: vec![attr("srcs", &["a.cc"]), attr("tags", &["b", "a"])],
            ..Default::default()
        }
    }

    #[test]
    fn location_changes_do_not_move_the_hash() {
        let a = base();
        let mut b = base();
        b.location = Some("/abs/path/BUILD:12:1".into());
        b.instantiation_stack = vec!["macro.bzl:3".into()];
        b.definition_stack = vec!["rules.bzl:9".into()];
        b.rule_class_key = Some("key".into());
        assert_eq!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn a_line_shift_in_the_generator_location_does_not_move_the_hash() {
        // The BUILD line a macro was called on is not a property of what the
        // rule builds; a line inserted above it must not mark it changed.
        let mut a = base();
        a.attribute
            .push(attr("generator_location", &["pkg/BUILD:12:1"]));
        a.attribute.push(attr("generator_name", &["t"]));
        let mut b = base();
        b.attribute
            .push(attr("generator_location", &["pkg/BUILD:41:1"]));
        b.attribute.push(attr("generator_name", &["t"]));
        assert_eq!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn the_generating_macro_still_moves_the_hash() {
        let mut a = base();
        a.attribute
            .push(attr("generator_function", &["cc_wrapper"]));
        let mut b = base();
        b.attribute
            .push(attr("generator_function", &["cc_wrapper_v2"]));
        assert_ne!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn tag_reordering_does_not_move_the_hash() {
        let a = base();
        let mut b = base();
        b.attribute[1] = attr("tags", &["a", "b"]);
        assert_eq!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn attribute_ordering_does_not_move_the_hash() {
        let a = base();
        let mut b = base();
        b.attribute.reverse();
        assert_eq!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn src_order_does_move_the_hash() {
        // deps/srcs order is semantically meaningful; only tags-like lists are
        // sorted.
        let a = base();
        let mut b = base();
        b.attribute[0] = attr("srcs", &["a.cc", "b.cc"]);
        assert_ne!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn implicit_attributes_are_ignored() {
        let a = base();
        let mut b = base();
        let mut implicit = attr("$implicit_tool", &["//tools:t"]);
        implicit.explicitly_specified = Some(false);
        b.attribute.push(implicit);
        assert_eq!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn input_changes_move_the_hash() {
        let a = base();
        let mut b = base();
        b.rule_input.push("//p:extra".into());
        assert_ne!(rule_hash(&a), rule_hash(&b));
    }

    #[test]
    fn rule_class_changes_move_the_hash() {
        let a = base();
        let mut b = base();
        b.rule_class = "cc_binary".into();
        assert_ne!(rule_hash(&a), rule_hash(&b));
    }
}
