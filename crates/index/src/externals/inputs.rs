//! Building [`RepoInputs`] for a checkout: every main-repo label a repository
//! rule, a module extension or a workspace-closure `.bzl` names, attributed
//! per occurrence to the repositories the code it sits in can create.

use std::collections::{BTreeMap, BTreeSet};

use crate::attribution::Attribution;
use crate::loads::{lex, unescape, FileText, LoadGraph, RepoMapping, Tok};

use super::module_facts::{apparent_of, ModuleFacts};
use super::path_maps::scan_bzl_path_maps;
use super::pattern::{PathMap, Pattern, RepoInputs};

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
    let rest = match label.strip_prefix("@@") {
        Some(r) => match r.split_once("//") {
            Some(("", rest)) => rest,
            _ => return Vec::new(),
        },
        None => match label.strip_prefix('@') {
            Some(r) => match r.split_once("//") {
                Some((repo, rest)) if m.is_main(repo) => rest,
                _ => return Vec::new(),
            },
            None => match label.strip_prefix("//") {
                Some(rest) => rest,
                None => return Vec::new(),
            },
        },
    };
    // `rest` is now `pkg:name` or `pkg`, without the leading `//`. A format
    // placeholder cuts the label short: `Label("//third_party/gpus/%s.tpl" %
    // name)` is how a configure rule reaches every template it writes into
    // its repository, and the directory before the placeholder is what can be
    // said about it. That directory is the input, as a prefix — wider than
    // the files the rule reads, never narrower. xla's
    // `rocm/build_defs.bzl.tpl` went through exactly this and was an input of
    // nothing. Whitespace or any other byte a label cannot carry means this
    // was never a label: text scraped out of a URL.
    if let Some(cut) = rest.find(['%', '{']) {
        let head = &rest[..cut];
        if head.is_empty() || !head.bytes().all(is_label_byte) {
            return Vec::new();
        }
        let dir = match head.rsplit_once(['/', ':']) {
            Some((d, _)) => d,
            None => return Vec::new(),
        };
        if dir.is_empty() || dir.starts_with("__") {
            return Vec::new();
        }
        return vec![Pattern::Prefix(format!("{dir}/"))];
    }
    if !rest.bytes().all(is_label_byte) {
        return Vec::new();
    }
    let (pkg, name) = match rest.split_once(':') {
        Some((p, n)) => (p, n),
        // `//pkg` is `//pkg:pkg`, a rule target. Only the file it might also
        // be — a source file called `pkg` in that directory — is an input.
        None => (rest, rest.rsplit('/').next().unwrap_or(rest)),
    };
    // A visibility token names no file and can never be read by a rule.
    if name.is_empty() || name.starts_with("__") {
        return Vec::new();
    }
    // The file the label names, and the target it names; see the doc comment.
    // A source-file label is covered both ways at once, and harmlessly: its
    // `Label` pattern resolves to the file's own node, whose reverse row is
    // the rules that consume it.
    let path = if pkg.is_empty() {
        name.to_string()
    } else {
        format!("{pkg}/{name}")
    };
    vec![
        Pattern::Exact(path),
        Pattern::Label(format!("//{pkg}:{name}")),
    ]
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
            .entry(repo)
            .or_default()
            .insert(PathMap::exhaustive_dir(path.clone()));
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
    // repositories; any other falls back to the universe, and is told so.
    let mut wide: BTreeSet<String> = BTreeSet::new();
    let fallbacks: BTreeMap<String, BTreeSet<String>> = texts
        .iter()
        .map(|(p, _)| {
            let repos = match graph.id_of(p).and_then(|id| owner.get(&id)) {
                Some(r) if !r.is_empty() => r.clone(),
                _ => {
                    wide.insert(p.clone());
                    out.repos.clone()
                }
            };
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
    for (path, text) in &texts {
        let path = path.clone();
        let mut file_repos = att.file_repos(&path).repos;
        for (tok, l) in string_literals(text) {
            let pats = main_repo_patterns(&l.text, m);
            if pats.is_empty() {
                continue;
            }
            let a = att.at(&path, tok);
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
    for (path, text) in &texts {
        let maps = scan_bzl_path_maps(text, m);
        for (repo, pm) in maps.named {
            if out.repos.contains(&repo) {
                let pat = Pattern::Prefix(pm.from.clone());
                direct.insert(pat.clone());
                out.patterns.entry(pat).or_default().insert(repo.clone());
                own.entry(path.clone()).or_default().insert(repo.clone());
                out.path_maps.entry(repo).or_default().insert(pm);
            }
        }
        for (tok, pm) in &maps.symlinks {
            let a = att.at(path, *tok);
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
        // Empty is the answer for a file read in full whose only fallback
        // would be everything (issue #9); the wide answer is for the rest.
        if repos.is_empty() && !att.read_in_full(&path) {
            repos = match owner.get(&id) {
                Some(r) if !r.is_empty() => r.clone(),
                _ => out.repos.clone(),
            };
        }
        out.bzl_repos.insert(path, repos);
    }
    out.target_only = embedded.difference(&direct).cloned().collect();

    out
}

/// The repo-relative path of a `.bzl` label in the main repository.
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
/// Comments and nesting are handled by the shared lexer, so a `//` in a
/// comment is not a label. A `//` preceded by `:` or `/` is not either, which
/// is what keeps `https://github.com/envoyproxy/envoy` out.
/// Each literal comes back with the index of its token, which is what the
/// per-occurrence attribution is keyed on.
pub(crate) fn string_literals(text: &str) -> Vec<(usize, Literal)> {
    let mut out = Vec::new();
    let toks = lex(text);
    for (i, t) in toks.iter().enumerate() {
        let Tok::Str(s, e) = t.tok else {
            continue;
        };
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
        if i >= 2
            && matches!(toks[i - 1].tok, Tok::Open(b'('))
            && matches!(
                toks[i - 2].tok,
                Tok::Name("load") | Tok::Name("use_repo_rule") | Tok::Name("use_extension")
            )
        {
            continue;
        }
        let lit = unescape(&text[s..e]);
        for l in labels_in(&lit) {
            if l != lit {
                out.push((
                    i,
                    Literal {
                        text: l,
                        embedded: true,
                    },
                ));
            }
        }
        out.push((
            i,
            Literal {
                text: lit,
                embedded: false,
            },
        ));
    }
    out
}

/// One string literal of a Starlark file, or a label found inside one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Literal {
    pub(crate) text: String,
    /// `true` for a label-shaped substring of a larger literal: text the
    /// repository rule writes, not a label it resolves.
    pub(crate) embedded: bool,
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
