//! Building [`RepoInputs`] for a checkout: every main-repo label a repository
//! rule, a module extension or a workspace-closure `.bzl` names, attributed
//! per occurrence to the repositories the code it sits in can create.

use std::collections::{BTreeMap, BTreeSet};

use crate::attribution::Attribution;
use crate::label::{rule_label, Label};
use crate::loads::{FileText, LoadGraph, RepoMapping};
use crate::starlark::{stmt_exprs, walk_stmts, ArgKind, ExprKind, Module, Stmt};

use super::bazelrc::{repo_settings, RepoSetting};
use super::module_facts::{apparent_of, ModuleFacts};
use super::path_maps::scan_path_maps;
use super::pattern::{OriginKind, PathMap, Pattern, RepoInputs, RepoOrigin};

/// What a main-repo label an external repository names can be an input
/// through; empty when the string does not name the main repository, or does
/// not name anything at all.
///
/// A label reaches a repository in exactly two ways, and this returns one
/// pattern for each:
///
/// - **as a file.** A repository rule and a module extension run during
///   loading and can read *source files* only: `repository_ctx.path`, `read`,
///   `template` and `symlink`, and a repository rule's `attr.label`, all
///   reject a rule target. So the only path a label can feed a repository
///   from is the file it names, exactly — [`Pattern::Exact`]. `//pkg` with no
///   target is `//pkg:pkg`, whose file reading is `pkg/pkg`.
/// - **as a rule target.** The repository's *generated targets* may depend on
///   it — envoy's `bazel/repo.bzl` writes a BUILD file whose `genrule` lists
///   `@envoy//changelogs` in `tools` — so a change that moves that target
///   moves the repository's targets with it. Which files those are is not
///   knowable here (the target is a `filegroup` over a glob), but it is
///   knowable from the graph, so the label is recorded as written —
///   [`Pattern::Label`], canonical `//pkg:name` — and the engine asks the
///   reverse walk whether the change reached that node.
///
/// The label kind replaces a [`Pattern::Prefix`] over the label's whole
/// package subtree, which was the old approximation of "the files that rule
/// is built from". Visibility specs and `config_setting` keys made that claim
/// over hundreds of repositories at once and 91% of a large repository's
/// packages (issue #8); the graph gives the exact answer for the one case it
/// was covering.
pub fn main_repo_patterns(label: &str, m: &RepoMapping) -> Vec<Pattern> {
    let Some(l) = Label::parse(label) else {
        return Vec::new();
    };
    // Another repository's file is not a main-repo input, and a relative
    // `:name` in a `.bzl` is not a label a repository rule resolves.
    if l.is_relative() || !l.is_main(m) {
        return Vec::new();
    }
    let rest = l.rest();
    // `rest` is `pkg:name` or `pkg`, without the leading `//`. A format
    // placeholder cuts the label short: `Label("//third_party/gpus/%s.tpl" %
    // name)` is how a configure rule reaches every template it writes into
    // its repository, and the literal text before the placeholder is what can
    // be said about it. Whatever the placeholder becomes, the label starts
    // with that text, and so does the path once `:` is read as `/`. That text
    // is the input, as a string prefix (wider than the files the rule reads,
    // never narrower). Cutting further back, to the directory, once turned
    // tensorflow's `//third_party/systemlibs%s.tpl` into `third_party/`, which
    // every edit under the vendored `third_party/xla` then matched, and a
    // configure repository nothing had touched re-queried its four hundred
    // loading packages on each of those commits. xla's
    // `rocm/build_defs.bzl.tpl` went through exactly this and was an input of
    // nothing. Whitespace or any other byte a label cannot carry means this
    // was never a label: text scraped out of a URL.
    if let Some(cut) = rest.find(['%', '{']) {
        let head = &rest[..cut];
        if head.is_empty() || !head.bytes().all(is_label_byte) {
            return Vec::new();
        }
        // A label whose package is itself the placeholder says nothing.
        let Some((dir, _)) = head.rsplit_once(['/', ':']) else {
            return Vec::new();
        };
        if dir.is_empty() || dir.starts_with("__") {
            return Vec::new();
        }
        return vec![Pattern::Prefix(head.replace(':', "/"))];
    }
    if !rest.bytes().all(is_label_byte) {
        return Vec::new();
    }
    // `//pkg` is `//pkg:pkg`, a rule target. Only the file it might also be —
    // a source file called `pkg` in that directory — is an input.
    let (pkg, name) = (l.package(), l.name());
    // A visibility token names no file and can never be read by a rule.
    if name.is_empty() || name.starts_with("__") {
        return Vec::new();
    }
    // The file the label names, and the target it names; see the doc comment.
    // A source-file label is covered both ways at once, and harmlessly: its
    // `Label` pattern resolves to the file's own node, whose reverse row is
    // the rules that consume it.
    vec![
        Pattern::Exact(l.path()),
        Pattern::Label(rule_label(pkg, name)),
    ]
}

