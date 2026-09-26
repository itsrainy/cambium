//! Round trips: a dependency path that leaves the index's universe through
//! a leaf and comes back into it.
//!
//! The index holds the target graph of `//...` and of the in-tree
//! repositories whose packages it indexes (its universe). Every other label a
//! rule names is a leaf: a node no package owns, with reverse edges from the
//! rules that name it and, until this module, no outgoing edges. A path
//! `//tests:env_test -> @bazel_tools//tools/test:collect_coverage ->
//! //shell/runfiles:runfiles` was cut at the leaf, so an edit to
//! `shell/runfiles/runfiles.bash` reached only the two tests that name the
//! library themselves. That is `bazelbuild/rules_shell` at `b1eed28`, which
//! is the root module: `@bazel_tools` names `@rules_shell//shell/runfiles`,
//! and for the root module that is the main repository. Bazel's `rdeps`
//! names 24 tests.
//!
//! # The idea
//!
//! A leaf is given its outgoing edges into the universe, and nothing else
//! changes: the reverse walk from a changed target inside reaches the leaf
//! through them, and from the leaf the rules that name it, with no special
//! case anywhere in the walk. The edges are Bazel's answer
//! ([`cambium_bazel::Bazel::round_trips`]): for each leaf, the first node
//! inside the universe on every path from the leaf that runs outside it until
//! then. The intermediate external nodes are not stored; the leaf gets a
//! direct edge to each such entry.
//!
//! Stopping at the first inside node is what keeps the answer cheap to hold.
//! What lies beyond an entry is the index's own rows, which a splice
//! re-queries with their packages like any others, so a `BUILD` edit in the
//! main repository never makes a leaf's edges stale: the path `leaf -> entry
//! -> (main-repo edges) -> changed target` is walked through rows that are
//! already current. A path that leaves the universe again goes out through
//! another label a main-repo rule names, which is a leaf with edges of its
//! own.
//!
//! What stays is the part of the path outside the universe, and that is not
//! a function of the repositories outside alone. The packages there load
//! `.bzl` files, and some of those can be the main repository's: on
//! rules_shell, `collect_coverage` is an `sh_binary`, whose rule class is the
//! root module's own `shell/private/sh_binary.bzl`, and the edge to the
//! runfiles library is that class's implicit `_runfiles_dep`. An edit there
//! can give the leaf a new entry, or take one away, with no `BUILD` file, no
//! package of the index and no repository outside moving. So an answer is
//! stored with what it was derived through (`IndexRef::round_trip_via`, in
//! the spelling `crate::via` reads, shared with the consumer table's leaf
//! map):
//!
//! - `//pkg:file`, every main-repo file the packages outside the universe on
//!   the leaves' closure load ([`cambium_bazel::Bazel::closure_loads`]);
//! - `@name`, every indexed repository one of those packages loads a file
//!   of;
//! - `//pkg` (or `@name`), the package (or indexed repository) of an entry
//!   the index had no node for when the answer was written, so that the
//!   entry becomes an edge once it is a node.
//!
//! The answer can move when one of those does, when a repository outside
//! the universe moves, or when a leaf appears, and at no other time.

