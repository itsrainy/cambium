//! [`BazelFacts`]: the [`Facts`] implementation that asks Bazel and the head
//! index.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use cambium_bazel::{query_word, Bazel};
use cambium_index::{IndexRef, WalkOptions};
use cambium_proto::blaze_query::{attribute::Discriminator, Rule};

use super::Facts;
use crate::oracles::rdeps::{is_external, label_set};

/// A memo of rule protos by label; `None` for a label Bazel did not return.
type RuleCache = RefCell<HashMap<String, Option<Rule>>>;

/// A [`Facts`] implementation backed by Bazel and the head index.
///
/// It caches every rule proto it fetches: a change set's symmetric difference asks
/// about the same handful of packages repeatedly, and each miss is a Bazel
/// query.
///
/// **On `cambium_extra_implicit`.** The index records no edge kind, so there is
/// no "implicit" edge to test directly.
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
    /// cambium's depth-0 set for this change set.
    direct: BTreeSet<String>,
    /// cambium's whole affected set for this change set (depth 0 included).
    affected: BTreeSet<String>,
    /// Workspace-relative paths `git diff --name-only base..head` reported.
    changed_paths: BTreeSet<String>,
    /// Affected labels whose `--explain` reason is `DeletedDep`.
    deleted_dep: BTreeSet<String>,
    /// Affected labels whose `--explain` reason is `ExternalRepo`.
    external_repo: BTreeSet<String>,
    /// Affected labels whose `--explain` reason is a changed `BUILD` file.
    build_file: BTreeSet<String>,
    /// The reverse-dependency oracle's answer, when the run computed one.
    /// `None` disables `tool_missing`, leaving those labels `unclassified`,
    /// and `tool_extra_source_directory`, leaving those `cambium_missing`.
    /// `Some` and empty is an oracle that ran and reached nothing, which is
    /// an answer.
    oracle: Option<BTreeSet<String>>,
    /// For an affected label, the shortest path back to the direct-set member
    /// that reached it (`--explain`). Lets the `select` test look along the
    /// whole path instead of one hop.
    paths: HashMap<String, Vec<String>>,
    cache: RuleCache,
    base_cache: RuleCache,
    /// Rule protos fetched for [`Facts::has_external_input`]'s closure sweep,
    /// kept apart from `cache` because that one is populated by `prefetch`
    /// from the symmetric difference and this one from whole dependency
    /// closures.
    implicit_cache: RuleCache,
    /// One forward walk per label, memoised: `bazeldiff_extra_external_content`
    /// asks two questions of the same traversal.
    dep_cache: RefCell<HashMap<String, DepFacts>>,
    /// Memo for [`Facts::consumes_source_directory_under_change`], whose
    /// fallback is a batched rule query over the closure.
    source_dir_cache: RefCell<HashMap<String, bool>>,
}

/// What one forward walk over the head index found about a target.
#[derive(Clone, Copy, Debug, Default)]
struct DepFacts {
    /// Some transitive `//...` dependency is in cambium's affected set.
    touches_affected: bool,
    /// Some transitive dependency is an external (`@`-prefixed) label.
    has_external: bool,
    /// The head index knows the label at all. When it does not, nothing here
    /// was actually established and the caller must not conclude anything.
    known: bool,
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
            affected: BTreeSet::new(),
            changed_paths: changed_paths.into_iter().collect(),
            deleted_dep: BTreeSet::new(),
            external_repo: BTreeSet::new(),
            build_file: BTreeSet::new(),
            oracle: None,
            paths: HashMap::new(),
            cache: RefCell::new(HashMap::new()),
            base_cache: RefCell::new(HashMap::new()),
            implicit_cache: RefCell::new(HashMap::new()),
            dep_cache: RefCell::new(HashMap::new()),
            source_dir_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Records cambium's affected set, enabling
    /// `bazeldiff_extra_external_content`. Without it that class's third
    /// condition can never be shown to hold and the label stays
    /// `cambium_missing`, which is the safe direction.
    pub fn with_affected(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.affected = labels.into_iter().collect();
        self
    }

    /// Records the targets cambium reached because a dependency was deleted,
    /// enabling `cambium_extra_deleted`. Feed it from the report's
    /// `explanations` (the comparison asks for them).
    pub fn with_deleted_dep(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.deleted_dep = labels.into_iter().collect();
        self
    }

