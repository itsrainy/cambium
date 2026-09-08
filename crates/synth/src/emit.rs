//! Emitting a synthetic graph as a real Bazel workspace (spec 13.4).
//!
//! The rules do no real work — each action just writes a file listing its
//! inputs — so `bazel build //...` succeeds on any machine with no compilers or
//! toolchains. That makes the same graph usable three ways: differential
//! correctness against `bazel query`, end-to-end timing that includes real
//! Bazel loading cost, and a like-for-like comparison with the "dump the whole
//! graph twice" approach.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::graph::{Kind, SynthGraph, EXT_LIBS};

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `out` already holds something that is not a synthetic workspace.
    #[error(
        "{path} is not empty and was not written by `cambi synth emit-bazel`; \
         refusing to delete its contents (pass --force to override)"
    )]
    NotSynthetic { path: PathBuf },
    #[error("git {args}: {stderr}")]
    Git { args: String, stderr: String },
}

type Result<T> = std::result::Result<T, EmitError>;

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|source| EmitError::Io {
            path: p.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| EmitError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes the whole workspace, replacing whatever was there (except `.git`).
///
/// `out` must be empty, missing, or a workspace this emitter wrote earlier —
/// recognised by its `MODULE.bazel` naming the `synth` module. Anything else is
/// refused with [`EmitError::NotSynthetic`]; a caller that really means it can
/// [`force_clear`] first.
pub fn emit_workspace(g: &SynthGraph, out: &Path) -> Result<()> {
    if !is_synthetic_workspace(out)? {
        return Err(EmitError::NotSynthetic {
            path: out.to_path_buf(),
        });
    }
    clear(out)?;
    write(&out.join("MODULE.bazel"), &module_bazel())?;
    write(&out.join(".bazelversion"), "7.4.1\n")?;
    write(&out.join("rules/BUILD.bazel"), "")?;
    write(&out.join("rules/synth.bzl"), RULES_BZL)?;
    write(&out.join("tools/synth/BUILD.bazel"), TOOLS_BUILD)?;
    for (i, b) in g.bzl.iter().enumerate() {
        write(&out.join(&b.path), &generated_bzl(i as u32, b.version))?;
    }
    emit_external(out)?;
    write(&out.join("cambium.toml"), CAMBIUM_TOML)?;

    for pkg in 0..g.packages.len() as u32 {
        if !g.packages[pkg as usize].alive {
            continue;
        }
        emit_package(g, pkg, out)?;
    }
    Ok(())
}

/// The line `module_bazel` starts with, which is how a later run knows the
/// directory is one of ours.
const MODULE_MARKER: &str = "module(name = \"synth\"";

/// Whether `out` is missing, empty apart from `.git`, or carries the emitter's
/// marker.
fn is_synthetic_workspace(out: &Path) -> Result<bool> {
    if !out.exists() {
        return Ok(true);
    }
    let io = |source| EmitError::Io {
        path: out.to_path_buf(),
        source,
    };
    let mut entries = std::fs::read_dir(out).map_err(io)?.flatten();
    if entries.all(|e| e.file_name() == ".git") {
        return Ok(true);
    }
    match std::fs::read_to_string(out.join("MODULE.bazel")) {
        Ok(text) => Ok(text.starts_with(MODULE_MARKER)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io(e)),
    }
}

/// Deletes everything under `out` except `.git`, whatever wrote it.
///
/// This is the `--force` path; [`emit_workspace`] on its own only clears a
/// directory it recognises as its own.
pub fn force_clear(out: &Path) -> Result<()> {
    clear(out)
}

fn clear(out: &Path) -> Result<()> {
    if !out.exists() {
        return std::fs::create_dir_all(out).map_err(|source| EmitError::Io {
            path: out.to_path_buf(),
            source,
        });
    }
    for e in std::fs::read_dir(out)
        .map_err(|source| EmitError::Io {
            path: out.to_path_buf(),
            source,
        })?
        .flatten()
    {
        if e.file_name() == ".git" {
            continue;
        }
        let p = e.path();
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        r.map_err(|source| EmitError::Io { path: p, source })?;
    }
    Ok(())
}

/// The `load()` prologue of a package's BUILD file.
///
/// Shared by the emitter and by [`SynthFiles`], which is what makes the
/// property tests exercise the real `load()` parser: the text the engine scans
/// in memory is the text `emit-bazel` would have written to disk.
pub fn build_file_loads(g: &SynthGraph, pkg: u32) -> String {
    let p = &g.packages[pkg as usize];
    let mut build = String::new();
    // Each generated .bzl exposes macros suffixed with its index, so a package
    // that loads several has no name clashes.
    for &b in &p.bzl {
        build.push_str(&format!(
            "load(\"//tools/synth:rules{b}.bzl\", \"synth_lib{b}\", \"synth_test{b}\", \"synth_bin{b}\")\n"
        ));
    }
    if p.bzl.is_empty() {
        build.push_str(
            "load(\"//rules:synth.bzl\", \"synth_lib\", \"synth_test\", \"synth_bin\")\n",
        );
    }
    build.push('\n');
    build
}

/// Every file of the synthetic workspace, served from memory.
///
/// The engine's static `.bzl` closure scan reads Starlark text through this
/// rather than from a checkout, so the soundness sweep runs the real parser
/// over the real load lines at graph sizes no emitted workspace could reach.
pub struct SynthFiles<'a>(pub &'a SynthGraph);