/// The repo-relative path of a main-repo label naming a *file*, for the one
/// attribute whose whole point is which file it is: `build_file`.
///
/// Narrower than [`main_repo_patterns`], which answers "what could this label
/// feed": a `build_file` that is a format string, a relative label, or
/// another repository's file is not a tree file this build can read, and a
/// guess would put a path map on the wrong file.
fn main_repo_file(label: &str, m: &RepoMapping) -> Option<String> {
    let l = Label::parse(label)?;
    if l.is_relative() || !l.is_main(m) {
        return None;
    }
    let rest = l.rest();
    if rest.is_empty() || !rest.bytes().all(is_label_byte) {
        return None;
    }
    let name = l.name();
    if name.is_empty() || name.starts_with("__") {
        return None;
    }
    let path = l.path();
    (!path.is_empty()).then_some(path)
}

/// What an `Overlay` claim records besides the origin: the BUILD file lands
/// at the package root of the repository, which is what `build_hash` and the
/// closure need to find it, and the mapping is *not* exhaustive — the sources
/// come from somewhere else entirely, so a path it does not cover says
/// nothing about whether the repository exports a `.bzl`.
fn overlay_map(path: &str) -> PathMap {
    PathMap {
        from: path.to_string(),
        to: "BUILD.bazel".to_string(),
        exhaustive: false,
    }
}

/// The `.bazelrc` lines that make a target of this tree an input of an
/// external repository's targets.
///
/// `build --@quiche//:quic_base_impl_lib=//source/common/quic/platform:...`
/// says a `label_flag` of `@quiche` resolves to a target of this tree, so
/// every `@quiche` target that reads that flag depends on it. At envoy's
/// bzlmod pin this is the only route back into the main repository that
/// exists at all: the overlay BUILD file is hosted in the registry and names
/// nothing of this tree.
///
/// Filtered from [`repo_settings`] by the two things text alone cannot
/// decide: a flag on the main repository under its own apparent name
/// (`--@envoy//...` inside envoy) is not an external repository's flag, and a
/// value that is not a label of this tree is not an input.
pub fn repo_setting_inputs(src: &dyn FileText, m: &RepoMapping) -> Vec<RepoSetting> {
    let Some(text) = src.text(".bazelrc") else {
        return Vec::new();
    };
    repo_settings(&text)
        .into_iter()
        .filter(|s| {
            Label::parse(&s.flag).is_some_and(|l| !l.is_main(m))
                && !main_repo_patterns(&s.value, m).is_empty()
        })
        .collect()
}

