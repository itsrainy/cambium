//! The consumer table's leaf map: which leaves of an external repository
//! that consumes an indexed one reach which of its packages.
//!
//! The table itself (`RepoInputs::consumers`) says that `@grpc` consumes
//! `@envoy_api` and through which packages. The index holds no graph for
//! `@grpc`, only the labels of it that `//...` names, each a node with no
//! outgoing edges, so the unit a change can seed is one of those leaves.
//! Seeding every leaf of a consumer whenever a package it consumes moves is
//! sound and wide: on envoy's step whose oracle is 649 labels, one of grpc's
//! five leaves reaches the API and the other four brought the answer to 5035.
//! The map here is what narrows it, and it is Bazel's answer, asked once per
//! indexed repository with every leaf of every consumer in one query
//! ([`cambium_bazel::Bazel::leaf_reaches`]). The same answer says what each
//! leaf's closure runs through outside the repository (`Consumer::via`),
//! which is what a later run holds the map's freshness against.

use std::collections::{BTreeMap, BTreeSet};

use cambium_index::externals::{Consumer, LeafReach};
use cambium_index::IndexBuilder;

use crate::query::PackageQuerier;

/// The external labels an index being built holds, less its tombstones:
/// the candidates for a consumer's leaves.
pub(crate) fn external_labels(b: &IndexBuilder<'_>) -> Vec<String> {
    (0..b.num_nodes())
        .filter(|&id| !b.is_tombstone(id))
        .map(|id| b.label(id))
        .filter(|l| l.starts_with('@'))
        .map(str::to_string)
        .collect()
}

/// Whether `label` is a label of the consumer repository `consumer`, matched
/// the way `externals::reach` rule (1) matches a dirty repository's labels,
/// so that the leaves the map knows are the leaves rule (1) would seed.
pub(crate) fn is_leaf_of(label: &str, consumer: &str, canonical: &str) -> bool {
    cambium_bazel::label_repo_token(label)
        .is_some_and(|t| crate::externals::repo_token_matches(t, consumer, canonical))
}

/// Leaf questions a run already asked at head, by indexed repository: the
/// leaves asked about, in order, and Bazel's answer. The splice reuses an
/// answer for the same leaves rather than ask again.
pub(crate) type Asked = BTreeMap<String, (Vec<String>, Option<BTreeMap<String, LeafReach>>)>;

/// The consumer table Bazel answered (`PackageQuerier::consumer_repos`),
/// with each consumer's leaf map derived against `labels`, the external
/// labels of the index the table goes into.
///
/// One question per indexed repository, over the leaves of all its
/// consumers at once. A consumer with no leaf in the index gets an empty map
/// without asking, since there is nothing rule (1) could seed for it. A
/// question Bazel will not answer leaves every consumer of that repository
/// with no map, which is the wide answer. A question `asked` already holds
/// the answer to, over exactly these leaves, is not asked again.
pub(crate) fn with_leaves(
    q: &dyn PackageQuerier,
    table: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    labels: &[String],
    canonical: &BTreeMap<String, String>,
    asked: &Asked,
) -> BTreeMap<String, BTreeMap<String, Consumer>> {
    let mut out = BTreeMap::new();
    for (repo, consumers) in table {
        let own: BTreeMap<&str, Vec<String>> = consumers
            .keys()
            .map(|c| {
                let canon = canonical.get(c).map_or("", String::as_str);
                let leaves = labels
                    .iter()
                    .filter(|l| is_leaf_of(l, c, canon))
                    .cloned()
                    .collect();
                (c.as_str(), leaves)
            })
            .collect();
        let all: Vec<String> = own
            .values()
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let answer = if all.is_empty() {
            Some(BTreeMap::new())
        } else if let Some((_, a)) = asked.get(&repo).filter(|(l, _)| *l == all) {
            tracing::debug!(
                %repo,
                leaves = all.len(),
                "the consumer leaf map was already asked for at head in this run"
            );
            a.clone()
        } else {
            let started = std::time::Instant::now();
            let a = q.consumer_leaf_packages(&repo, &all);
            tracing::debug!(
                %repo,
                leaves = all.len(),
                answered = a.as_ref().map(BTreeMap::len),
                reaching = a
                    .as_ref()
                    .map(|m| m.values().filter(|r| !r.packages.is_empty()).count()),
                ms = started.elapsed().as_millis() as u64,
                "the consumer leaf map was asked for"
            );
            a
        };
        let entry: BTreeMap<String, Consumer> = consumers
            .iter()
            .map(|(c, packages)| {
                let known: Vec<(&String, &LeafReach)> = answer
                    .iter()
                    .flat_map(|a| {
                        own[c.as_str()]
                            .iter()
                            .filter_map(|l| a.get(l).map(|r| (l, r)))
                    })
                    .collect();
                let leaves = answer.as_ref().map(|_| {
                    known
                        .iter()
                        .map(|(l, r)| ((*l).clone(), r.packages.clone()))
                        .collect()
                });
                // Pooled over the consumer's leaves: what any of them was
                // derived through moving makes the whole map suspect.
                let via = known
                    .iter()
                    .flat_map(|(_, r)| r.via.iter().cloned())
                    .collect();
                (
                    c.clone(),
                    Consumer {
                        packages: packages.clone(),
                        leaves,
                        via,
                    },
                )
            })
            .collect();
        out.insert(repo, entry);
    }
    out
}