impl cambium_index::FileText for SynthFiles<'_> {
    fn text(&self, path: &str) -> Option<String> {
        let g = self.0;
        if path == "MODULE.bazel" {
            return Some(module_bazel());
        }
        if path == "rules/synth.bzl" {
            return Some(RULES_BZL.to_string());
        }
        if let Some(&b) = g.bzl_by_path.get(path) {
            return Some(generated_bzl(b, g.bzl[b as usize].version));
        }
        let dir = path.strip_suffix("/BUILD.bazel").or_else(|| {
            // The root package, and the `BUILD` spelling, neither of which the
            // emitter writes.
            (path == "BUILD.bazel").then_some("")
        })?;
        let &pkg = g.pkg_by_path.get(dir)?;
        g.packages[pkg as usize]
            .alive
            .then(|| build_file_loads(g, pkg))
    }
}

fn emit_package(g: &SynthGraph, pkg: u32, out: &Path) -> Result<()> {
    let p = &g.packages[pkg as usize];
    let dir = out.join(&p.path);
    let mut build = build_file_loads(g, pkg);

    if !p.exported.is_empty() {
        let files: Vec<String> = p
            .exported
            .iter()
            .map(|&(t, s)| format!("\"{}\"", SynthGraph::src_name(t, s)))
            .collect();
        build.push_str(&format!("exports_files([{}])\n\n", files.join(", ")));
    }

    let suffix = p.bzl.first().map(|b| b.to_string()).unwrap_or_default();
    for &tid in &p.targets {
        let t = &g.targets[tid as usize];
        if !t.alive {
            continue;
        }
        // Source files: written empty, picked up by a per-target glob so that
        // adding a file really does change the target's inputs.
        for &slot in &t.srcs {
            let name = SynthGraph::src_name(tid, slot);
            let mut f = std::fs::File::create(dir.join(&name))
                .or_else(|_| {
                    std::fs::create_dir_all(&dir)
                        .and_then(|_| std::fs::File::create(dir.join(&name)))
                })
                .map_err(|source| EmitError::Io {
                    path: dir.join(&name),
                    source,
                })?;
            // Content version goes in the file so that a ModifySource commit
            // produces a real diff.
            let version = t
                .srcs
                .iter()
                .position(|&s| s == slot)
                .map(|i| t.src_versions[i])
                .unwrap_or(0);
            writeln!(f, "// v{version}").map_err(|source| EmitError::Io {
                path: dir.join(&name),
                source,
            })?;
        }

        let name = format!("t{tid}");
        let (macro_name, is_suite) = match t.kind {
            Kind::TestSuite => ("test_suite".to_string(), true),
            Kind::Alias => ("alias".to_string(), false),
            k => (format!("{}{}", k.rule_class(), suffix), false),
        };

        if is_suite {
            let tests: Vec<String> = t
                .deps
                .iter()
                .filter(|&&d| g.targets[d as usize].alive)
                .map(|&d| format!("\"{}\"", g.target_label(d)))
                .collect();
            build.push_str(&format!(
                "test_suite(\n    name = \"{name}\",\n    tests = [{}],\n)\n\n",
                tests.join(", ")
            ));
            continue;
        }
        if t.kind == Kind::Alias {
            match t.deps.iter().find(|&&d| g.targets[d as usize].alive) {
                Some(&d) => {
                    build.push_str(&format!(
                        "alias(\n    name = \"{name}\",\n    actual = \"{}\",\n    visibility = [\"//visibility:public\"],\n)\n\n",
                        g.target_label(d)
                    ));
                }
                // An alias whose target was deleted still has to exist, or the
                // emitted repo would disagree with the in-memory graph about
                // which labels are present.
                None => build.push_str(&format!(
                    "synth_lib{suffix}(\n    name = \"{name}\",\n    salt = \"{}\",\n    visibility = [\"//visibility:public\"],\n)\n\n",
                    t.attr_salt
                )),
            }
            continue;
        }

        let live_deps: Vec<u32> = t
            .deps
            .iter()
            .copied()
            .filter(|&d| g.targets[d as usize].alive)
            .collect();
        let split = live_deps.len().saturating_sub(t.select_deps as usize);
        let plain: Vec<String> = live_deps[..split]
            .iter()
            .map(|&d| format!("\"{}\"", g.target_label(d)))
            .collect();
        let selected: Vec<String> = live_deps[split..]
            .iter()
            .map(|&d| format!("\"{}\"", g.target_label(d)))
            .collect();
        let mut ext: Vec<String> = (0..t.ext_deps)
            .map(|k| format!("\"{}\"", SynthGraph::ext_label(tid, k)))
            .collect();
        let mut dep_expr = format!(
            "[{}]",
            plain
                .into_iter()
                .chain(ext.drain(..))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !selected.is_empty() {
            // The union over select branches is what --proto:flatten_selects
            // reports, and that union is the over-approximation we want.
            dep_expr.push_str(&format!(
                " + select({{\n        \"//tools/synth:opt\": [{}],\n        \"//conditions:default\": [{}],\n    }})",
                selected.join(", "),
                selected.join(", ")
            ));
        }

        let mut srcs = format!("glob([\"t{tid}_*.src\"], allow_empty = True)");
        if !t.foreign_srcs.is_empty() {
            let f: Vec<String> = t
                .foreign_srcs
                .iter()
                .map(|&(_, ft, fs)| format!("\"{}\"", g.src_label(ft, fs)))
                .collect();
            srcs.push_str(&format!(" + [{}]", f.join(", ")));
        }

        let tags = if t.is_manual() { "[\"manual\"]" } else { "[]" };
        build.push_str(&format!(
            "{macro_name}(\n    name = \"{name}\",\n    srcs = {srcs},\n    deps = {dep_expr},\n    \
             salt = \"{}\",\n    tags = {tags},\n    visibility = [\"//visibility:public\"],\n)\n\n",
            t.attr_salt
        ));
    }

    write(&dir.join("BUILD.bazel"), &build)
}

fn module_bazel() -> String {
    "module(name = \"synth\", version = \"0.0.1\")\n\
     bazel_dep(name = \"synth_ext\", version = \"0.0.1\")\n\
     local_path_override(\n    module_name = \"synth_ext\",\n    path = \"external/synth_ext\",\n)\n"
        .to_string()
}

fn emit_external(out: &Path) -> Result<()> {
    let root = out.join("external/synth_ext");
    write(
        &root.join("MODULE.bazel"),
        "module(name = \"synth_ext\", version = \"0.0.1\")\n",
    )?;
    write(&root.join("BUILD.bazel"), "")?;
    let mut build = String::from("load(\"@synth_ext//:rules.bzl\", \"ext_lib\")\n\n");
    for i in 0..EXT_LIBS {
        build.push_str(&format!(
            "ext_lib(\n    name = \"lib{i}\",\n    visibility = [\"//visibility:public\"],\n)\n\n"
        ));
    }
    write(&root.join("ext/BUILD.bazel"), &build)?;
    write(
        &root.join("rules.bzl"),
        "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + \".out\")\n    \
         ctx.actions.write(out, ctx.label.name)\n    return [DefaultInfo(files = depset([out]))]\n\n\
         ext_lib = rule(implementation = _impl)\n",
    )
}

fn generated_bzl(index: u32, version: u32) -> String {
    format!(
        "\"\"\"Generated rule wrappers, revision {version}.\"\"\"\n\n\
         load(\"//rules:synth.bzl\", \"synth_bin\", \"synth_lib\", \"synth_test\")\n\n\
         VERSION = \"{version}\"\n\n\
         def synth_lib{index}(**kwargs):\n    synth_lib(**kwargs)\n\n\
         def synth_test{index}(**kwargs):\n    synth_test(**kwargs)\n\n\
         def synth_bin{index}(**kwargs):\n    synth_bin(**kwargs)\n"
    )
}

const TOOLS_BUILD: &str = "config_setting(\n    name = \"opt\",\n    values = {\"compilation_mode\": \"opt\"},\n    visibility = [\"//visibility:public\"],\n)\n";

const CAMBIUM_TOML: &str = "# Generated by `cambi synth emit-bazel`.\n\
[invalidators]\n\
paths = [\"MODULE.bazel\", \"MODULE.bazel.lock\", \".bazelversion\", \"BUILD\", \"BUILD.bazel\"]\n\
globs = [\"rules/**\", \"external/**\"]\n\n\
[ignore]\n\
globs = [\"**/*.md\"]\n";

const RULES_BZL: &str = r##""""Rules that do no real work, so `bazel build //...` runs anywhere."""

def _synth_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    lines = [f.path for f in ctx.files.srcs] + [str(d.label) for d in ctx.attr.deps]
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

_ATTRS = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "salt": attr.string(),
}

