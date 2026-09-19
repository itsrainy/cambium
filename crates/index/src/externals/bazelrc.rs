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
    Mode::detect(src, m).bzlmod_live()
}

/// Which of Bazel's two external-dependency worlds a tree is read in.
///
/// Decided once per run by [`Mode::detect`] — the rule [`bzlmod_live`]
/// documents — and passed down to whatever needs it, rather than re-derived
/// from the presence of a `WORKSPACE` and the shape of the mapping at each
/// step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// bzlmod is live and there is no `WORKSPACE`: `MODULE.bazel` and its
    /// closure declare every repository.
    Bzlmod,
    /// bzlmod is off: `MODULE.bazel` is inert whether or not it exists, and
    /// the `WORKSPACE` closure is the whole story.
    Workspace,
    /// Both, as Bazel 7 reads them under `--enable_workspace`: bzlmod is
    /// live and a `WORKSPACE` exists too, so both closures declare
    /// repositories and both are read.
    Both,
}

impl Mode {
    /// Reads the mode off a checkout; see [`bzlmod_live`] for the rule.
    pub fn detect(src: &dyn FileText, m: &RepoMapping) -> Mode {
        let has_workspace = ["WORKSPACE", "WORKSPACE.bazel"]
            .iter()
            .any(|f| src.text(f).is_some());
        let bzlmod = match src.text(".bazelrc").as_deref().and_then(bazelrc_bzlmod) {
            Some(explicit) => explicit,
            None => !has_workspace || !m.is_identity(),
        };
        match (bzlmod, has_workspace) {
            (false, _) => Mode::Workspace,
            (true, false) => Mode::Bzlmod,
            (true, true) => Mode::Both,
        }
    }

    /// Whether Bazel reads `MODULE.bazel` in this mode.
    pub fn bzlmod_live(self) -> bool {
        self != Mode::Workspace
    }
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

/// The value of the last `--incompatible_autoload_externally` a bazelrc
/// states on an unconditional command line, as [`bazelrc_bzlmod`] reads its
/// flag; `None` when it says nothing, and the Bazel's own default stands.
///
/// Both spellings are read: `--flag=value` (an empty value is
/// `--incompatible_autoload_externally=`, the documented way to turn autoload
/// off), and `--flag value`, where the next token is the value as it is for
/// Bazel's own parser, even when it starts with `-`: a value removing symbols
/// (`-py_binary`) is a value, not another flag.
pub fn bazelrc_autoload_externally(text: &str) -> Option<String> {
    const FLAG: &str = "--incompatible_autoload_externally";
    let mut out = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(command) = tokens.next() else {
            continue;
        };
        if command.contains(':') || command == "import" || command == "try-import" {
            continue;
        }
        let tokens: Vec<&str> = tokens.collect();
        let mut i = 0;
        while i < tokens.len() {
            let tok = tokens[i];
            i += 1;
            if let Some(value) = tok.strip_prefix(FLAG) {
                if let Some(value) = value.strip_prefix('=') {
                    out = Some(value.trim_matches(['"', '\'']).to_string());
                } else if value.is_empty() {
                    let next = tokens.get(i).copied();
                    if next.is_some() {
                        i += 1;
                    }
                    out = Some(
                        next.map(|t| t.trim_matches(['"', '\'']))
                            .unwrap_or("")
                            .to_string(),
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_autoload_flag_is_read_off_unconditional_command_lines_only() {
        assert_eq!(bazelrc_autoload_externally("build --jobs=8\n"), None);
        assert_eq!(
            bazelrc_autoload_externally("common --incompatible_autoload_externally=\n").as_deref(),
            Some("")
        );
        assert_eq!(
            bazelrc_autoload_externally(
                "common --incompatible_autoload_externally=+@rules_python,@com_google_protobuf\n"
            )
            .as_deref(),
            Some("+@rules_python,@com_google_protobuf")
        );
        // The last unconditional line wins; a `--config` line and a comment
        // say nothing.
        assert_eq!(
            bazelrc_autoload_externally(
                "build --incompatible_autoload_externally=+@rules_python\n\
                 # build --incompatible_autoload_externally=@x\n\
                 build:migrate --incompatible_autoload_externally=@y\n\
                 common --incompatible_autoload_externally \"\"\n"
            )
            .as_deref(),
            Some("")
        );
        assert_eq!(
            bazelrc_autoload_externally("build --incompatible_autoload_externally +@rules_shell\n")
                .as_deref(),
            Some("+@rules_shell")
        );
        // In the separated spelling the next token is the value even when it
        // starts with `-`: Bazel's parser takes it as one, and a value that
        // only removes symbols is what turns autoload off.
        assert_eq!(
            bazelrc_autoload_externally(
                "build --incompatible_autoload_externally -py_binary,+@rules_shell\n"
            )
            .as_deref(),
            Some("-py_binary,+@rules_shell")
        );
        // A different flag with the same prefix is not this one.
        assert_eq!(
            bazelrc_autoload_externally("build --incompatible_autoload_externally_extra=1\n"),
            None
        );
    }
}
