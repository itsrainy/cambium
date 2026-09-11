//! `cambi index` — queries against a built index.
//!
//! Every subcommand reads one index file and answers one question, in text for
//! a person or JSON for a program. The graph work is
//! `cambium_engine::inspect`, which is the same walk the `affected` run uses;
//! a query that disagreed with a run would be worse than no query at all.

use std::path::PathBuf;

use anyhow::Result;
use cambium_engine::inspect::{self, IndexQuery, QueryOptions};
use cambium_index::IndexFile;
use clap::{Args as ClapArgs, Subcommand};
use serde::Serialize;

use crate::cmd::Context;
use crate::DataFormat;

/// Options every query shares.
#[derive(ClapArgs, Clone)]
pub struct Common {
    /// Index file to query.
    pub file: PathBuf,
}

/// Options the reachability queries share.
#[derive(ClapArgs, Clone)]
pub struct Reach {
    /// Follow edges transitively rather than one hop.
    #[arg(long)]
    pub transitive: bool,
    /// Include targets tagged `manual`.
    #[arg(long)]
    pub include_manual: bool,
    /// Report only test targets.
    #[arg(long)]
    pub tests_only: bool,
    /// With --transitive, stop after this many hops.
    #[arg(long)]
    pub max_depth: Option<u32>,
}

impl From<&Reach> for QueryOptions {
    fn from(r: &Reach) -> Self {
        QueryOptions {
            transitive: r.transitive,
            include_manual: r.include_manual,
            tests_only: r.tests_only,
            max_depth: r.max_depth,
        }
    }
}

/// `index verify`, dispatched separately from the other subcommands: it does
/// not take a [`Common`] (it shells out to Bazel rather than answering from
/// the index alone), so it never shares `IndexQuery::common()`.
#[derive(ClapArgs)]
pub struct Verify {
    file: PathBuf,
    /// How many targets to sample.
    #[arg(long, default_value_t = 200)]
    sample: usize,
    /// Seed for the sample, so a failure can be reproduced.
    #[arg(long, default_value_t = 0)]
    seed: u64,
}