    /// Records the targets cambium reached because an external repository
    /// moved, enabling `cambium_extra_external_repo`. Feed it from the
    /// report's `explanations`.
    pub fn with_external_repo(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.external_repo = labels.into_iter().collect();
        self
    }

    /// Records the targets cambium reached from a changed `BUILD` file,
    /// enabling `cambium_extra_build_glob`.
    pub fn with_build_file(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.build_file = labels.into_iter().collect();
        self
    }

    /// Records the reverse-dependency oracle's answer, enabling
    /// `tool_missing` and `tool_extra_source_directory`. Call it only when
    /// the oracle ran: an empty answer is "Bazel reached nothing", which is
    /// evidence, where not calling it is "nobody asked", which is not.
    /// Without it an extra nothing else explains stays `unclassified` and a
    /// tool-side extra stays `cambium_missing`, the safe directions.
    pub fn with_oracle(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.oracle = Some(labels.into_iter().collect());
        self
    }

    /// Records `--explain` paths, so `cambium_extra_select` can look along the
    /// whole path from the change to the target rather than at its last hop.
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        self.paths = paths.into_iter().collect();
        self
    }

    /// Adds the base-side Bazel, enabling `cambium_extra_visibility`.
    pub fn with_base(mut self, base: Bazel) -> Self {
        self.base = Some(base);
        self
    }

    fn rule(&self, bazel: &Bazel, cache: &RuleCache, label: &str) -> Option<Rule> {
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

    /// Everything the head index can say about a target's dependency cone.
    ///
    /// One breadth-first walk over the forward edges answers both "does it
    /// reach anything cambium called affected" and "does it reach anything
    /// external". External labels *are* in the index: `rule_input` is interned
    /// unfiltered, so an `@repo//...` dependency of an indexed rule exists as
    /// an [`cambium_index::NodeKind::Unknown`] node with no package and no
    /// rule class — enough to see it and to walk past it, which is all this
    /// needs. (Its own dependencies are not there: cambium queries `//...`, so
    /// the external graph stops at its first node.)
    fn dep_facts(&self, label: &str) -> DepFacts {
        if let Some(hit) = self.dep_cache.borrow().get(label) {
            return *hit;
        }
        let facts = self.compute_dep_facts(label);
        self.dep_cache.borrow_mut().insert(label.to_string(), facts);
        facts
    }

    fn compute_dep_facts(&self, label: &str) -> DepFacts {
        let Some(id) = self.index.id_of(label) else {
            return DepFacts::default();
        };
        let mut out = DepFacts {
            known: true,
            ..Default::default()
        };
        let walk = cambium_index::walk_deps(self.index, &[id], &WalkOptions::default());
        for v in walk.visited.ones() {
            let v = v as u32;
            if v == id {
                continue;
            }
            let l = self.index.label(v);
            if is_external(l) {
                out.has_external = true;
            } else if self.affected.contains(l) {
                out.touches_affected = true;
            }
        }
        // A dependency named in the rule but absent from the index still
        // counts: the index is `//...`, so an external label the walk cannot
        // reach is exactly the case this looks for. The toolchain graph —
        // `@bazel_tools//tools/jdk:jdk`, `@local_jdk//:jdk`, the C++
        // toolchain — is exactly what a tool's two runs disagree about
        // most often, and it is reached through implicit attributes; the index
        // now carries those (`cambium_bazel::QUERY_FLAGS`), so the walk above
        // usually settles it and this is the fallback for a label the index
        // does not know.
        if !out.has_external {
            if let Some(rule) = self.head_rule(label) {
                out.has_external = names_external_input(&rule);
            }
        }
        // And when the label's own inputs say nothing either, ask the same
        // question of every rule in the closure the walk just enumerated.
        // `//:srcs` reaches `@async_profiler//file:file` only several
        // filegroups down, through an edge on a target inside its closure;
        // bazel-diff reported it on a change set whose only changed files were
        // `.github/workflows/*.yml`, which no rule consumes at all.
        if !out.has_external {
            let mut closure: Vec<String> = walk
                .visited
                .ones()
                .map(|v| v as u32)
                // Only rules have a `rule_input`; the file nodes in a
                // filegroup's closure would be a Bazel query each for nothing.
                .filter(|&v| self.index.is_rule(v))
                .map(|v| self.index.label(v).to_string())
                .filter(|l| !is_external(l))
                .collect();
            closure.sort();
            closure.dedup();
            out.has_external = self.any_implicit_external(&closure);
        }
        out
    }

    /// Whether the label's dependency cone names a directory a changed path
    /// lies under.
    ///
    /// Two sources, cheapest first. The head index carries every source-file
    /// target the `deps(//...)` query returned, and `//pkg:.` is one of them,
    /// so the forward walk usually settles it without a Bazel. When the index
    /// does not know the label, or the walk found nothing, the rule protos of
    /// the label and every rule in its closure are asked the same question of
    /// their `rule_input` — batched and memoised the way
    /// [`Self::any_implicit_external`] is.
    fn compute_source_directory(&self, label: &str) -> bool {
        let under = |l: &str| directory_contains_changed_path(l, &self.changed_paths);
        let mut closure: Vec<String> = vec![label.to_string()];
        if let Some(id) = self.index.id_of(label) {
            let walk = cambium_index::walk_deps(self.index, &[id], &WalkOptions::default());
            for v in walk.visited.ones() {
                let v = v as u32;
                let l = self.index.label(v);
                if self.index.is_rule(v) {
                    if v != id {
                        closure.push(l.to_string());
                    }
                } else if under(l) {
                    return true;
                }
            }
        }
        closure.retain(|l| !is_external(l));
        closure.sort();
        closure.dedup();
        self.any_rule_input(&closure, |input| under(input))
    }

    /// Does any rule in `labels` name an input `pred` accepts?
    ///
    /// Batched and memoised: the closures overlap heavily between the handful
    /// of labels one change set asks about, and one `query 'set(...)'` answers five
    /// hundred of them.
    fn any_rule_input(&self, labels: &[String], pred: impl Fn(&str) -> bool) -> bool {
        for chunk in labels.chunks(PREFETCH_CHUNK) {
            fill_cache(self.head, &self.implicit_cache, chunk);
            let c = self.implicit_cache.borrow();
            if chunk
                .iter()
                .filter_map(|l| c.get(l.as_str()))
                .flatten()
                .any(|r| r.rule_input.iter().any(|i| pred(i)))
            {
                return true;
            }
        }
        false
    }

    /// Does any rule in `labels` name an external label among its inputs?
    fn any_implicit_external(&self, labels: &[String]) -> bool {
        self.any_rule_input(labels, is_external)
    }

    /// Fetches the rule protos for `labels` in bulk, at both revisions.
    ///
    /// Without this the classifier is one `bazel query <label>` per label, and
    /// the first real run on `bazel/bazel` produced symmetric differences of
    /// tens of thousands of labels: at roughly a second per invocation that is
    /// hours per change set, and it is *all* process startup — one batched
    /// `query 'set(a b c ...)'` answers five hundred labels in the same time.
    /// Labels the batch does not return are cached as `None`, so a target
    /// Bazel will not talk about is not retried one at a time either.
    ///
    /// Best-effort: a batch that fails leaves its labels uncached, and they
    /// fall back to the per-label path.
    pub fn prefetch(&self, labels: impl IntoIterator<Item = String>) {
        let wanted: Vec<String> = labels.into_iter().filter(|l| !is_external(l)).collect();
        if wanted.is_empty() {
            return;
        }
        prefetch_into(self.head, &self.cache, &wanted);
        if let Some(base) = self.base.as_ref() {
            prefetch_into(base, &self.base_cache, &wanted);
        }
    }
}

