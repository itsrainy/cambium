//! Parsing `bazel dump --skyframe=deps` into a reverse-reachable graph.
//!
//! The dump is a flat text listing: a key at column zero, then its
//! dependencies under indented `Group N:` headings. Nothing about it is a
//! stable interface, so everything this module knows about the shapes is
//! written down here, with the Bazel 9.2.0 spelling beside it, and a fixture
//! test for each.
//!
//! ```text
//! CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:foo, config=BuildConfigurationKey[ab12…]}
//!   Group 1:
//!     PACKAGE:src/main
//!
//!   Group 2:
//!     CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:bar, config=BuildConfigurationKey[ab12…]}
//! ```
//!
//! The group numbering is Skyframe's own restart bookkeeping and carries no
//! meaning for a reverse walk, so the groups are flattened away: every
//! indented key is one dependency edge, and the graph stores it reversed.
//!
//! Key shapes on Bazel 9.2.0, all confirmed against the `bazel` subject:
//!
//! | kind | spelling |
//! | --- | --- |
//! | `CONFIGURED_TARGET` | `ConfiguredTargetKey{label=//pkg:name, config=BuildConfigurationKey[<64 hex>]}`, or `config=null` for a source file target |
//! | `ASPECT` | `SimpleAspectKey{baseConfiguredTargetKey=ConfiguredTargetKey{…}, aspectDescriptor=…}` |
//! | `TOOLCHAIN_RESOLUTION` | `ToolchainContextKey{configurationKey=BuildConfigurationKey[…], toolchainTypes=[…], …}` |
//! | `PLATFORM` | `Key[label=//:default_host_platform, flagAliasMappings={…}]` |
//! | `BUILD_CONFIGURATION` | `BuildConfigurationKey[<64 hex>]` |
//! | `PACKAGE`, `PACKAGE_LOOKUP` | `src/foo` in the main repository, `@@repo+//pkg` elsewhere |
//! | `BZL_LOAD` | `KeyForBuild{label=//pkg:file.bzl, isBuildPrelude=false}` |
//! | `FILE`, `FILE_STATE`, `DIRECTORY_LISTING`, `DIRECTORY_LISTING_STATE` | `[/abs/root]/[rel/path]` |
//! | `GLOBS` | `<GlobsKey packageRoot = /abs, packageIdentifier = pkg, globRequests = [...]>` |
//!
//! One thing learned the hard way and worth keeping in view: **nothing in the
//! analysis graph depends on a `FILE_STATE` directly.** The chain is
//! `PACKAGE`/`BZL_LOAD`/`GLOBS` -> `FILE` -> `FILE_STATE`, so a filter that
//! leaves `FILE` out makes every `FILE_STATE` a reverse-walk dead end. The
//! filter in [`super::SKYKEY_FILTER`] includes `FILE` for that reason.

use std::collections::BTreeMap;

/// The node kinds this tier keeps. Everything else the dump names is folded
/// into [`NodeKind::Other`], which still gets a node so an edge is never lost,
/// but is never a seed and never an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    ConfiguredTarget,
    Aspect,
    ToolchainResolution,
    SingleToolchainResolution,
    RegisteredToolchains,
    RegisteredExecutionPlatforms,
    Platform,
    BuildConfiguration,
    Package,
    PackageLookup,
    BzlLoad,
    File,
    FileState,
    DirectoryListing,
    DirectoryListingState,
    Globs,
    Other,
}