/// Computes the repository-input map for a checkout.
///
/// `graph` is the load graph the closure walk already built, so the `.bzl`
/// files here are read from the same memoised parse; `workspace` is that
/// walk's workspace closure.
pub fn build_repo_inputs(
    src: &dyn FileText,
    m: &RepoMapping,
    graph: &LoadGraph,
    workspace: &[u32],
    facts: &ModuleFacts,
) -> RepoInputs {
    let mut out = RepoInputs::default();
    // `@envoy//...` inside envoy is a main-repo label; see
    // [`RepoMapping::with_main_alias`].
    let m = &m.clone().with_main_alias(&facts.module_name);

    // Seed: which `.bzl` each extension / repo rule is defined in, and the
    // repositories that come out of it.
    let mut seeds: Vec<(String, BTreeSet<String>)> = Vec::new();
    for e in facts.extensions.values() {
        if let Some(p) = main_bzl_path(&e.bzl, m) {
            seeds.push((p, e.repos.clone()));
        }
        for l in &e.labels {
            for pat in main_repo_patterns(l, m) {
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(e.repos.iter().cloned());
            }
        }
        out.repos.extend(e.repos.iter().cloned());
    }
    for r in facts.repo_rules.values() {
        if let Some(p) = main_bzl_path(&r.bzl, m) {
            seeds.push((p, r.repos.clone()));
        }
        for l in &r.labels {
            for pat in main_repo_patterns(l, m) {
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(r.repos.iter().cloned());
            }
        }
        out.repos.extend(r.repos.iter().cloned());
    }
    // `local_path_override(module_name = "envoy_api", path = "api")`: the whole
    // directory is that module's source, and its root is that directory.
    for (module, path) in &facts.local_paths {
        let repo = apparent_of(facts, module).unwrap_or_else(|| module.clone());
        out.repos.insert(repo.clone());
        out.patterns
            .entry(Pattern::Prefix(path.clone()))
            .or_default()
            .insert(repo.clone());
        out.path_maps
            .entry(repo.clone())
            .or_default()
            .insert(PathMap::exhaustive_dir(path.clone()));
        RepoOrigin::claim(&mut out.origins, &repo, dir_origin(path, "MODULE.bazel"));
    }
    // The same shape written as a repository rule in `MODULE.bazel`.
    for (repo, path) in &facts.local_repo_paths {
        if !out.repos.contains(repo) {
            continue;
        }
        out.patterns
            .entry(Pattern::Prefix(path.clone()))
            .or_default()
            .insert(repo.clone());
        out.path_maps
            .entry(repo.clone())
            .or_default()
            .insert(PathMap::exhaustive_dir(path.clone()));
        RepoOrigin::claim(&mut out.origins, repo, dir_origin(path, "MODULE.bazel"));
    }
    // `new_local_repository(name = "quiche", build_file =
    // "@envoy//bazel/external:quiche.BUILD")`: the sources are fetched and
    // the BUILD file is a file of this tree. The file's own labels are read
    // below, once the repository set is settled.
    let mut overlays: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (repo, label) in &facts.build_files {
        if !out.repos.contains(repo) && !facts.deps.contains_key(repo) {
            continue;
        }
        // A `bazel_dep` gets no row of its own today, because a declaration
        // alone says nothing about this tree. One an overlay claim names does
        // say something, so it gets one here.
        out.repos.insert(repo.clone());
        let path = main_repo_file(label, m);
        RepoOrigin::claim(
            &mut out.origins,
            repo,
            overlay_origin(path.as_deref(), "MODULE.bazel"),
        );
        if let Some(path) = path {
            out.path_maps
                .entry(repo.clone())
                .or_default()
                .insert(overlay_map(&path));
            overlays.entry(path).or_default().insert(repo.clone());
        }
    }
    for d in facts.deps.values() {
        out.canonical
            .insert(d.repo_name.clone(), m.canonical(&d.repo_name).to_string());
    }

    // Propagate down the load graph: a helper a repository rule's `.bzl`
    // loads is part of that rule's implementation.
    let mut owner: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for (path, repos) in &seeds {
        let Some(id) = graph.id_of(path) else {
            continue;
        };
        let mut stack = vec![id];
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            owner.entry(i).or_default().extend(repos.iter().cloned());
            stack.extend_from_slice(graph.loads(i));
        }
    }

    // Per-occurrence attribution. The analysis reads every file of the
    // closure, the workspace files included: a `local_repository(name = ...)`
    // written in `WORKSPACE` names a repository nothing else does, and a
    // `MODULE.bazel` tag call is a call site like any other.
    let mut texts: Vec<(String, String)> = Vec::new();
    for name in crate::loads::WORKSPACE_FILES {
        if let Some(t) = src.text(name) {
            texts.push((name.to_string(), t));
        }
    }
    for &id in workspace {
        let path = graph.path(id).to_string();
        if let Some(t) = src.text(&path) {
            texts.push((path, t));
        }
    }
    let mut att = Attribution::scan(&texts, m);
    // The universe a fallback ranges over: every repository `MODULE.bazel`
    // names, plus every literal `name =` written anywhere in the closure. On a
    // WORKSPACE-only repository the first is empty and the second is the whole
    // answer, which is what turns "no idea, invalidate everything" into a set.
    out.repos.extend(att.literal_names.iter().cloned());
    // A file under a seeded subtree falls back to that subtree's few
    // repositories; any other falls back to the universe. This is the one
    // place that decides what a file's fallback is; the attribution pass
    // decides when a file gets it.
    let universe = out.repos.clone();
    let fallback_of = |p: &str| -> (BTreeSet<String>, bool) {
        match graph.id_of(p).and_then(|id| owner.get(&id)) {
            Some(r) if !r.is_empty() => (r.clone(), false),
            _ => (universe.clone(), true),
        }
    };
    let mut wide: BTreeSet<String> = BTreeSet::new();
    let fallbacks: BTreeMap<String, BTreeSet<String>> = texts
        .iter()
        .map(|(p, _)| {
            let (repos, is_wide) = fallback_of(p);
            if is_wide {
                wide.insert(p.clone());
            }
            (p.clone(), repos)
        })
        .collect();
    let mut seeds_by_sym: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for e in facts.extensions.values() {
        if let Some(p) = main_bzl_path(&e.bzl, m) {
            seeds_by_sym
                .entry((p, e.name.clone()))
                .or_default()
                .extend(e.repos.iter().cloned());
        }
    }
    for r in facts.repo_rules.values() {
        if let Some(p) = main_bzl_path(&r.bzl, m) {
            seeds_by_sym
                .entry((p, r.rule.clone()))
                .or_default()
                .extend(r.repos.iter().cloned());
        }
    }
    att.solve(&fallbacks, &seeds_by_sym, &wide);

    // A main-repo label in a workspace-closure `.bzl`, or in a workspace file
    // itself, is an input of the repositories the *occurrence* is attributed
    // to. The workspace files are read too: a WORKSPACE-only repository writes
    // `npm_translate_lock(name = "npm", pnpm_lock = "//:pnpm-lock.yaml")`
    // straight into `WORKSPACE`, and the lock is then the one file that moves
    // every target the repository generates.
    //
    // Which *kind* of input is tracked alongside: everything derived so far
    // (`MODULE.bazel` shapes) and every literal that is itself a label feeds
    // the rule; a label embedded in a larger literal is text the rule writes.
    // A pattern is target-only when no occurrence of it was the first kind.
    let mut direct: BTreeSet<Pattern> = out.patterns.keys().cloned().collect();
    let mut embedded: BTreeSet<Pattern> = BTreeSet::new();
    // A `.bzl`'s own answer — what a change to it can move — is
    // [`Attribution::file_repos`]: what it defines itself, plus what every use
    // of the symbols it exports does. The extra repositories a *label* in it
    // feeds are folded in here.
    let mut own: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // Claims read out of the closure, held aside until the repository set is
    // final: a claim about a name no repository table will carry is not a
    // claim about anything.
    let mut origins_from_bzl: BTreeMap<String, RepoOrigin> = BTreeMap::new();
    for (path, module) in att.modules() {
        let path = path.to_string();
        let mut file_repos = att.file_repos(&path).repos;
        for (offset, l) in string_literals(module) {
            // The BUILD text a repository rule writes with
            // `repository_ctx.file("BUILD", ...)` says what the repository
            // *is*: envoy's `@envoy_repo`. Only an exactly attributed
            // occurrence makes the claim — a fallback here would call every
            // repository in the universe generated.
            if l.build_text && !l.embedded {
                let a = att.at(&path, offset);
                if a.exact {
                    for r in &a.repos {
                        RepoOrigin::claim(&mut origins_from_bzl, r, generated_origin(&path));
                    }
                }
            }
            let pats = main_repo_patterns(&l.text, m);
            if pats.is_empty() {
                continue;
            }
            let a = att.at(&path, offset);
            if a.repos.is_empty() {
                continue;
            }
            // A prefix is a claim over a whole directory. Made on the
            // fallback it says every edit beneath that directory moves every
            // repository in the universe, which is the shape issue #8 removed
            // and issue #9 found back: three such rows over a build-config
            // tree, each at 3,817 repositories. Unattributed, the format
            // string is no input at all, as before.
            if !a.exact && pats.iter().any(Pattern::is_prefix) {
                continue;
            }
            file_repos.extend(a.repos.iter().cloned());
            // A generated `.bzl` can `load()` a main-repo `.bzl` it names, so
            // an embedded `.bzl` label is still a real input.
            let feeds_rule = !l.embedded || l.text.ends_with(".bzl");
            for pat in pats {
                if feeds_rule {
                    direct.insert(pat.clone());
                } else {
                    embedded.insert(pat.clone());
                }
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(a.repos.iter().cloned());
            }
        }
        own.insert(path, file_repos);
    }
    // Path mappings, from the workspace files as well as the `.bzl` closure.
    // A `local_repository` call names its own repository; a `symlink` does
    // not, so it is attributed from the position of the call — inside the
    // implementation of the rule the mapping is true of — and dropped when
    // that position is unattributable, rather than being handed to every
    // repository the main repository defines.
    for (path, module) in att.modules() {
        let maps = scan_path_maps(module, m);
        for (repo, pm) in maps.named {
            if out.repos.contains(&repo) {
                let pat = Pattern::Prefix(pm.from.clone());
                direct.insert(pat.clone());
                out.patterns.entry(pat).or_default().insert(repo.clone());
                own.entry(path.to_string())
                    .or_default()
                    .insert(repo.clone());
                RepoOrigin::claim(&mut origins_from_bzl, &repo, dir_origin(&pm.from, path));
                out.path_maps.entry(repo).or_default().insert(pm);
            }
        }
        for (repo, label, line) in maps.overlays {
            if !out.repos.contains(&repo) && !facts.deps.contains_key(&repo) {
                continue;
            }
            out.repos.insert(repo.clone());
            let file = (!label.is_empty())
                .then(|| main_repo_file(&label, m))
                .flatten();
            let evidence = format!("{path}:{line}");
            RepoOrigin::claim(
                &mut origins_from_bzl,
                &repo,
                overlay_origin(file.as_deref(), &evidence),
            );
            if let Some(file) = file {
                out.path_maps
                    .entry(repo.clone())
                    .or_default()
                    .insert(overlay_map(&file));
                overlays.entry(file).or_default().insert(repo.clone());
            }
        }
        for (offset, pm) in &maps.symlinks {
            let a = att.at(path, *offset);
            if !a.exact || a.repos.is_empty() {
                continue;
            }
            for repo in &a.repos {
                out.path_maps
                    .entry(repo.clone())
                    .or_default()
                    .insert(pm.clone());
            }
        }
    }
    // What a change to a workspace `.bzl` moves. The seed propagation down
    // `load()` edges is *not* part of this: it says a loader's whole answer
    // belongs to everything it loads, which on a root file that loads a
    // hundred `init.bzl` files is the universe. Per-symbol use sites say the
    // same thing more finely — `pcl()` in the root's macro is answered by
    // `init()`'s own repositories — and a file whose set comes out empty
    // keeps the fallback.
    for &id in workspace {
        let path = graph.path(id).to_string();
        let mut repos = own.remove(&path).unwrap_or_default();
        if repos.is_empty() {
            // Empty is the answer for a file read in full whose only
            // fallback would be everything (issue #9); the wide answer is
            // for the rest, and for a file of the closure this checkout
            // does not have, which nothing read at all.
            repos = att
                .fallback_for(&path)
                .unwrap_or_else(|| fallback_of(&path).0);
        }
        out.bzl_repos.insert(path, repos);
    }
    for (repo, o) in origins_from_bzl {
        if out.repos.contains(&repo) {
            RepoOrigin::claim(&mut out.origins, &repo, o);
        }
    }
    // The BUILD file of an overlay repository is a file of this tree, and
    // every main-repo label in it is a dependency of that repository's
    // *targets* — `//source/common/quic/platform:quic_base_impl_lib` in
    // envoy's `bazel/external/quiche.BUILD`. That is the same kind of input
    // as a label embedded in the BUILD text a repository rule writes, and it
    // is recorded the same way: target-only, so the engine applies rule (1)
    // alone and the repository fixpoint does the rest.
    //
    // Read off the text rather than parsed: a BUILD file is Starlark, but
    // what is wanted from it is every label it names, which is what
    // `labels_in` answers for the generated case already.
    for (file, repos) in &overlays {
        let Some(text) = src.text(file) else {
            continue;
        };
        for label in labels_in(&text) {
            for pat in main_repo_patterns(&label, m) {
                embedded.insert(pat.clone());
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(repos.iter().cloned());
            }
        }
    }
    // The same kind of input again, written in configuration: a `.bazelrc`
    // line that resolves an external repository's build setting to a target
    // of this tree. Target-only, so rule (1) and the repository fixpoint
    // answer it, exactly as the overlay BUILD file's labels are answered.
    for s in repo_setting_inputs(src, m) {
        // A plain `bazel_dep` has no row of its own, because a declaration
        // says nothing about this tree. One a setting line names does say
        // something, so it gets one here.
        if !out.repos.contains(&s.repo) && !facts.deps.contains_key(&s.repo) {
            continue;
        }
        out.repos.insert(s.repo.clone());
        for pat in main_repo_patterns(&s.value, m) {
            embedded.insert(pat.clone());
            out.patterns.entry(pat).or_default().insert(s.repo.clone());
        }
    }
    out.target_only = embedded.difference(&direct).cloned().collect();

    out
}