/// How many labels go into one batched `bazel query 'set(...)'`. Large enough
/// that startup stops dominating, small enough that the command line and the
/// decoded proto stay manageable.
pub const PREFETCH_CHUNK: usize = 500;

fn prefetch_into(bazel: &Bazel, cache: &RuleCache, labels: &[String]) {
    for chunk in labels.chunks(PREFETCH_CHUNK) {
        fill_cache(bazel, cache, chunk);
    }
}

/// Asks Bazel, in one `set(...)` query, for every label in `chunk` the cache
/// has not seen, and caches the answer: the rules it returned, and `None` for
/// the rest. A query that fails caches nothing, so those labels are asked
/// again the next time (or one at a time, by [`BazelFacts::rule`]).
fn fill_cache(bazel: &Bazel, cache: &RuleCache, chunk: &[String]) {
    let todo: Vec<&String> = {
        let c = cache.borrow();
        chunk
            .iter()
            .filter(|l| !c.contains_key(l.as_str()))
            .collect()
    };
    if todo.is_empty() {
        return;
    }
    let Some(rules) = crate::tools::bazel_query::rules(bazel, &label_set(&todo)) else {
        return;
    };
    let mut c = cache.borrow_mut();
    for r in rules {
        c.insert(r.name.clone(), Some(r));
    }
    for l in todo {
        c.entry(l.clone()).or_insert(None);
    }
}