impl NodeKind {
    pub fn of(name: &str) -> Self {
        match name {
            "CONFIGURED_TARGET" => Self::ConfiguredTarget,
            "ASPECT" => Self::Aspect,
            "TOOLCHAIN_RESOLUTION" => Self::ToolchainResolution,
            "SINGLE_TOOLCHAIN_RESOLUTION" => Self::SingleToolchainResolution,
            "REGISTERED_TOOLCHAINS" => Self::RegisteredToolchains,
            "REGISTERED_EXECUTION_PLATFORMS" => Self::RegisteredExecutionPlatforms,
            "PLATFORM" => Self::Platform,
            "BUILD_CONFIGURATION" => Self::BuildConfiguration,
            "PACKAGE" => Self::Package,
            "PACKAGE_LOOKUP" => Self::PackageLookup,
            "BZL_LOAD" => Self::BzlLoad,
            "FILE" => Self::File,
            "FILE_STATE" => Self::FileState,
            "DIRECTORY_LISTING" => Self::DirectoryListing,
            "DIRECTORY_LISTING_STATE" => Self::DirectoryListingState,
            "GLOBS" => Self::Globs,
            _ => Self::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ConfiguredTarget => "CONFIGURED_TARGET",
            Self::Aspect => "ASPECT",
            Self::ToolchainResolution => "TOOLCHAIN_RESOLUTION",
            Self::SingleToolchainResolution => "SINGLE_TOOLCHAIN_RESOLUTION",
            Self::RegisteredToolchains => "REGISTERED_TOOLCHAINS",
            Self::RegisteredExecutionPlatforms => "REGISTERED_EXECUTION_PLATFORMS",
            Self::Platform => "PLATFORM",
            Self::BuildConfiguration => "BUILD_CONFIGURATION",
            Self::Package => "PACKAGE",
            Self::PackageLookup => "PACKAGE_LOOKUP",
            Self::BzlLoad => "BZL_LOAD",
            Self::File => "FILE",
            Self::FileState => "FILE_STATE",
            Self::DirectoryListing => "DIRECTORY_LISTING",
            Self::DirectoryListingState => "DIRECTORY_LISTING_STATE",
            Self::Globs => "GLOBS",
            Self::Other => "OTHER",
        }
    }
}

/// One dump key, reduced to the part a reverse walk can address.
///
/// The reduction is the whole point of the type: two keys that differ only in
/// text a seed could never reproduce (a `flagAliasMappings` blob on a
/// `PLATFORM`, a `globRequests` list) must still be one node, and a key a seed
/// *can* name (a file path, a label) must come out in the same spelling the
/// seeder uses.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey {
    pub kind: NodeKind,
    /// The addressable part: a label, a path, a package name, a checksum.
    pub id: String,
    /// The configuration checksum, for a `CONFIGURED_TARGET` that has one.
    /// `None` on a source file target (`config=null`) and on every other kind.
    pub config: Option<String>,
}

impl NodeKey {
    /// The string form the graph stores and the seeder builds.
    pub fn text(&self) -> String {
        match &self.config {
            Some(c) => format!("{}|{}|{c}", self.kind.name(), self.id),
            None => format!("{}|{}", self.kind.name(), self.id),
        }
    }

    pub fn configured_target(label: &str, config: Option<&str>) -> Self {
        Self {
            kind: NodeKind::ConfiguredTarget,
            id: label.to_string(),
            config: config.map(str::to_string),
        }
    }

    pub fn simple(kind: NodeKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            config: None,
        }
    }

    /// True for a node whose label is in the main repository: `//pkg:name`
    /// rather than `@@repo+//pkg:name`.
    pub fn is_main_repo_label(&self) -> bool {
        self.id.starts_with("//")
    }
}

/// Parses one dump line into a key, or `None` when the line names a kind
/// with no payload at all.
///
/// `line` is the whole key text, `KIND:payload`.
pub fn parse_key(line: &str) -> Option<NodeKey> {
    let (kind_name, payload) = line.split_once(':')?;
    let kind = NodeKind::of(kind_name);
    Some(match kind {
        NodeKind::ConfiguredTarget => {
            let (label, config) = configured_target(payload)?;
            NodeKey::configured_target(&label, config.as_deref())
        }
        // An aspect is addressed by the target it is applied to plus the
        // aspect itself: two aspects on one target are two nodes, and the
        // same aspect on two configurations of a target is two nodes.
        NodeKind::Aspect => {
            let base = between(payload, "baseConfiguredTargetKey=ConfiguredTargetKey{", "}")
                .and_then(|inner| configured_target(&format!("ConfiguredTargetKey{{{inner}}}")))
                .map(|(l, c)| format!("{l}@{}", c.unwrap_or_else(|| "null".into())))
                .unwrap_or_else(|| payload.to_string());
            let descriptor = after(payload, "aspectDescriptor=").unwrap_or_default();
            let descriptor = descriptor.trim_end_matches('}');
            NodeKey::simple(kind, format!("{descriptor}%{base}"))
        }
        NodeKind::BzlLoad => {
            let label = between(payload, "label=", ",")
                .or_else(|| between(payload, "label=", "}"))
                .unwrap_or_else(|| payload.to_string());
            NodeKey::simple(kind, label)
        }
        // The flagAliasMappings blob is repository configuration, identical on
        // every PLATFORM key in one dump; the label is the addressable part.
        NodeKind::Platform => {
            let label = between(payload, "label=", ",")
                .or_else(|| between(payload, "label=", "]"))
                .unwrap_or_else(|| payload.to_string());
            NodeKey::simple(kind, label)
        }
        NodeKind::BuildConfiguration => {
            NodeKey::simple(kind, between(payload, "[", "]").unwrap_or_else(|| payload.into()))
        }
        // `<GlobsKey packageRoot = /abs, packageIdentifier = pkg, globRequests = [...]>`.
        // Two GLOBS keys for one package differ only in the request list, and
        // a seeder can only ever name the package, so the package is the node.
        NodeKind::Globs => {
            let pkg = between(payload, "packageIdentifier = ", ",")
                .unwrap_or_else(|| payload.to_string());
            NodeKey::simple(kind, pkg.trim().to_string())
        }
        NodeKind::File
        | NodeKind::FileState
        | NodeKind::DirectoryListing
        | NodeKind::DirectoryListingState => {
            let (root, rel) = rooted_path(payload)?;
            NodeKey::simple(kind, format!("{root}\u{1}{rel}"))
        }
        _ => NodeKey::simple(kind, payload.to_string()),
    })
}