//! # When it is asked
//!
//! - At cold build, for every leaf, after everything else the build asks of
//!   Bazel.
//! - At head, in the dirty stage of a run in which a repository outside the
//!   universe moved, or an entry of the via set did ([`at_head`]): the
//!   leaves may now reach the universe where they did not, and a change in
//!   the same commit to what they reach would otherwise miss the rules above
//!   the leaf. This run's rows are Bazel's answer at head, like every row a
//!   splice writes, and so is the via set the index it writes carries.
//! - In a splice that writes an index ([`refresh`]): for every leaf after an
//!   overflow's re-query of `//...`, and for a leaf this splice added
//!   otherwise. A leaf added by a splice does not need its edges for that
//!   run's own answer: the rule that names it changed its inputs, so its
//!   hash moved and it is direct already.
//!
//! A question Bazel will not answer leaves every leaf with the row it had.
//! A leaf the answer leaves out (it reaches nothing inside, or Bazel no
//! longer knows it) gets an empty row, so a leaf that stops existing, or
//! stops reaching the universe, has its edges retired rather than carried.
//!
//! # The repositories a leaf's closure runs through
//!
//! The same walk says which repositories outside the universe each leaf's
//! closure passes through before it comes back in
//! ([`cambium_bazel::RoundTrips::closure`], stored per repository as
//! `IndexRef::leaf_closure`, format 16). That is the other half of what a
//! leaf means: rule (1) of `crate::externals` seeds the labels of a
//! repository that moved, and a rule that reaches the repository only
//! through another one is above none of them. envoy's rules name
//! `@grpc//:grpc++`, grpc links `@c-ares`, and an edit to
//! `bazel/c-ares.patch` (box run F, step 161) moved `@c-ares` alone: the
//! rules naming `@c-ares//:ares` were seeded, and 145 of the oracle's
//! labels, rules that reach `@c-ares` only through `grpc++`, were missing. A run now
//! seeds every leaf whose closure runs through a repository that moved
//! ([`closure_seeds`]), which is what Bazel rebuilds. The sets are Bazel's
//! answer wherever the edges are (cold build, overflow, at head, a leaf a
//! splice added), are carried by every splice beside them, and a run that
//! asked at head seeds by the union of the base's and the head's.
//!
//! The consumer table's `Consumer::via` records the same repositories, per
//! consumer and pooled over its leaves, and only for the consumers of an
//! indexed repository; it answers whether that leaf map is still fresh, not
//! what to seed, so it stays where it is. The spelling is shared: a closure
//! entry is `@name` as Bazel printed the name, read by `crate::via::Moved`
//! like every other via entry.
//!
//! # Cost
//!
//! One `deps` over every leaf, `--output=graph`, which answers the edges and
//! the closures both; and one `loadfiles` over the same closure less the
//! universe's packages, `--output=label`. The graph replaced an
//! `--output=package` listing and an `allpaths` graph asked only when the
//! closure reached back in. On envoy at `eea2ba5e2911` (728 leaves, a
//! WORKSPACE-mode output base the somepath queries had warmed, the laptop
//! loaded) the graph is 95828 nodes and 23.9 MB, 14 s the first time and
//! 8 s again. Before the graph, at the pins, on
//! the output bases the cold builds warmed, with the laptop's load average
//! near 100: envoy, 752 leaves and a closure of 1399 packages, 13.1 s of a
//! 482 s cold build, 2 leaves with edges and nothing loaded from the main
//! repository; xla, 636 leaves and 488 packages, 6.3 s of 174 s, 34 leaves
//! with 74 edges and 14 main-repo `.bzl` files loaded outside (`tsl.bzl`,
//! `py_strict.bzl`, `third_party/rules_python`'s); rules_shell, 28 leaves,
//! 1.3 s, four `.bzl` files. Timed alone on a warm xla server the `deps` is
//! 0.9 s and the `loadfiles` 0.8 s. On an output base that has fetched
//! nothing (the repository cache warm) the `deps` alone took 952 s on xla:
//! it fetches and loads every repository the leaves reach. A cold build's
//! universe query has fetched most of those by then, which that number does
//! not subtract.

//! # Where the edges live
//!
//! In the forward CSR, as the leaf node's own row. No package owns a leaf, so
//! `apply_packages` and `delete_package`, which replace and retire the rows of
//! the packages they are given, never touch it, and every splice carries it
//! verbatim ([`cambium_index::IndexBuilder::set_leaf_row`]). The via set is
//! a column of its own, carried by every splice until an answer replaces it
//! (index format 15, which retires every index written without the edges).
//!
//! # The consumer table, and why both exist
//!
//! The consumer table (`crate::consumers`, `RepoInputs::consumers`) answers
//! the other direction of the same shape for an indexed in-tree repository
//! `R`: which external repositories consume `R`, and which of their leaves
//! reach which of `R`'s packages. It exists because `R` moves as a
//! repository, not only target by target: an input-path edit, a `.bzl` of
//! `R`, or its definition moving can change what `R`'s packages mean with no
//! node to walk from, and the table is what turns "`R` moved" into the
//! consumer leaves to seed. The main repository never moves that way. Every
//! change to it is a change to its packages, which the splice turns into
//! nodes, so a leaf's edges are all a change inside needs to find the rules
//! above the leaf. The edges here reach into `R` too, since `R`'s nodes are
//! inside the universe, and for a change that is a node of `R` they are the
//! exact answer; the table stays for the repository-level moves, and a leaf
//! seeded by both is seeded once.