/// Whether a rule names a label outside the main repository among its
/// inputs. Split out from the walk so the predicate can be tested on a
/// hand-built proto without a Bazel.
pub fn names_external_input(rule: &Rule) -> bool {
    rule.rule_input.iter().any(|l| is_external(l))
}

/// One rule's proto at a revision, or `None` when Bazel cannot produce it.
/// [`crate::tools::bazel_query::rules`] carries the flags and the exit-3
/// policy; this only asks for one label.
fn query_rule(bazel: &Bazel, label: &str) -> Option<Rule> {
    crate::tools::bazel_query::rules(bazel, query_word(label).as_str())?
        .into_iter()
        .next()
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

/// A rule with every occurrence of `from` in its string-valued attributes
/// replaced by `to`.
///
/// Only the fields that can carry a workspace path matter: `string_value`
/// (a `genrule`'s `cmd`), `string_list_value` (an `args` list), the values of
/// a `string_dict` (a test's `env`), and the same three inside every branch of
/// a `selector_list`. Anything the substitution does not reach simply keeps
/// its bytes, and the comparison this feeds then reports "not equal", which is
/// the safe direction: the label stays `cambium_missing`.
pub fn substitute_paths(rule: &Rule, from: &str, to: &str) -> Rule {
    fn fix(v: &mut String, from: &str, to: &str) {
        if v.contains(from) {
            *v = v.replace(from, to);
        }
    }
    let mut r = rule.clone();
    for a in &mut r.attribute {
        if let Some(v) = a.string_value.as_mut() {
            fix(v, from, to);
        }
        for v in &mut a.string_list_value {
            fix(v, from, to);
        }
        for e in &mut a.string_dict_value {
            fix(&mut e.value, from, to);
        }
        if let Some(sel) = a.selector_list.as_mut() {
            for s in &mut sel.elements {
                for e in &mut s.entries {
                    if let Some(v) = e.string_value.as_mut() {
                        fix(v, from, to);
                    }
                    for v in &mut e.string_list_value {
                        fix(v, from, to);
                    }
                    for d in &mut e.string_dict_value {
                        fix(&mut d.value, from, to);
                    }
                }
            }
        }
    }
    r
}

/// Whether `label` names a directory some path in `changed` lies strictly
/// under.
///
/// `//pkg:.` is the package directory itself (`pkg`; the root package's
/// `//:.` is the whole tree), `//pkg:sub` a subdirectory. Strictly under is
/// the point: a changed path *equal* to the label's is a plain file edit
/// that cambium's walk reports by itself, and a real file can have nothing
/// beneath it, so `starts_with(dir + "/")` is exactly "the label is a
/// directory and something in it moved" with no need to stat anything.
pub fn directory_contains_changed_path(label: &str, changed: &BTreeSet<String>) -> bool {
    let Some(path) = label_to_path(label) else {
        return false;
    };
    let dir = path.strip_suffix("/.").unwrap_or(&path);
    if dir == "." {
        return !changed.is_empty();
    }
    let prefix = format!("{dir}/");
    changed
        .range(prefix.clone()..)
        .next()
        .is_some_and(|p| p.starts_with(&prefix))
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
        is_external(label)
    }

    fn reachable_via_select(&self, label: &str) -> bool {
        // Any rule on the path from the change to this target declares a
        // `select`ed dependency attribute, so a configured build need not take
        // this route at all.
        //
        // Looking only at the target's own rule (which is all this could do
        // before the comparison carried `--explain` paths) misses the common
        // shape by a mile: on `bazel/bazel` the extra targets are reached
        // through a `select` several hops down — a `cc_library` picking a
        // platform-specific dep, with plain `java_library` rules stacked above
        // it — and every one of those landed in `unclassified`, which fails
        // the run for a difference that is exactly what cambium promises to
        // over-approximate.
        let path = self.paths.get(label);
        let candidates: Vec<&str> = match path {
            Some(p) => p.iter().map(String::as_str).collect(),
            None => vec![label],
        };
        candidates.iter().any(|l| match self.head_rule(l) {
            Some(rule) => explicit_dep_labels(&rule).1,
            None => false,
        })
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

    fn reached_via_deleted_dep(&self, label: &str) -> bool {
        self.deleted_dep.contains(label)
    }

    fn reached_via_external_repo(&self, label: &str) -> bool {
        self.external_repo.contains(label)
    }

    fn reached_via_build_file(&self, label: &str) -> bool {
        self.build_file.contains(label)
    }

    fn rule_unchanged_modulo_visibility(&self, label: &str) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            // Without both protos nothing is established. Saying "unchanged"
            // here would let a target Bazel refuses to talk about into a
            // benign class.
            return false;
        };
        use prost::Message as _;
        rule_without_visibility(&h).encode_to_vec() == rule_without_visibility(&b).encode_to_vec()
    }

    fn no_changed_source_inputs(&self, label: &str) -> bool {
        let Some(rule) = self.head_rule(label) else {
            return false;
        };
        !rule
            .rule_input
            .iter()
            .filter_map(|l| label_to_path(l))
            .any(|p| self.changed_paths.contains(&p))
    }

    fn no_affected_deps(&self, label: &str) -> bool {
        let facts = self.dep_facts(label);
        facts.known && !facts.touches_affected
    }

    fn has_external_input(&self, label: &str) -> bool {
        self.dep_facts(label).has_external
    }

    fn confirmed_by_oracle(&self, label: &str) -> bool {
        self.oracle.as_ref().is_some_and(|o| o.contains(label))
    }

    fn oracle_ran(&self) -> bool {
        self.oracle.is_some()
    }

    fn consumes_source_directory_under_change(&self, label: &str) -> bool {
        if let Some(hit) = self.source_dir_cache.borrow().get(label) {
            return *hit;
        }
        let found = self.compute_source_directory(label);
        self.source_dir_cache
            .borrow_mut()
            .insert(label.to_string(), found);
        found
    }

    fn workspace_path_only_change(&self, label: &str) -> bool {
        let Some(base) = self.base.as_ref() else {
            return false;
        };
        // Both spellings of each root. Bazel prints the path it resolved the
        // workspace to, which on macOS is `/private/var/...` where the
        // worktree was created under `/var/...`; substituting the unresolved
        // spelling would then find nothing and the label would stay
        // `cambium_missing` for a reason that is pure path formatting.
        let roots = |b: &Bazel| {
            let p = b.workspace().to_path_buf();
            let mut v = vec![p.to_string_lossy().into_owned()];
            if let Ok(c) = p.canonicalize() {
                let c = c.to_string_lossy().into_owned();
                if !v.contains(&c) {
                    v.push(c);
                }
            }
            v
        };
        let (base_roots, head_roots) = (roots(base), roots(self.head));
        // Same directory on both sides: there is no artefact to explain, and
        // the substitution would be the identity.
        if base_roots == head_roots {
            return false;
        }
        let (Some(h), Some(b)) = (
            self.head_rule(label),
            self.rule(base, &self.base_cache, label),
        ) else {
            return false;
        };
        use prost::Message as _;
        let (h, b) = (rule_without_visibility(&h), rule_without_visibility(&b));
        let (hb, bb) = (h.encode_to_vec(), b.encode_to_vec());
        // Equal already: this is not what happened, and the external-content
        // class is the one that should speak for it.
        if hb == bb {
            return false;
        }
        base_roots.iter().any(|from| {
            head_roots
                .iter()
                .any(|to| substitute_paths(&b, from, to).encode_to_vec() == hb)
        })
    }

    fn build_file_changed(&self, label: &str) -> bool {
        // `//pkg:name` -> `pkg/BUILD`, either spelling. The root package's
        // BUILD file has no directory prefix.
        let Some(rest) = label.strip_prefix("//") else {
            return false;
        };
        let pkg = rest.split_once(':').map(|(p, _)| p).unwrap_or(rest);
        ["BUILD.bazel", "BUILD"].iter().any(|name| {
            let path = if pkg.is_empty() {
                (*name).to_string()
            } else {
                format!("{pkg}/{name}")
            };
            self.changed_paths.contains(&path)
        })
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

    /// The fourth condition of `bazeldiff_extra_external_content` reads the
    /// rule's inputs; a toolchain edge only shows up there when Bazel is
    /// asked *with* implicit deps, which every benchmark query is (no
    /// `--noimplicit_deps`; see [`crate::tools::bazel_query`]).
    #[test]
    fn a_toolchain_input_counts_as_external() {
        let mut rule = Rule {
            name: "//examples:hello-world".into(),
            rule_input: vec!["//examples:Greeter.java".into()],
            ..Default::default()
        };
        assert!(
            !names_external_input(&rule),
            "a proto naming only main-repo inputs names nothing external"
        );
        rule.rule_input.push("@bazel_tools//tools/jdk:jdk".into());
        assert!(names_external_input(&rule));
    }

    #[test]
    fn the_legacy_external_spelling_counts_too() {
        let rule = Rule {
            name: "//a:b".into(),
            rule_input: vec!["//external:jdk".into()],
            ..Default::default()
        };
        assert!(names_external_input(&rule));
    }

    /// The substitution reaches an `args` list, a `cmd` string and a `select`
    /// branch — the three shapes envoy's `@envoy_repo//:path.bzl` constant
    /// lands in.
    #[test]
    fn substitution_normalises_every_string_shape() {
        use cambium_proto::blaze_query::attribute::{
            Discriminator as D, Selector, SelectorEntry, SelectorList,
        };
        use cambium_proto::blaze_query::Attribute;
        let rule = Rule {
            name: "//tools/sha:replace".into(),
            attribute: vec![
                Attribute {
                    name: "args".into(),
                    r#type: D::StringList as i32,
                    string_list_value: vec!["/base/wt".into()],
                    ..Default::default()
                },
                Attribute {
                    name: "cmd".into(),
                    r#type: D::String as i32,
                    string_value: Some("git -C /base/wt ls-files".into()),
                    ..Default::default()
                },
                Attribute {
                    name: "copts".into(),
                    r#type: D::SelectorList as i32,
                    selector_list: Some(SelectorList {
                        r#type: Some(D::StringList as i32),
                        elements: vec![Selector {
                            entries: vec![SelectorEntry {
                                label: Some("//conditions:default".into()),
                                string_list_value: vec!["-I/base/wt/include".into()],
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let got = substitute_paths(&rule, "/base/wt", "/head/wt");
        assert_eq!(got.attribute[0].string_list_value, vec!["/head/wt"]);
        assert_eq!(
            got.attribute[1].string_value.as_deref(),
            Some("git -C /head/wt ls-files")
        );
        assert_eq!(
            got.attribute[2].selector_list.as_ref().unwrap().elements[0].entries[0]
                .string_list_value,
            vec!["-I/head/wt/include"]
        );
        // A rule with nothing to substitute comes back untouched.
        use prost::Message as _;
        assert_eq!(
            substitute_paths(&rule, "/elsewhere", "/x").encode_to_vec(),
            rule.encode_to_vec()
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

    #[test]
    fn a_directory_label_contains_paths_strictly_under_it() {
        let changed: BTreeSet<String> = [
            "modules/perception/onboard/component/lidar_tracking_component.cc",
            "cyber/BUILD",
            "top.txt",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // `src_dir = ["."]` as Bazel renders it.
        assert!(directory_contains_changed_path(
            "//modules/perception:.",
            &changed
        ));
        // A subdirectory named as a source (`srcs = ["testdata"]`).
        assert!(directory_contains_changed_path(
            "//modules/perception:onboard",
            &changed
        ));
        assert!(directory_contains_changed_path("//cyber:.", &changed));
        // The root package's own directory is the whole tree.
        assert!(directory_contains_changed_path("//:.", &changed));
        assert!(!directory_contains_changed_path("//:.", &BTreeSet::new()));
        // Not under it.
        assert!(!directory_contains_changed_path(
            "//modules/planning:.",
            &changed
        ));
        // A sibling whose name is a prefix of the directory's is not inside it.
        assert!(!directory_contains_changed_path(
            "//modules/percept:.",
            &changed
        ));
        // Equal is a file edit, not a directory: the walk sees those itself.
        assert!(!directory_contains_changed_path("//cyber:BUILD", &changed));
        assert!(!directory_contains_changed_path("//:top.txt", &changed));
        // External labels never.
        assert!(!directory_contains_changed_path(
            "@ext//modules:.",
            &changed
        ));
    }
}