synth_lib = rule(implementation = _synth_impl, attrs = _ATTRS)

def _synth_bin_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(out, "#!/bin/sh\nexit 0\n", is_executable = True)
    return [DefaultInfo(executable = out, files = depset([out]))]

synth_bin = rule(implementation = _synth_bin_impl, attrs = _ATTRS, executable = True)

synth_test = rule(implementation = _synth_bin_impl, attrs = _ATTRS, test = True)
"##;

// ------------------------------------------------------------------- history

/// A workspace with one git commit per synthetic revision.
pub struct EmittedRepo {
    pub dir: PathBuf,
    /// Commit shas, oldest first.
    pub commits: Vec<String>,
    /// The graph after the last commit.
    pub graph: SynthGraph,
    /// The commit applied at each step, aligned with `commits[1..]`.
    pub steps: Vec<crate::mutate::Commit>,
    /// Graph snapshot per commit, when the caller asked for them. Aligned with
    /// `commits`, so `graphs[i]` is the graph at `commits[i]`.
    pub graphs: Vec<SynthGraph>,
}

/// Writes the workspace and a git history of `revisions` commits, so
/// `git diff --name-status` works on synthetic data and the end-to-end path can
/// be measured against a real Bazel.
pub fn emit_git_history(
    mut graph: SynthGraph,
    change_spec: &crate::mutate::ChangeSpec,
    revisions: u64,
    out: &Path,
    keep_graphs: bool,
) -> Result<EmittedRepo> {
    emit_workspace(&graph, out)?;
    git(out, &["init", "-q", "-b", "main"])?;
    git(out, &["config", "user.email", "synth@example.com"])?;
    git(out, &["config", "user.name", "cambi synth"])?;
    let mut commits = vec![commit_all(out, "synthetic base")?];
    let mut steps = Vec::new();
    let mut graphs = Vec::new();
    if keep_graphs {
        graphs.push(graph.clone());
    }

    for rev in 0..revisions {
        let c = crate::mutate::commit(&mut graph, change_spec, rev);
        if c.changes.is_empty() {
            continue;
        }
        emit_workspace(&graph, out)?;
        commits.push(commit_all(out, &format!("synthetic revision {rev}"))?);
        steps.push(c);
        if keep_graphs {
            graphs.push(graph.clone());
        }
    }

    Ok(EmittedRepo {
        dir: out.to_path_buf(),
        commits,
        graph,
        steps,
        graphs,
    })
}