use std::collections::{BTreeMap, BTreeSet};

use cambium_index::{IndexBuilder, IndexRef, NodeKind};

use crate::externals::Externals;
use crate::query::PackageQuerier;
use crate::via::Moved;

/// One round-trip question and Bazel's answer: the leaves asked about, for
/// each that reaches the universe the inside labels it reaches, what the
/// answer was derived through ([`cambium_bazel::RoundTrips::via`]), and the
/// repositories each leaf's closure runs through
/// ([`cambium_bazel::RoundTrips::closure`]). An asked leaf missing from
/// `reach` reaches nothing inside; one missing from `closure` is one Bazel
/// could not resolve.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub leaves: Vec<String>,
    pub reach: BTreeMap<String, BTreeSet<String>>,
    pub via: BTreeSet<String>,
    pub closure: BTreeMap<String, BTreeSet<String>>,
}

/// Which leaves a splice asks for, beyond any answer the run already has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    /// None: an overlay, which is never written and has its answer already.
    Nothing,
    /// The leaves the splice added, whose ids are at or past this one.
    AddedSince(u32),
    /// Every leaf: `//...` was re-queried, or the index is being built cold.
    All,
}

/// Whether `label` is a leaf by its spelling: an external label. A main-repo
/// label with no node of its own is a target of a package that failed to
/// load, which is not a way out of the universe.
fn external(label: &str) -> bool {
    label.starts_with('@')
}

/// The leaves of an index being built whose ids are at or past `from`:
/// external labels no package owns, less tombstones.
pub(crate) fn leaves(b: &IndexBuilder<'_>, from: u32) -> Vec<String> {
    (from..b.num_nodes())
        .filter(|&id| b.is_leaf(id) && !b.is_tombstone(id) && external(b.label(id)))
        .map(|id| b.label(id).to_string())
        .collect()
}

/// The leaves of a finished index, in label order.
pub(crate) fn base_leaves(base: IndexRef<'_>) -> Vec<String> {
    // External labels sort after every main-repo one (`@` > `/`), so they are
    // one run at the end of `label_sorted`.
    let start = base
        .label_sorted
        .partition_point(|&id| base.label(id) < "@");
    base.label_sorted[start..]
        .iter()
        .copied()
        .filter(|&id| base.kind_of(id) == NodeKind::Unknown && !base.is_tombstone(id))
        .map(|id| base.label(id))
        .filter(|l| external(l))
        .map(str::to_string)
        .collect()
}

/// Asks Bazel about `leaves`, with the index's `indexed` repositories as
/// part of the universe. `None` when Bazel would not answer.
pub(crate) fn ask(
    q: &dyn PackageQuerier,
    indexed: &[String],
    leaves: Vec<String>,
) -> Option<Answer> {
    if leaves.is_empty() {
        return Some(Answer::default());
    }
    let started = std::time::Instant::now();
    let answer = q.round_trips(indexed, &leaves);
    tracing::debug!(
        leaves = leaves.len(),
        answered = answer.is_some(),
        reaching = answer.as_ref().map(|a| a.reach.len()),
        via = answer.as_ref().map(|a| a.via.len()),
        ms = started.elapsed().as_millis() as u64,
        "the round-trip edges were asked for"
    );
    answer.map(|a| Answer {
        leaves,
        reach: a.reach,
        via: a.via,
        closure: a.closure,
    })
}