#[derive(Subcommand)]
pub enum Args {
    /// Check an index against Bazel (spec 12.3): sample rule targets and
    /// compare our reverse closure with `bazel query rdeps(//..., label)`.
    Verify(Verify),
    /// Print an index's header and counts, and optionally one target or package.
    Inspect {
        #[command(flatten)]
        common: Common,
        /// Show one target's row: kind, package, hash, deps and rdeps.
        #[arg(long)]
        label: Option<String>,
        /// Show one package's targets.
        #[arg(long)]
        package: Option<String>,
    },
    /// Targets that depend on a label.
    Rdeps {
        #[command(flatten)]
        common: Common,
        label: String,
        #[command(flatten)]
        reach: Reach,
    },
    /// Targets a label depends on.
    Deps {
        #[command(flatten)]
        common: Common,
        label: String,
        #[command(flatten)]
        reach: Reach,
    },
    /// The shortest dependency path between two targets.
    Path {
        #[command(flatten)]
        common: Common,
        from: String,
        to: String,
    },
    /// The rules that consume each repo-relative file path.
    Consumers {
        #[command(flatten)]
        common: Common,
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        reach: Reach,
    },
    /// Which package owns each repo-relative path.
    Owner {
        #[command(flatten)]
        common: Common,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Everything a package owns.
    Targets {
        #[command(flatten)]
        common: Common,
        package: String,
        /// Filter by node kind: rule, source_file, generated_file,
        /// package_group, environment_group, unknown, tombstone.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Byte accounting, counts, and in-degree distribution.
    Stats {
        #[command(flatten)]
        common: Common,
    },
    /// The externals tables: every repository the main repository defines,
    /// its canonical name, path mappings and input patterns; every workspace
    /// `.bzl` and the repositories it answers for; every external label a
    /// load walk stopped at. What decides whether an input edit re-queries
    /// a repository's loaders, and the first thing to diff when a run and a
    /// replay disagree.
    Externals {
        #[command(flatten)]
        common: Common,
        /// Only this repository (apparent name).
        #[arg(long)]
        repo: Option<String>,
    },
}

impl Args {
    fn common(&self) -> &Common {
        match self {
            Args::Inspect { common, .. }
            | Args::Rdeps { common, .. }
            | Args::Deps { common, .. }
            | Args::Path { common, .. }
            | Args::Consumers { common, .. }
            | Args::Owner { common, .. }
            | Args::Targets { common, .. }
            | Args::Stats { common }
            | Args::Externals { common, .. } => common,
            Args::Verify(_) => unreachable!("index::run intercepts Verify before common()"),
        }
    }
}

/// Dispatches `index verify`, which does not go through [`Args`] or
/// [`Args::common`] since it has no [`Common`] and talks to Bazel directly.
pub fn run_verify(ctx: &Context, args: Verify) -> Result<u8> {
    if args.sample == 0 {
        anyhow::bail!("--sample must be at least 1");
    }
    let engine = ctx.engine()?;
    let f = IndexFile::open(&args.file)?;
    let result = inspect::verify(&engine.bazel, f.view(), args.sample, args.seed)?;
    if let Some(fmt) = ctx.format.data("cambi index verify")? {
        emit(&result, fmt, print_verify)?;
    }
    Ok(if result.is_sound() {
        crate::EXIT_OK
    } else {
        crate::EXIT_ERROR
    })
}

fn print_verify(r: &inspect::VerifyResult) {
    for f in &r.failures {
        eprintln!(
            "UNSOUND {}: {} targets Bazel reports as rdeps are missing from the index",
            f.label,
            f.missing.len()
        );
        for m in f.missing.iter().take(10) {
            eprintln!("    {m}");
        }
    }
    println!("checked {} targets against bazel", r.checked);
    println!("unsound: {}", r.unsound);
    println!(
        "precision: {:.1}% ({} of {} reported targets are real rdeps)",
        r.precision_pct, r.theirs_total, r.ours_total
    );
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    if let Args::Verify(v) = args {
        return run_verify(ctx, v);
    }
    // `--format query` has no meaning for a byte count or a path, and a silent
    // fallback to text would be worse than saying so. Checked before the file
    // is opened: a usage error is the caller's, whatever the file's state.
    let fmt = ctx.format.data("cambi index")?;
    let common = args.common().clone();
    let f = IndexFile::open(&common.file)?;
    let q = IndexQuery::new(f.view());
    let Some(fmt) = fmt else {
        return run_quietly(&f, &q, args);
    };

    match args {
        Args::Inspect { label, package, .. } => {
            inspect_cmd(&f, &q, label.as_deref(), package.as_deref(), fmt)
        }
        Args::Rdeps { label, reach, .. } => {
            emit(&q.rdeps(&label, &(&reach).into())?, fmt, print_targets)
        }
        Args::Deps { label, reach, .. } => {
            emit(&q.deps(&label, &(&reach).into())?, fmt, print_targets)
        }
        Args::Path { from, to, .. } => {
            let r = q.path(&from, &to)?;
            let found = r.found;
            emit(&r, fmt, print_path)?;
            // Exit non-zero when there is no path, so a shell can branch on it.
            return Ok(if found {
                crate::EXIT_OK
            } else {
                crate::EXIT_ERROR
            });
        }
        Args::Consumers { paths, reach, .. } => emit(
            &q.consumers(&paths, &(&reach).into())?,
            fmt,
            print_consumers,
        ),
        Args::Owner { paths, .. } => emit(&q.owner(&paths)?, fmt, print_owner),
        Args::Targets { package, kind, .. } => {
            let kind = kind.as_deref().map(inspect::parse_kind).transpose()?;
            emit(&q.targets(&package, kind)?, fmt, print_package_targets)
        }
        Args::Stats { .. } => emit(&q.stats(f.file_size() as u64), fmt, print_stats),
        Args::Externals { repo, .. } => {
            emit(&externals(f.view(), repo.as_deref()), fmt, print_externals)
        }
        Args::Verify(_) => unreachable!("handled above"),
    }?;
    Ok(crate::EXIT_OK)
}

/// Prints a result as JSON or through a text formatter.
fn emit<T: Serialize>(value: &T, fmt: DataFormat, text: fn(&T)) -> Result<()> {
    match fmt {
        DataFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
        DataFormat::Jsonl => println!("{}", crate::cmd::result_event(value)?),
        DataFormat::Text => text(value),
    }
    Ok(())
}

/// `--format null`: run the query for its exit code and print nothing.
///
/// The query still runs, because the exit code is an answer — `path` says
/// whether there is a path, and an unknown label is still an error.
fn run_quietly(f: &IndexFile, q: &IndexQuery<'_>, args: Args) -> Result<u8> {
    match args {
        Args::Inspect { label, package, .. } => {
            if let Some(l) = label {
                q.detail(&l)?;
            } else if let Some(p) = package {
                q.targets(&p, None)?;
            }
        }
        Args::Rdeps { label, reach, .. } => {
            q.rdeps(&label, &(&reach).into())?;
        }
        Args::Deps { label, reach, .. } => {
            q.deps(&label, &(&reach).into())?;
        }
        Args::Path { from, to, .. } => {
            return Ok(if q.path(&from, &to)?.found {
                crate::EXIT_OK
            } else {
                crate::EXIT_ERROR
            });
        }
        Args::Consumers { paths, reach, .. } => {
            q.consumers(&paths, &(&reach).into())?;
        }
        Args::Owner { paths, .. } => {
            q.owner(&paths)?;
        }
        Args::Targets { package, kind, .. } => {
            let kind = kind.as_deref().map(inspect::parse_kind).transpose()?;
            q.targets(&package, kind)?;
        }
        Args::Stats { .. } => {
            q.stats(f.file_size() as u64);
        }
        Args::Externals { .. } => {}
        Args::Verify(_) => unreachable!("handled above"),
    }
    Ok(crate::EXIT_OK)
}

fn print_targets(r: &inspect::TargetsResult) {
    for t in &r.targets {
        println!("{t}");
    }
}

fn print_path(r: &inspect::PathResult) {
    if r.found {
        println!("{}", r.path.join("\n  -> "));
    } else {
        eprintln!("cambium: no dependency path from {} to {}", r.from, r.to);
        if let Some(h) = &r.reversed_hint {
            eprintln!("cambium: {h}");
        }
    }
}

fn print_consumers(r: &inspect::ConsumersResult) {
    for f in &r.files {
        match (&f.package, &f.label) {
            (Some(_), Some(label)) if !f.consumers.is_empty() => {
                for c in &f.consumers {
                    println!("{}\t{c}", f.path);
                }
                let _ = label;
            }
            (Some(pkg), Some(_)) => {
                eprintln!("cambium: {} is in //{pkg} but no rule consumes it", f.path);
            }
            (Some(pkg), None) => {
                eprintln!(
                    "cambium: {} is in //{pkg} but is not referenced by any rule",
                    f.path
                );
            }
            _ => eprintln!("cambium: {} is not under any package", f.path),
        }
    }
}

fn print_owner(r: &inspect::OwnerResult) {
    for p in &r.paths {
        match &p.package {
            Some(pkg) if pkg.is_empty() => println!("{}\t//", p.path),
            Some(pkg) => println!("{}\t//{pkg}", p.path),
            None => println!("{}\t-", p.path),
        }
    }
}

fn print_package_targets(r: &inspect::PackageTargets) {
    for t in &r.targets {
        println!("{:<16}{}", t.kind, t.label);
    }
}

fn print_stats(s: &inspect::IndexStats) {
    if let Some(w) = s.partial.warning() {
        println!("WARNING:         {w}");
        if !s.partial.packages.is_empty() {
            println!("  packages in error (first {}):", s.partial.packages.len());
            for p in &s.partial.packages {
                // Main-repo packages are bare names; an external repository
                // that failed to configure comes back already qualified.
                let label = if p.starts_with('@') {
                    p.clone()
                } else {
                    format!("//{p}")
                };
                println!("    {label}");
            }
        }
        println!();
    }
    println!("commit:          {}", s.commit);
    println!("lineage:         {}", s.lineage);
    println!("format version:  {}", s.format_version);
    println!();
    let c = &s.counts;
    println!("nodes:           {}", c.nodes);
    println!("  rules:         {} ({} tests)", c.rules, c.tests);
    println!("  source files:  {}", c.source_files);
    println!("  generated:     {}", c.generated_files);
    println!("  package groups:{}", c.package_groups);
    println!("  unknown:       {}", c.unknown);
    println!("  tombstones:    {}", c.tombstones);
    println!("edges:           {}", c.edges);
    println!(
        "packages:        {} ({} tombstoned)",
        c.packages, c.tombstoned_packages
    );
    println!(
        "bzl closure:     {} .bzl files ({} reachable from MODULE.bazel/WORKSPACE)",
        c.bzl_files, c.bzl_workspace_files
    );
    println!();
    println!(
        "in-degree:       max {} ({})",
        s.in_degree.max, s.in_degree.max_label
    );
    println!(
        "                 p99 {}  p50 {}  mean {:.2}",
        s.in_degree.p99, s.in_degree.p50, s.in_degree.mean
    );
    if !s.in_degree.top.is_empty() {
        println!("hubs (rules by in-degree):");
        for h in &s.in_degree.top {
            println!("  {:>8}  {}", h.in_degree, h.label);
        }
    }
    println!();
    let b = &s.bytes;
    println!("bytes:           {} total", human(b.file_total));
    let rows: [(&str, i64); 12] = [
        ("label table", b.label_table as i64),
        ("label sort index", b.label_sorted as i64),
        ("rule class table", b.rule_class_table as i64),
        ("forward offsets", b.forward_offsets as i64),
        ("forward edges", b.forward_edges as i64),
        ("reverse offsets", b.reverse_offsets as i64),
        ("reverse edges", b.reverse_edges as i64),
        ("files csr (view)", b.files_csr as i64),
        ("rule hashes", b.rule_hashes as i64),
        ("node columns", b.node_columns as i64),
        ("package table", b.package_table as i64),
        ("bzl load closure", b.bzl_closure as i64),
    ];
    for (name, v) in rows {
        println!(
            "  {name:<18} {:>10}  {:>5.1}%",
            human(v.max(0) as u64),
            share(v, b.file_total)
        );
    }
    println!(
        "  {:<18} {:>10}  {:>5.1}%",
        "framing/padding",
        human(b.framing.max(0) as u64),
        share(b.framing, b.file_total)
    );
    println!();
    let lp = &b.label_packing;
    println!(
        "labels:          {:>10}  {:>5.1}% of the file (table + sort index)",
        human(b.label_table + b.label_sorted),
        b.label_total_share * 100.0
    );
    println!(
        "  packed         {:>10}  {:>5.1}% would remain, storing (package, name)",
        human(lp.packed_label_table + b.label_sorted),
        share(
            (lp.packed_label_table + b.label_sorted) as i64,
            b.file_total
        )
    );
    println!(
        "  saving         {:>10}  {:>5.1}% of the file, {} labels not packable",
        human(lp.prefix_bytes),
        lp.file_saving_share * 100.0,
        lp.unpackable_nodes
    );
}

fn share(part: i64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn inspect_cmd(
    f: &IndexFile,
    q: &IndexQuery<'_>,
    label: Option<&str>,
    package: Option<&str>,
    fmt: DataFormat,
) -> Result<()> {
    if let Some(l) = label {
        return emit(&q.detail(l)?, fmt, print_detail);
    }
    if let Some(p) = package {
        return emit(&q.targets(p, None)?, fmt, print_package_targets);
    }
    // No selector: the header and a kind histogram.
    let stats = q.stats(f.file_size() as u64);
    match fmt {
        DataFormat::Json => println!("{}", serde_json::to_string_pretty(&stats)?),
        DataFormat::Jsonl => println!("{}", crate::cmd::result_event(&stats)?),
        DataFormat::Text => {
            let v = q.view();
            println!("file:            {}", f.path().display());
            println!("format version:  {}", v.format_version);
            println!("commit:          {}", v.commit);
            println!("lineage:         {}", inspect::hex(&v.lineage));
            println!("bazel:           {}", v.bazel_version);
            println!("config hash:     {}", inspect::hex(&v.config_hash));
            println!("created:         {}", v.created_unix);
            if let Some(w) = inspect::PartialLoad::from(v).warning() {
                println!("WARNING:         {w}");
            }
            println!("size on disk:    {}", human(f.file_size() as u64));
            if f.is_compressed() {
                println!(
                    "                 zstd, {} decompressed",
                    human(f.framed_size() as u64)
                );
            }
            println!("nodes:           {}", v.num_nodes());
            println!("edges:           {}", v.num_edges());
            println!("packages:        {}", v.num_packages());
            for (kind, n) in q.kind_histogram() {
                println!("  {kind:<18} {n}");
            }
        }
    }
    Ok(())
}

fn print_detail(d: &inspect::TargetDetail) {
    println!("{}", d.label);
    println!("  id:         {}", d.id);
    println!("  kind:       {}", d.kind);
    println!("  package:    {}", d.package.as_deref().unwrap_or("-"));
    println!("  rule class: {}", d.rule_class.as_deref().unwrap_or("-"));
    println!("  is_test:    {}", d.is_test);
    println!("  manual:     {}", d.manual);
    println!("  tombstone:  {}", d.tombstone);
    println!("  hash:       {}", d.rule_hash);
    println!("  deps ({}):", d.deps.len());
    for x in &d.deps {
        println!("    {x}");
    }
    println!("  rdeps ({}):", d.rdeps.len());
    for x in &d.rdeps {
        println!("    {x}");
    }
}

#[derive(Serialize)]
struct ExternalsRepo {
    name: String,
    canonical: String,
    path_maps: Vec<String>,
    /// `(pattern, feeds the rule)`.
    inputs: Vec<(String, bool)>,
}

#[derive(Serialize)]
struct ExternalsBzl {
    path: String,
    repos: Vec<String>,
}

#[derive(Serialize)]
struct ExternalsDump {
    bzl_closure: bool,
    repos: Vec<ExternalsRepo>,
    workspace_bzl: Vec<ExternalsBzl>,
    ext_labels: Vec<String>,
}

fn externals(v: cambium_index::IndexRef<'_>, only: Option<&str>) -> ExternalsDump {
    let mut inputs: std::collections::BTreeMap<u32, Vec<(String, bool)>> = Default::default();
    for p in 0..v.repo_input_patterns.len() as u32 {
        let pat = v.repo_input_patterns.get(p).to_string();
        let feeds = !v.repo_input_target_only[p as usize];
        for &r in v.repo_inputs.row(p) {
            inputs.entry(r).or_default().push((pat.clone(), feeds));
        }
    }
    let repos = (0..v.num_repos())
        .filter(|&r| only.map_or(true, |o| o == v.repo_name(r)))
        .map(|r| ExternalsRepo {
            name: v.repo_name(r).to_string(),
            canonical: v.repo_canonical_name(r).to_string(),
            path_maps: v
                .repo_path_maps(r)
                .iter()
                .map(|m| {
                    format!(
                        "{} -> {}{}",
                        m.from,
                        m.to,
                        if m.exhaustive { " (exhaustive)" } else { "" }
                    )
                })
                .collect(),
            inputs: inputs.remove(&r).unwrap_or_default(),
        })
        .collect();
    let workspace_bzl = (0..v.num_bzl())
        .filter(|&id| v.bzl_in_workspace(id))
        .map(|id| ExternalsBzl {
            path: v.bzl_path(id).to_string(),
            repos: v
                .bzl_repos_of(id)
                .iter()
                .map(|&r| v.repo_name(r).to_string())
                .collect(),
        })
        .filter(|b| only.map_or(true, |o| b.repos.iter().any(|r| r == o)))
        .collect();
    let ext_labels = (0..v.bzl_ext_labels.len() as u32)
        .map(|e| v.bzl_ext_labels.get(e).to_string())
        .filter(|l| only.map_or(true, |o| l.contains(o)))
        .collect();
    ExternalsDump {
        bzl_closure: v.bzl_closure,
        repos,
        workspace_bzl,
        ext_labels,
    }
}

fn print_externals(d: &ExternalsDump) {
    println!("bzl_closure: {}", d.bzl_closure);
    for r in &d.repos {
        println!("repo {}\tcanonical {:?}", r.name, r.canonical);
        for m in &r.path_maps {
            println!("  map     {m}");
        }
        for (p, feeds) in &r.inputs {
            println!(
                "  input   {p}{}",
                if *feeds { "" } else { "\t(target only)" }
            );
        }
    }
    for b in &d.workspace_bzl {
        println!("bzl {}\t{}", b.path, b.repos.join(" "));
    }
    for l in &d.ext_labels {
        println!("ext {l}");
    }
}
