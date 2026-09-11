//! `cambi status`: one screen saying where this checkout is and what a run
//! would load. The default verb, so a bare `cambi` prints it.
//!
//! `status` reports and `doctor` judges. Keeping the two apart is what keeps
//! this quiet: every line here is a fact, the Warnings section is whatever the
//! probes raised, and nothing is graded. It runs no Bazel — the version comes
//! off `.bazelversion` — and reaches the network only to ask the store for a
//! listing and the one index a run would load.

use anyhow::Result;
use cambium_engine::probe;

use crate::cmd::{result_event, Context};

pub fn run(ctx: &Context) -> Result<u8> {
    let probes = probe::collect(&ctx.probe_options());
    let diagnostics = probes.diagnostics();

    let Some(fmt) = ctx.format.data("cambi status")? else {
        return Ok(crate::EXIT_OK);
    };
    match fmt {
        crate::DataFormat::Text => print_human(&probes, &diagnostics),
        crate::DataFormat::Json | crate::DataFormat::Jsonl => {
            let doc = serde_json::json!({
                "schema": cambium_engine::report::SCHEMA_VERSION,
                "workspace": probes.workspace,
                "config": probes.config,
                "position": probes.position,
                "index": probes.index,
                "lineage": probes.lineage,
                "store": probes.store,
                "diagnostics": diagnostics,
            });
            if fmt == crate::DataFormat::Jsonl {
                println!("{}", result_event(&doc)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            }
        }
    }
    // Reporting is not judging: a workspace that cannot be found, a store that
    // cannot be reached and a cold position are all things `status` says and
    // exits 0 about. `cambi doctor` is the verb with an opinion.
    Ok(crate::EXIT_OK)
}

/// The sections `docs/CLI.md` lists, in that order.
fn print_human(p: &probe::Probes, diagnostics: &[cambium_engine::Diagnostic]) {
    println!("Workspace");
    match &p.workspace.root {
        None => {
            println!("  no Bazel workspace found above the current directory");
            println!("  (looked for MODULE.bazel, WORKSPACE, WORKSPACE.bazel, WORKSPACE.bzlmod)");
        }
        Some(root) => {
            println!("  root       {}", root.display());
            println!(
                "  mode       {}{}",
                match p.workspace.mode {
                    Some(probe::WorkspaceMode::Bzlmod) => "bzlmod",
                    Some(probe::WorkspaceMode::Workspace) => "WORKSPACE",
                    Some(probe::WorkspaceMode::Ambiguous) => "ambiguous",
                    None => "unknown",
                },
                if p.workspace.inert_module_bazel {
                    "  (MODULE.bazel is present and inert)"
                } else if p.workspace.inert_workspace {
                    "  (WORKSPACE is present and inert)"
                } else {
                    ""
                }
            );
            println!(
                "  bazel      {}",
                p.workspace
                    .bazelversion
                    .as_deref()
                    .map(|v| format!("{v} (.bazelversion)"))
                    .unwrap_or_else(|| "not pinned; `cambi doctor` asks the binary".into())
            );
            if !p.workspace.bazelrc_try_imports.is_empty() {
                println!(
                    "  bazelrc    try-import {}",
                    p.workspace.bazelrc_try_imports.join(", ")
                );
            }
        }
    }
    println!(
        "  config     {}{}",
        p.config.path.display(),
        if p.config.exists {
            ""
        } else {
            "  (absent; the defaults apply)"
        }
    );
    match (&p.config.hash, &p.config.parse_error) {
        (Some(h), _) => println!("  hash       {}", &h[..16]),
        (None, Some(e)) => println!("  hash       unknown: {e}"),
        (None, None) => {}
    }

    // The line this verb exists for.
    println!();
    println!("Position");
    match &p.position {
        None => println!("  no git repository here, so there is no revision to place"),
        Some(pos) => match (&pos.head, pos.has_index, &pos.nearest, pos.distance) {
            (None, ..) => println!("  HEAD could not be resolved (an empty repository?)"),
            (Some(head), true, ..) => println!(
                "  HEAD {} has an index{}",
                probe::short(head),
                lineage_suffix(p, "warm")
            ),
            (Some(head), false, Some(near), Some(back)) => println!(
                "  HEAD {} has no index; nearest is {}, {back} commit{} back{}",
                probe::short(head),
                probe::short(near),
                if back == 1 { "" } else { "s" },
                lineage_suffix(p, "splice")
            ),
            (Some(head), false, ..) => println!(
                "  HEAD {} has no index, and none of the last {} ancestors has one (cold)",
                probe::short(head),
                pos.max_ancestor_walk
            ),
        },
    }
    if p.position.as_ref().is_some_and(|pos| pos.dirty) {
        println!("  the working tree has uncommitted changes, which a run indexes as they are");
    }

    println!();
    println!("Index");
    match &p.index {
        None => println!("  none: the next run builds one with `bazel query //...`"),
        Some(i) => {
            println!("  size       {}", human_bytes(i.size_bytes));
            println!("  packages   {}", i.packages);
            println!("  targets    {} rules of {} nodes", i.targets, i.nodes);
            println!("  externals  {} repositories", i.external_repos);
            println!(
                "  built      {} by bazel {}",
                unix_time(i.built_at_unix),
                if i.bazel_version.is_empty() {
                    "(unrecorded)"
                } else {
                    &i.bazel_version
                }
            );
            println!("  format     version {}", i.format_version);
        }
    }

    println!();
    println!("Lineage");
    match &p.lineage {
        None | Some(probe::LineageProbe { current: None, .. }) => {
            println!("  none yet: a cold build mints one")
        }
        Some(l) => {
            println!(
                "  current    {}",
                &l.current.clone().unwrap_or_default()[..16]
            );
            if !l.others.is_empty() {
                println!(
                    "  others     {} in the store{}",
                    l.others.len(),
                    if l.conflicting.is_empty() {
                        ""
                    } else {
                        ", and at least one was built with a different config hash"
                    }
                );
            }
        }
    }

    println!();
    println!("Store");
    if !p.store.configured {
        println!("  none configured: every run builds its own index");
        println!("  (set [baseline].store, or run `cambi init --store-dir <path>`)");
    } else {
        println!(
            "  {}  {}",
            p.store.kind,
            p.store.location.as_deref().unwrap_or("-")
        );
        match (p.store.reachable, p.store.indexes) {
            // One line for an unreachable store, and everything above it still
            // printed: an offline store does not stop `status` saying where the
            // checkout is.
            (Some(false), _) => println!(
                "  unreachable: {}",
                p.store.error.as_deref().unwrap_or("no detail")
            ),
            (Some(true), Some(n)) => println!("  reachable, {n} indexes"),
            (Some(true), None) => println!("  reachable (this backend cannot be listed)"),
            (None, _) => {}
        }
    }

    if !diagnostics.is_empty() {
        println!();
        println!("Warnings");
        for d in diagnostics {
            println!("  {} {}", d.code, d.message);
            if let Some(h) = &d.help {
                println!("    help: {h}");
            }
        }
    }
}

/// `(lineage 9f..., splice)` — the lineage a run would splice onto and what it
/// would have to do.
fn lineage_suffix(p: &probe::Probes, plan: &str) -> String {
    match p.lineage.as_ref().and_then(|l| l.current.clone()) {
        Some(l) => format!(" (lineage {}, {plan})", &l[..6]),
        None => format!(" ({plan})"),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// A unix timestamp as an age, which is what someone reading `status` wants:
/// "how stale is this index", not "what time was it in UTC".
fn unix_time(unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if unix == 0 {
        return "at an unrecorded time".into();
    }
    format!("{} ago", duration(now.saturating_sub(unix)))
}

/// A span of seconds in the unit someone would say it in: `3 days`, `1 hour`.
/// `--dry-run` says how old the index it would load is with the same words.
pub fn duration(secs: u64) -> String {
    let (n, unit) = match secs {
        0..=119 => (secs, "second"),
        120..=3599 => (secs / 60, "minute"),
        3600..=86_399 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_rounded_the_way_a_reader_scans_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MiB");
    }

    #[test]
    fn an_unrecorded_build_time_does_not_print_as_the_epoch() {
        assert_eq!(unix_time(0), "at an unrecorded time");
    }

    #[test]
    fn an_age_is_singular_when_it_is_one() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(unix_time(now - 3600), "1 hour ago");
        assert_eq!(unix_time(now - 7200 * 2), "4 hours ago");
        assert_eq!(unix_time(now - 86_400 * 3), "3 days ago");
    }
}