/// The three origin claims, spelled once each.
fn dir_origin(dir: &str, evidence: &str) -> RepoOrigin {
    RepoOrigin {
        kind: OriginKind::Dir,
        dir: dir.to_string(),
        evidence: evidence.to_string(),
        ..RepoOrigin::default()
    }
}

fn overlay_origin(build_file: Option<&str>, evidence: &str) -> RepoOrigin {
    RepoOrigin {
        kind: OriginKind::Overlay,
        dir: build_file.unwrap_or_default().to_string(),
        evidence: evidence.to_string(),
        ..RepoOrigin::default()
    }
}

fn generated_origin(evidence: &str) -> RepoOrigin {
    RepoOrigin {
        kind: OriginKind::Generated,
        evidence: evidence.to_string(),
        ..RepoOrigin::default()
    }
}

/// The repo-relative path of a `.bzl` label in the main repository, under
/// the run's repository mapping: `@envoy//bazel:repo.bzl` resolves here when
/// `envoy` is a main alias. The `WORKSPACE` diff has no mapping and reads
/// only the literal spellings; see `workspace::literal_main_bzl_path`.
fn main_bzl_path(label: &str, m: &RepoMapping) -> Option<String> {
    match crate::loads::resolve_label(label, "", m) {
        Some(crate::loads::Load::Main(p)) => Some(p),
        _ => None,
    }
}

