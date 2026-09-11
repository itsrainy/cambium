//! Which world Bazel reads in a tree that carries both: the startup flags in
//! `.bazelrc`, and the fallbacks when it says nothing.

use crate::loads::{FileText, RepoMapping};

/// Whether Bazel reads `MODULE.bazel` in this tree.
///
/// A repository mid-migration carries both worlds, and only the startup
/// flags say which one is live. envoy at `8424aa0d` had `common
/// --noenable_bzlmod` and `--enable_workspace` in `.bazelrc`, a
/// `MODULE.bazel` whose `local_path_override` put `envoy_build_config` at
/// `mobile/envoy_build_config`, and a WORKSPACE that built the repository of
/// the same name from a repository rule reading
/// `source/extensions/extensions_build_config.bzl`. Reading the module file
/// there minted an exhaustive path map for a repository Bazel never built
/// that way, and the map then said the real input could not be a `.bzl` the
/// repository exports: 1097 packages missed, 8 dirtied.
///
/// The bazelrc's own word wins. Without one, a tree with no WORKSPACE is
/// bzlmod, and a tree with one is bzlmod only when the repository mapping
/// was produced by `bazel mod dump_repo_mapping`, a command WORKSPACE mode
/// cannot run. Only the workspace `.bazelrc` is read: a `try-import` or a
/// `--bazelrc` startup flag can set the same flag, and a run under
/// `--noworkspace_rc` ignores the file altogether; those fall back to the
/// mapping.
pub fn bzlmod_live(src: &dyn FileText, m: &RepoMapping) -> bool {
    if let Some(explicit) = src.text(".bazelrc").as_deref().and_then(bazelrc_bzlmod) {
        return explicit;
    }
    let has_workspace = ["WORKSPACE", "WORKSPACE.bazel"]
        .iter()
        .any(|f| src.text(f).is_some());
    !has_workspace || !m.is_identity()
}

/// The last `--enable_bzlmod` / `--noenable_bzlmod` a bazelrc states, on
/// any command line of it; `None` when it says nothing.
pub fn bazelrc_bzlmod(text: &str) -> Option<bool> {
    let mut out = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        // Only an unconditional command line counts. `common:workspace
        // --noenable_bzlmod` applies under `--config=workspace` and nowhere
        // else; xla's bazelrc has exactly that after its `common
        // --enable_bzlmod`, and reading it turned the pin into WORKSPACE mode
        // with no module facts at all. An `import` line is not a command.
        let Some(command) = line.split_whitespace().next() else {
            continue;
        };
        if command.contains(':') || command == "import" || command == "try-import" {
            continue;
        }
        for tok in line.split_whitespace().skip(1) {
            match tok {
                "--enable_bzlmod" | "--enable_bzlmod=true" | "--enable_bzlmod=yes" => {
                    out = Some(true)
                }
                "--noenable_bzlmod" | "--enable_bzlmod=false" | "--enable_bzlmod=no" => {
                    out = Some(false)
                }
                _ => {}
            }
        }
    }
    out
}