/// The via entry that stands for an entry the index has no node for: its
/// package (`//pkg`) for a main-repo label, its repository (`@name`) for an
/// indexed repository's. When that moves, the entry may have a node, and
/// the leaf is asked about again.
fn missing_via(label: &str) -> Option<String> {
    if let Some(rest) = label.strip_prefix("//") {
        return Some(format!(
            "//{}",
            rest.split_once(':').map_or(rest, |(p, _)| p)
        ));
    }
    let (repo, _) = label.strip_prefix('@')?.split_once("//")?;
    (!repo.is_empty()).then(|| format!("@{repo}"))
}

/// Writes an answer into the index being built: each asked leaf's row
/// becomes the entries it reaches, empty for one that reaches nothing, and
/// its closure the repositories the answer says it runs through, none for
/// one Bazel could not resolve. Returns how many rows changed, and the via
/// entries that stand for the entries the index has no node for
/// ([`missing_via`]): a target of a package that failed to load, or one the
/// index does not hold yet.
pub(crate) fn apply(b: &mut IndexBuilder<'_>, answer: &Answer) -> (usize, BTreeSet<String>) {
    let mut changed = 0;
    let mut missing = BTreeSet::new();
    for leaf in &answer.leaves {
        let Some(id) = b.label_id(leaf) else {
            continue;
        };
        // A label that has become a package's target is not a leaf any
        // more, and runs through nothing here.
        let closure = answer.closure.get(leaf).filter(|_| b.is_leaf(id));
        b.set_leaf_closure(id, closure.into_iter().flatten().map(String::as_str));
        let mut dsts: Vec<u32> = Vec::new();
        for e in answer.reach.get(leaf).into_iter().flatten() {
            match b.label_id(e) {
                Some(d) => dsts.push(d),
                None => missing.extend(missing_via(e)),
            }
        }
        dsts.sort_unstable();
        dsts.dedup();
        if b.forward_row(id) == dsts {
            continue;
        }
        if b.set_leaf_row(id, &dsts) {
            changed += 1;
        }
    }
    if !missing.is_empty() {
        tracing::debug!(
            ?missing,
            "round-trip entries the index has no node for; their packages are watched"
        );
    }
    (changed, missing)
}

/// The repositories outside the universe that moved in this run: every
/// repository the externals stage dirtied, by any route, less the indexed
/// ones, whose targets are nodes of the index with rows of their own.
pub(crate) fn moved_outside(ext: &Externals, indexed: &[String]) -> BTreeSet<String> {
    ext.repos
        .iter()
        .chain(&ext.load_repos)
        .filter(|r| !indexed.contains(r))
        .cloned()
        .collect()
}

/// The leaves whose closure runs through one of `moved`, the repositories
/// that moved in this run, for `externals::reach` rule (1) to seed beside
/// the moved repositories' own labels.
///
/// Rule (1) seeds a repository's labels, which are the leaves of it that a
/// main-repo rule names. A repository no main-repo rule names, or one named
/// only on some paths, is reached on the others through another
/// repository: envoy's rules name `@grpc//:grpc++`, grpc links `@c-ares`,
/// and an edit to `bazel/c-ares.patch` moves `@c-ares` alone. A few envoy
/// rules name `@c-ares//:ares` and rule (1) seeds them; the rest reach it
/// only through `grpc++`. Bazel refetches it, rebuilds grpc and every rule
/// above `grpc++`; the index holds no edge from the leaf into `@c-ares`, so
/// run F's step 161 missed 145 labels. The table ([`IndexRef::leaf_closure`]) is what says the leaf
/// runs through it. Seeding every leaf a moved repository is on the closure
/// of is exactly what Bazel rebuilds; a repository every leaf runs through
/// seeds every leaf, which is the answer.
///
/// `moved` less the indexed repositories: their nodes are inside, and a
/// change to one is walked through its rows, the leaves' round-trip edges
/// and the consumer table. `head`, when the run asked at head
/// ([`at_head`]), adds the leaves whose closure runs through a moved
/// repository there: the same commit can put the repository on a closure
/// and move it. A leaf the table does not know (never asked about, or one
/// Bazel could not resolve) is seeded by its own labels alone, which is the
/// answer before the table.
pub(crate) fn closure_seeds(
    base: IndexRef<'_>,
    moved: &BTreeSet<String>,
    head: Option<&Answer>,
) -> BTreeSet<u32> {
    let indexed = base.indexed_repos();
    let outside: BTreeSet<String> = moved
        .iter()
        .filter(|r| !indexed.contains(r))
        .cloned()
        .collect();
    let mut out = BTreeSet::new();
    if outside.is_empty() {
        return out;
    }
    let none = BTreeSet::new();
    let through = Moved::new(base, &none, &none, &outside);
    let live = |id: &u32| *id < base.num_nodes() && !base.is_tombstone(*id);
    for (repo, leaves) in base.leaf_closure() {
        if through.hit([repo], None).is_some() {
            out.extend(leaves.iter().copied().filter(live));
        }
    }
    for (leaf, repos) in head.map(|a| &a.closure).into_iter().flatten() {
        if through
            .hit(repos.iter().map(String::as_str), None)
            .is_some()
        {
            out.extend(base.id_of(leaf).filter(live));
        }
    }
    if !out.is_empty() {
        tracing::debug!(
            ?outside,
            leaves = out.len(),
            first = ?out.iter().take(5).map(|&id| base.label(id)).collect::<Vec<_>>(),
            "leaves whose closure runs through a repository that moved are seeded"
        );
    }
    out
}