/// Every label-shaped token inside a Starlark file's string literals.
///
/// Two levels, because repository rules use both. A label is usually a literal
/// of its own (`attr.label(default = "@envoy//:.github/config.yml")`), but the
/// other half of the idiom is a repository rule that *writes a BUILD file*:
///
/// ```text
/// rctx.file("BUILD", '''
/// genrule(
///     tools = ["@envoy//:VERSION.txt", "@envoy//changelogs"],
/// )
/// ''')
/// ```
///
/// That whole block is one string literal, and the labels the generated
/// repository will depend on are inside it. So each literal is scanned for
/// label-shaped substrings as well as being offered whole.
///
/// The two are not the same kind of input, and the flag says which is which.
/// A literal that *is* a label is one the rule resolves: Bazel reads the file
/// and the rule may write anything it likes from it, `.bzl` files included. A
/// label found *inside* a larger literal is text the rule writes into the
/// repository, and what it names is consumed by the repository's targets at
/// build time — `@envoy_repo//:project` lists `@envoy//changelogs` in its
/// `tools`. That file's content cannot move a `.bzl` the repository exports,
/// which is what lets the engine skip re-querying the twelve hundred envoy
/// packages whose load closure reaches `@envoy_repo//:compiler.bzl` on every
/// commit that adds a changelog entry.
///
/// Comments and nesting are the parser's business, so a `//` in a comment is
/// not a label; a literal inside a statement the parser could not read is
/// still found, off the tokens. A `//` preceded by `:` or `/` is not a label
/// either, which is what keeps `https://github.com/envoyproxy/envoy` out.
/// Each literal comes back with its byte offset, which is what the
/// per-occurrence attribution is keyed on.
fn string_literals(m: &Module<'_>) -> Vec<(usize, Literal)> {
    let mut raw: Vec<(usize, String)> = Vec::new();
    // The label of a `load()` is an edge of the load graph, not a file
    // the repository rule reads: the `.bzl` it names is in the workspace
    // closure, and a change to it is answered by that file's own
    // repositories. Read as an input it would make every `.bzl` a root
    // workspace file loads an input of everything that file can define.
    // `use_repo_rule("//tools:gen.bzl", "gen_repo")` and
    // `use_extension("//bazel:ext.bzl", "ext")` in `MODULE.bazel` are the
    // same edge: written at module level, where nothing attributes them
    // more finely than the whole universe, so as inputs they would feed
    // every repository the module declares.
    let mut skip: BTreeSet<usize> = BTreeSet::new();
    // The literals passed to `repository_ctx.file("BUILD", ...)` and
    // `.template("BUILD.bazel", ...)`: the text that becomes the
    // repository's BUILD file. The enclosing call is what makes it that, and
    // nothing downstream can tell from the literal alone.
    let mut build_texts: BTreeSet<usize> = BTreeSet::new();
    let mut skip_first = |args: &[crate::starlark::Arg<'_>]| {
        if let Some(a) = args.first() {
            if matches!(a.kind, ArgKind::Positional(_)) && a.value().as_str().is_some() {
                skip.insert(a.value().span.start);
            }
        }
    };
    walk_stmts(&m.stmts, &mut |st| {
        match st {
            Stmt::Unparsed(span) => {
                raw.extend(m.strings_in(span).into_iter().map(|(s, v)| (s.start, v)));
            }
            Stmt::Load(l) => skip_first(&l.args),
            _ => {}
        }
        for e in stmt_exprs(st) {
            e.walk(&mut |x| match &x.kind {
                ExprKind::Str(s) => raw.push((x.span.start, s.clone())),
                ExprKind::Unparsed => {
                    raw.extend(m.strings_in(&x.span).into_iter().map(|(s, v)| (s.start, v)));
                }
                ExprKind::Call { callee, args }
                    if matches!(callee.as_name(), Some("use_repo_rule" | "use_extension")) =>
                {
                    skip_first(args);
                }
                ExprKind::Call { callee, args }
                    if matches!(
                        &callee.kind,
                        ExprKind::Attr {
                            name: "file" | "template",
                            ..
                        }
                    ) =>
                {
                    mark_build_text(args, &mut build_texts);
                }
                _ => {}
            });
        }
    });
    let mut out = Vec::new();
    for (offset, lit) in raw {
        if skip.contains(&offset) {
            continue;
        }
        let build_text = build_texts.contains(&offset);
        for l in labels_in(&lit) {
            if l != lit {
                out.push((
                    offset,
                    Literal {
                        text: l,
                        embedded: true,
                        build_text,
                    },
                ));
            }
        }
        out.push((
            offset,
            Literal {
                text: lit,
                embedded: false,
                build_text,
            },
        ));
    }
    out
}