/// `ConfiguredTargetKey{label=…, config=BuildConfigurationKey[…]}` ->
/// `(label, Some(checksum))`, and `config=null` -> `(label, None)`.
fn configured_target(payload: &str) -> Option<(String, Option<String>)> {
    let label = between(payload, "label=", ",")?;
    let config = after(payload, "config=")?;
    let config = config.trim_end_matches('}');
    let config = if config.starts_with("null") {
        None
    } else {
        between(config, "[", "]").or_else(|| Some(config.to_string()))
    };
    Some((label, config))
}

/// `[/abs/root]/[rel/path]` -> `(root, rel)`. The root is `<absolute root>`
/// for a path outside any package root, which is left as it is: it is still a
/// stable name for that half of the key.
pub fn rooted_path(payload: &str) -> Option<(String, String)> {
    let rest = payload.strip_prefix('[')?;
    let close = rest.find("]/[")?;
    let root = &rest[..close];
    let rel = rest[close + 3..].strip_suffix(']')?;
    Some((root.to_string(), rel.to_string()))
}

fn between(hay: &str, start: &str, end: &str) -> Option<String> {
    let i = hay.find(start)? + start.len();
    let j = hay[i..].find(end)? + i;
    Some(hay[i..j].to_string())
}

fn after(hay: &str, start: &str) -> Option<String> {
    let i = hay.find(start)? + start.len();
    Some(hay[i..].to_string())
}

/// What one parsed dump amounts to.
#[derive(Debug, Default)]
pub struct Dump {
    /// Every node, in first-seen order.
    pub nodes: Vec<NodeKey>,
    /// `nodes[i]`'s dependents: the reverse of the dump's own direction.
    pub rdeps: Vec<Vec<u32>>,
    /// Key text -> index into `nodes`.
    pub index: BTreeMap<String, u32>,
    /// Forward edge count, before de-duplication.
    pub edges: usize,
    /// Lines the parser could not make a key of. Non-zero is worth reporting.
    pub unparsed: usize,
}

impl Dump {
    pub fn get(&self, key: &NodeKey) -> Option<u32> {
        self.index.get(&key.text()).copied()
    }

    fn intern(&mut self, key: NodeKey) -> u32 {
        let text = key.text();
        if let Some(&i) = self.index.get(&text) {
            return i;
        }
        let i = self.nodes.len() as u32;
        self.nodes.push(key);
        self.rdeps.push(Vec::new());
        self.index.insert(text, i);
        i
    }

    /// Every `CONFIGURED_TARGET` label carrying a configuration, and the set
    /// of checksums it appears under. This is the answer to "how many
    /// configurations is one target analysed in", which is the question a
    /// tier keyed on (label, config) has to answer before it can drop the
    /// config half.
    pub fn configs_by_label(&self) -> BTreeMap<&str, std::collections::BTreeSet<&str>> {
        let mut out: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
        for n in &self.nodes {
            if n.kind == NodeKind::ConfiguredTarget {
                if let Some(c) = &n.config {
                    out.entry(n.id.as_str()).or_default().insert(c.as_str());
                }
            }
        }
        out
    }
}