/// Asks Bazel at head for every leaf's edges when something they were
/// derived through moved: a repository outside the universe, or an entry of
/// the base's via set ([`IndexRef::round_trip_via`], read by `through`).
/// This is the one run in which the base's answer can be wrong. `None` when
/// nothing moved, when the index holds no leaf, or when Bazel would not
/// answer (the base's rows then stand).
pub(crate) fn at_head(
    base: IndexRef<'_>,
    q: &dyn PackageQuerier,
    ext: &Externals,
    through: &Moved<'_>,
) -> Option<Answer> {
    let indexed = base.indexed_repos();
    let moved = moved_outside(ext, &indexed);
    let hit = through.hit(base.round_trip_via(), None);
    if moved.is_empty() && hit.is_none() {
        return None;
    }
    let leaves = base_leaves(base);
    if leaves.is_empty() {
        return None;
    }
    tracing::debug!(
        ?moved,
        through = hit,
        leaves = leaves.len(),
        "something the round-trip edges were derived through moved, so they are asked for at head"
    );
    ask(q, &indexed, leaves)
}

/// The round-trip rows of an index being built: `known` (an answer this run
/// already has, from [`at_head`], over every leaf of the base) goes in, then
/// the leaves `want` names that it did not cover are asked for and go in
/// too, and the via set follows: replaced when the answers cover every leaf
/// the index holds, widened otherwise.
pub(crate) fn refresh(
    q: &dyn PackageQuerier,
    b: &mut IndexBuilder<'_>,
    indexed: &[String],
    known: Option<&Answer>,
    want: Want,
) {
    let asked: BTreeSet<&str> = known
        .map(|a| a.leaves.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let rest: Vec<String> = match want {
        Want::Nothing => Vec::new(),
        Want::AddedSince(from) => leaves(b, from),
        Want::All => leaves(b, 0),
    }
    .into_iter()
    .filter(|l| !asked.contains(l.as_str()))
    .collect();
    let fresh = ask(q, indexed, rest);
    let mut changed = 0;
    let mut via: BTreeSet<String> = BTreeSet::new();
    for a in known.into_iter().chain(fresh.as_ref()) {
        let (c, missing) = apply(b, a);
        changed += c;
        via.extend(a.via.iter().cloned());
        via.extend(missing);
    }
    // Every leaf is covered when the run's answer was over the base's
    // leaves and this splice asked for the ones it added, or when this
    // splice asked for all of them; otherwise what the base was derived
    // through still stands beside the new answers.
    let complete = fresh.is_some() && (known.is_some() || want == Want::All);
    if !complete {
        via.extend(b.round_trip_via().iter().cloned());
    }
    if changed > 0 {
        tracing::debug!(changed, via = via.len(), "round-trip rows written");
    }
    b.set_round_trip_via(via);
}