/// One string literal of a Starlark file, or a label found inside one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Literal {
    text: String,
    /// `true` for a label-shaped substring of a larger literal: text the
    /// repository rule writes, not a label it resolves.
    embedded: bool,
    /// `true` when the literal this came from is the BUILD file text a
    /// repository rule writes: `rctx.file("BUILD", '''...''')`. That makes
    /// the repository a [`OriginKind::Generated`] one.
    build_text: bool,
}

/// Marks the literals of a `file("BUILD", ...)` / `template("BUILD.bazel",
/// ...)` call other than the file name itself.
///
/// Narrow on purpose: only a first argument that is literally one of Bazel's
/// two BUILD file names counts, so a rule writing `rctx.file(name + ".bzl")`
/// claims nothing. Every other argument's literals are marked, because which
/// one carries the text differs between `file(path, content)` and
/// `template(path, tpl, substitutions)`, and marking one too many can only
/// add a claim about a repository that does write a BUILD file.
fn mark_build_text(args: &[crate::starlark::Arg<'_>], out: &mut BTreeSet<usize>) {
    let mut it = args
        .iter()
        .filter(|a| matches!(a.kind, ArgKind::Positional(_)));
    let Some(first) = it.next() else {
        return;
    };
    if !matches!(first.value().as_str(), Some("BUILD" | "BUILD.bazel")) {
        return;
    }
    for a in args.iter().filter(|a| a.value().span != first.value().span) {
        a.value().walk(&mut |x| {
            if matches!(x.kind, ExprKind::Str(_)) {
                out.insert(x.span.start);
            }
        });
    }
}

fn is_label_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'+' | b'~' | b'/' | b':' | b'@')
}