fn commit_all(dir: &Path, message: &str) -> Result<String> {
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "-q", "--allow-empty", "-m", message])?;
    Ok(git(dir, &["rev-parse", "HEAD"])?.trim().to_string())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|source| EmitError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    if !out.status.success() {
        return Err(EmitError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::GraphSpec;

    #[test]
    fn emits_a_plausible_workspace() {
        let g = crate::generate(&GraphSpec::preset("tiny").unwrap());
        let dir = tempfile::tempdir().unwrap();
        emit_workspace(&g, dir.path()).unwrap();

        assert!(dir.path().join("MODULE.bazel").exists());
        assert!(dir.path().join("rules/synth.bzl").exists());
        assert!(dir
            .path()
            .join("external/synth_ext/ext/BUILD.bazel")
            .exists());

        let pkg = g.package_path(0);
        let build = std::fs::read_to_string(dir.path().join(pkg).join("BUILD.bazel")).unwrap();
        assert!(build.contains("name = \"t0\""), "{build}");
        assert!(build.contains("glob(["), "{build}");
        for &slot in &g.targets[0].srcs {
            assert!(dir.path().join(g.src_path(0, slot)).exists());
        }
    }

    #[test]
    fn refuses_to_clear_a_directory_it_did_not_write() {
        let g = crate::generate(&GraphSpec::preset("tiny").unwrap());
        let dir = tempfile::tempdir().unwrap();
        let precious = dir.path().join("notes.txt");
        std::fs::write(&precious, "mine").unwrap();
        let err = emit_workspace(&g, dir.path()).unwrap_err();
        assert!(matches!(err, EmitError::NotSynthetic { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&precious).unwrap(), "mine");

        // A MODULE.bazel that is not ours is not a marker either.
        std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"real\")\n").unwrap();
        assert!(emit_workspace(&g, dir.path()).is_err());
        assert!(precious.exists());

        // --force is the only way through, and it takes everything.
        force_clear(dir.path()).unwrap();
        emit_workspace(&g, dir.path()).unwrap();
        assert!(!precious.exists());
        assert!(dir.path().join("MODULE.bazel").exists());
    }

    #[test]
    fn an_empty_or_git_only_directory_is_fine() {
        let g = crate::generate(&GraphSpec::preset("tiny").unwrap());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        emit_workspace(&g, dir.path()).unwrap();
        assert!(dir.path().join(".git/HEAD").exists());
        // And a second emit sees the marker and proceeds.
        emit_workspace(&g, dir.path()).unwrap();
    }

    #[test]
    fn the_marker_is_what_module_bazel_starts_with() {
        assert!(module_bazel().starts_with(MODULE_MARKER));
    }

    #[test]
    fn re_emitting_removes_stale_files() {
        let mut g = crate::generate(&GraphSpec::preset("tiny").unwrap());
        let dir = tempfile::tempdir().unwrap();
        emit_workspace(&g, dir.path()).unwrap();
        let t = (0..g.targets.len() as u32)
            .find(|&i| !g.targets[i as usize].srcs.is_empty())
            .unwrap();
        let slot = g.targets[t as usize].srcs[0];
        let path = dir.path().join(g.src_path(t, slot));
        assert!(path.exists());
        crate::apply_one(&mut g, &crate::Mutation::DeleteSource { target: t, slot });
        emit_workspace(&g, dir.path()).unwrap();
        assert!(!path.exists());
    }
}