/// Reads a dump, one line at a time, into a reversed graph.
///
/// Memory is the reason this takes a reader rather than a `&str`: the `//...`
/// dump on the `bazel` subject is hundreds of megabytes of text and almost
/// all of it is repeated key spellings, which interning collapses.
pub fn parse<R: std::io::BufRead>(reader: R) -> std::io::Result<Dump> {
    let mut dump = Dump::default();
    let mut current: Option<u32> = None;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            match parse_key(trimmed) {
                Some(k) => current = Some(dump.intern(k)),
                None => {
                    dump.unparsed += 1;
                    current = None;
                }
            }
            continue;
        }
        // `Group 3:` is Skyframe's restart bookkeeping, not an edge.
        if trimmed.starts_with("Group ") && trimmed.ends_with(':') {
            continue;
        }
        let Some(parent) = current else {
            dump.unparsed += 1;
            continue;
        };
        match parse_key(trimmed) {
            Some(k) => {
                let child = dump.intern(k);
                dump.edges += 1;
                dump.rdeps[child as usize].push(parent);
            }
            None => dump.unparsed += 1,
        }
    }
    for r in &mut dump.rdeps {
        r.sort_unstable();
        r.dedup();
    }
    Ok(dump)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture cut from a real `bazel dump --skyframe=deps` on the `bazel`
    /// subject at 9.2.0, shortened but not reshaped.
    const FIXTURE: &str = "\
CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:foo, config=BuildConfigurationKey[f3f51bf1]}
  Group 1:
    PACKAGE:src/main

  Group 2:
    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:Foo.java, config=null}

    CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:bar, config=BuildConfigurationKey[f3f51bf1]}

  Group 3:
    TOOLCHAIN_RESOLUTION:ToolchainContextKey{configurationKey=BuildConfigurationKey[f3f51bf1], toolchainTypes=[ToolchainTypeRequirement[toolchainType=@@bazel_tools//tools/jdk:toolchain_type, mandatory=true, ignoreIfInvalid=false]], execConstraintLabels=[], forceExecutionPlatform=Optional.empty, debugTarget=false}

PACKAGE:src/main
  Group 1:
    FILE:[/w]/[src/main/BUILD]

    BZL_LOAD:KeyForBuild{label=//tools:defs.bzl, isBuildPrelude=false}

    GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = src/main, globRequests = [GlobRequest: ** FILES]>

FILE:[/w]/[src/main/BUILD]
  Group 1:
    FILE_STATE:[/w]/[src/main/BUILD]

TOOLCHAIN_RESOLUTION:ToolchainContextKey{configurationKey=BuildConfigurationKey[f3f51bf1], toolchainTypes=[], execConstraintLabels=[], forceExecutionPlatform=Optional.empty, debugTarget=false}
  Group 1:
    REGISTERED_TOOLCHAINS:RegisteredToolchainsValue.Key{configurationKey=BuildConfigurationKey[f3f51bf1]}

    PLATFORM:Key[label=//:default_host_platform, flagAliasMappings={python_path=@@rules_python+//python/config_settings:python_path}]
";

    fn parsed() -> Dump {
        parse(std::io::Cursor::new(FIXTURE)).unwrap()
    }

    #[test]
    fn a_configured_target_key_is_a_label_and_a_config_checksum() {
        let k = parse_key(
            "CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:foo, \
             config=BuildConfigurationKey[abc123]}",
        )
        .unwrap();
        assert_eq!(k.kind, NodeKind::ConfiguredTarget);
        assert_eq!(k.id, "//src/main:foo");
        assert_eq!(k.config.as_deref(), Some("abc123"));
        assert!(k.is_main_repo_label());
    }

    #[test]
    fn a_source_file_target_has_no_configuration() {
        let k = parse_key(
            "CONFIGURED_TARGET:ConfiguredTargetKey{label=//src/main:Foo.java, config=null}",
        )
        .unwrap();
        assert_eq!(k.config, None);
        assert_eq!(k.text(), "CONFIGURED_TARGET|//src/main:Foo.java");
    }

    #[test]
    fn an_external_configured_target_is_not_a_main_repo_label() {
        let k = parse_key(
            "CONFIGURED_TARGET:ConfiguredTargetKey{label=@@rules_go++go_sdk+t//:x, \
             config=BuildConfigurationKey[f33e]}",
        )
        .unwrap();
        assert!(!k.is_main_repo_label());
    }

    #[test]
    fn a_rooted_path_splits_into_root_and_relative_path() {
        assert_eq!(
            rooted_path("[/a/b]/[src/main/BUILD]").unwrap(),
            ("/a/b".to_string(), "src/main/BUILD".to_string())
        );
        // The root Bazel uses for a path under no package root.
        assert_eq!(
            rooted_path("[<absolute root>]/[/tmp/x]").unwrap(),
            ("<absolute root>".to_string(), "/tmp/x".to_string())
        );
    }

    #[test]
    fn a_globs_key_reduces_to_its_package() {
        let k = parse_key(
            "GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = src/main, \
             globRequests = [GlobRequest: ** FILES,GlobRequest: *.jar FILES]>",
        )
        .unwrap();
        assert_eq!(k.id, "src/main");
        // Two request lists, one node: a seeder can only ever name a package.
        let other = parse_key(
            "GLOBS:<GlobsKey packageRoot = /w, packageIdentifier = src/main, \
             globRequests = [GlobRequest: *.txt FILES]>",
        )
        .unwrap();
        assert_eq!(k, other);
    }

    #[test]
    fn a_platform_key_reduces_to_its_label() {
        let k = parse_key(
            "PLATFORM:Key[label=//:default_host_platform, flagAliasMappings={a=//x:y}]",
        )
        .unwrap();
        assert_eq!(k.id, "//:default_host_platform");
    }

    #[test]
    fn a_bzl_load_key_reduces_to_its_label() {
        let k =
            parse_key("BZL_LOAD:KeyForBuild{label=//tools:defs.bzl, isBuildPrelude=false}").unwrap();
        assert_eq!(k.id, "//tools:defs.bzl");
        let prelude =
            parse_key("BZL_LOAD:KeyForBuild{label=//tools:prelude_bazel, isBuildPrelude=true}")
                .unwrap();
        assert_eq!(prelude.id, "//tools:prelude_bazel");
    }

    #[test]
    fn an_aspect_key_carries_its_base_target_and_its_descriptor() {
        let k = parse_key(
            "ASPECT:SimpleAspectKey{baseConfiguredTargetKey=ConfiguredTargetKey{label=//a:b, \
             config=BuildConfigurationKey[f3f5]}, aspectDescriptor=//x:y.bzl%an_aspect}",
        )
        .unwrap();
        assert_eq!(k.kind, NodeKind::Aspect);
        assert_eq!(k.id, "//x:y.bzl%an_aspect%//a:b@f3f5");
    }

    #[test]
    fn a_toolchain_resolution_key_keeps_its_whole_payload() {
        // Two resolutions differing only in the requested toolchain types are
        // two nodes; nothing here reduces them, because nothing seeds them by
        // name either.
        let a = parse_key("TOOLCHAIN_RESOLUTION:ToolchainContextKey{configurationKey=K[1], toolchainTypes=[jdk]}").unwrap();
        let b = parse_key("TOOLCHAIN_RESOLUTION:ToolchainContextKey{configurationKey=K[1], toolchainTypes=[cpp]}").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn groups_are_flattened_into_plain_edges() {
        let d = parsed();
        let foo = d
            .get(&NodeKey::configured_target("//src/main:foo", Some("f3f51bf1")))
            .unwrap();
        // The source file target's one dependent is the rule.
        let src = d
            .get(&NodeKey::configured_target("//src/main:Foo.java", None))
            .unwrap();
        assert_eq!(d.rdeps[src as usize], vec![foo]);
        // And so is the package's, through the same flattening.
        let pkg = d.get(&NodeKey::simple(NodeKind::Package, "src/main")).unwrap();
        assert_eq!(d.rdeps[pkg as usize], vec![foo]);
        assert_eq!(d.unparsed, 0);
    }

    #[test]
    fn a_file_state_is_reached_only_through_its_file() {
        let d = parsed();
        let state = d
            .get(&NodeKey::simple(NodeKind::FileState, "/w\u{1}src/main/BUILD"))
            .unwrap();
        let file = d
            .get(&NodeKey::simple(NodeKind::File, "/w\u{1}src/main/BUILD"))
            .unwrap();
        // FILE_STATE -> FILE -> PACKAGE, and no shortcut: leaving FILE out of
        // the dump filter would strand the file state entirely.
        assert_eq!(d.rdeps[state as usize], vec![file]);
        assert!(!d.rdeps[file as usize].is_empty());
    }

    #[test]
    fn a_repeated_key_is_one_node() {
        let d = parsed();
        // `PACKAGE:src/main` appears once as a dependency and once as a
        // header.
        let pkg = d.get(&NodeKey::simple(NodeKind::Package, "src/main")).unwrap();
        assert_eq!(
            d.nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Package && n.id == "src/main")
                .count(),
            1
        );
        assert!(!d.rdeps[pkg as usize].is_empty());
    }

    #[test]
    fn config_checksums_are_counted_per_label() {
        let d = parsed();
        let by_label = d.configs_by_label();
        assert_eq!(by_label["//src/main:foo"].len(), 1);
        // A source file target has no configuration and is not counted.
        assert!(!by_label.contains_key("//src/main:Foo.java"));
    }
}