/// Label-shaped substrings of a blob of text.
fn labels_in(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] != b'/' || b[i + 1] != b'/' {
            i += 1;
            continue;
        }
        // `https://`, and a path that merely contains `//`.
        if i > 0 && matches!(b[i - 1], b':' | b'/') {
            i += 2;
            continue;
        }
        // Expand left over an `@repo` prefix, if there is one.
        let mut start = i;
        while start > 0 && is_label_byte(b[start - 1]) && b[start - 1] != b'/' {
            start -= 1;
        }
        if start != i && b[start] != b'@' {
            i += 2;
            continue;
        }
        let mut end = i + 2;
        while end < b.len() && is_label_byte(b[end]) {
            end += 1;
        }
        if let Ok(l) = std::str::from_utf8(&b[start..end]) {
            out.push(l.trim_end_matches(['.', ':', '-']).to_string());
        }
        i = end.max(i + 2);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::string_literals;
    use crate::starlark::Module;

    #[test]
    fn a_label_inside_a_larger_literal_is_embedded() {
        let m =
            Module::parse("A = \"@envoy//:VERSION.txt\"\nB = 'tools = [\"@envoy//changelogs\"]'\n");
        let lits = string_literals(&m);
        let find = |text: &str| -> Vec<bool> {
            lits.iter()
                .filter(|(_, l)| l.text == text)
                .map(|(_, l)| l.embedded)
                .collect()
        };
        assert_eq!(find("@envoy//:VERSION.txt"), vec![false]);
        assert_eq!(find("@envoy//changelogs"), vec![true]);
    }
}
