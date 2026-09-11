//! `cambi init`: write `cambium.toml`, and nothing else.
//!
//! It builds no index, runs no Bazel, and never asks for or stores a
//! credential. The store backends are a directory, an HTTP base, or a command
//! template that drives `aws s3` or `gsutil` under whatever credentials the
//! environment already has — that is the whole of what `cambi` will ever know
//! about authentication, and `init` is where someone would otherwise expect to
//! be asked for a key.
//!
//! Everything it can detect it detects and shows; the one thing it cannot infer
//! is where the indexes should live, so that is the only question it asks.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cambium_engine::config::{StoreConfig, CONFIG_FILE, LOCAL_CONFIG_FILE};
use cambium_engine::probe;
use clap::Args as ClapArgs;

use crate::cmd::Context;
use crate::diag::UsageError;

#[derive(ClapArgs)]
pub struct Args {
    /// Keep indexes in a directory (a shared mount, or a local path).
    #[arg(long, value_name = "PATH", conflicts_with_all = ["store_s3", "store_gcs", "store_http", "store_none"])]
    pub store_dir: Option<PathBuf>,
    /// Keep indexes in S3, driven by `aws s3` under this environment's own
    /// credentials.
    #[arg(long, value_name = "BUCKET/PREFIX", conflicts_with_all = ["store_gcs", "store_http", "store_none"])]
    pub store_s3: Option<String>,
    /// Keep indexes in GCS, driven by `gsutil`.
    #[arg(long, value_name = "BUCKET/PREFIX", conflicts_with_all = ["store_http", "store_none"])]
    pub store_gcs: Option<String>,
    /// Read indexes over HTTP from this base URL. Read-only: publishing needs
    /// one of the other backends.
    #[arg(long, value_name = "URL", conflicts_with = "store_none")]
    pub store_http: Option<String>,
    /// Configure no store: every run builds its own index.
    #[arg(long)]
    pub store_none: bool,
    /// The branch a bare `cambi affected` measures from. Detected from
    /// `origin/HEAD` when it can be.
    #[arg(long, value_name = "BRANCH")]
    pub default_branch: Option<String>,
    /// Take the defaults for everything not given, without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

impl Args {
    fn store(&self) -> Option<StoreConfig> {
        if let Some(path) = &self.store_dir {
            return Some(StoreConfig::Dir { path: path.clone() });
        }
        if let Some(loc) = &self.store_s3 {
            return Some(command_store("s3", loc));
        }
        if let Some(loc) = &self.store_gcs {
            return Some(command_store("gs", loc));
        }
        if let Some(url) = &self.store_http {
            return Some(StoreConfig::Http {
                url: url.clone(),
                headers: Vec::new(),
            });
        }
        self.store_none.then_some(StoreConfig::None)
    }
}

/// An object-store backend as a command template.
///
/// `aws s3 cp` and `gsutil cp` take the same shape and both exit 1 for a
/// missing key, which `not_found_exit` records so that any *other* failure — an
/// expired credential, a wrong bucket — is a store error rather than a silent
/// miss that turns into a cold build.
fn command_store(scheme: &str, location: &str) -> StoreConfig {
    let base = format!("{scheme}://{}", location.trim_matches('/'));
    let tool = if scheme == "s3" {
        vec!["aws".to_string(), "s3".to_string()]
    } else {
        vec!["gsutil".to_string()]
    };
    let mut fetch = tool.clone();
    fetch.extend(["cp".into(), format!("{base}/{{name}}"), "{path}".into()]);
    let mut push = tool.clone();
    push.extend(["cp".into(), "{path}".into(), format!("{base}/{{name}}")]);
    let mut list = tool;
    list.extend(if scheme == "s3" {
        vec!["ls".into(), "--recursive".into(), format!("{base}/")]
    } else {
        vec!["ls".into(), "-r".into(), format!("{base}/**")]
    });
    StoreConfig::Command {
        fetch_cmd: fetch,
        push_cmd: push,
        list_cmd: list,
        not_found_exit: Some(1),
    }
}

pub fn run(ctx: &Context, args: Args) -> Result<u8> {
    let probes = probe::collect(&ctx.probe_options());
    report_detected(&probes);

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let store = match args.store() {
        Some(s) => s,
        None if args.yes => StoreConfig::None,
        None if interactive => ask_for_store()?,
        // A script that gave no flags and cannot be asked must be told which
        // flags exist rather than handed a default it did not choose.
        None => {
            return Err(UsageError::raised(
                "cambi init has nothing to ask: stdin is not a terminal and no store was \
                 given. Pass one of --store-dir PATH, --store-s3 BUCKET/PREFIX, --store-gcs \
                 BUCKET/PREFIX, --store-http URL or --store-none, or --yes to take the \
                 defaults (no store)",
            ))
        }
    };

    let default_branch = args
        .default_branch
        .clone()
        .or_else(|| detect_default_branch(&probes));

    let path = ctx
        .config
        .clone()
        .unwrap_or_else(|| workspace_root(&probes, ctx).join(CONFIG_FILE));
    let existed = path.exists();
    write_config(&path, &store, default_branch.as_deref())?;
    println!();
    println!(
        "{} {}",
        if existed { "updated" } else { "wrote" },
        path.display()
    );
    if existed {
        println!("  (only the keys init was asked about changed)");
    }

    // Reachability, not the round trip: `init` writes a config file and does
    // not need to prove the store is writable to do it. An unreachable store is
    // worth saying and is not worth refusing over — the config may well be for
    // a bucket this machine cannot see.
    let configured = !matches!(store, StoreConfig::None);
    if configured {
        let mut cfg = cambium_engine::Config::default();
        cfg.baseline.store = store.clone();
        let sink = cambium_engine::Diagnostics::new();
        let backend = cambium_engine::store::from_config(&cfg.baseline.store, &sink);
        let sp = probe::store(&cfg, backend.as_deref());
        match sp.reachable {
            Some(false) => {
                for d in sp.diagnostics.iter().chain(sink.drain().iter()) {
                    ctx.render.warn(d);
                }
                println!("  the store did not answer; the file is written either way");
            }
            Some(true) => println!("  the store answered"),
            None => {}
        }
    }

    warn_about_gitignore(ctx, &path);

    println!();
    println!("next: {}", next_command(configured));
    Ok(crate::EXIT_OK)
}

fn workspace_root(p: &probe::Probes, ctx: &Context) -> PathBuf {
    p.workspace.root.clone().unwrap_or_else(|| ctx.root.clone())
}

fn report_detected(p: &probe::Probes) {
    println!("detected");
    match &p.workspace.root {
        None => println!(
            "  no Bazel workspace above the current directory; cambium.toml goes where you are"
        ),
        Some(root) => {
            println!("  workspace  {}", root.display());
            println!(
                "  mode       {}",
                match p.workspace.mode {
                    Some(probe::WorkspaceMode::Bzlmod) if p.workspace.inert_workspace =>
                        "bzlmod (the WORKSPACE file is inert)",
                    Some(probe::WorkspaceMode::Bzlmod) => "bzlmod",
                    Some(probe::WorkspaceMode::Workspace) if p.workspace.inert_module_bazel =>
                        "WORKSPACE (the MODULE.bazel file is inert)",
                    Some(probe::WorkspaceMode::Workspace) => "WORKSPACE",
                    Some(probe::WorkspaceMode::Ambiguous) =>
                        "MODULE.bazel and WORKSPACE both present, and no .bazelversion to say which wins",
                    None => "no marker file",
                }
            );
            if let Some(v) = &p.workspace.bazelversion {
                println!("  bazel      {v} (.bazelversion)");
            }
            if !p.workspace.bazelrc_try_imports.is_empty() {
                println!(
                    "  bazelrc    try-import {}",
                    p.workspace.bazelrc_try_imports.join(", ")
                );
            }
        }
    }
}

/// `origin/HEAD`, which is what `git clone` sets and what a CI checkout keeps.
fn detect_default_branch(p: &probe::Probes) -> Option<String> {
    let root = p.workspace.root.clone()?;
    let repo = cambium_diff::Repo::discover(&root).ok()?;
    let out = repo
        .git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .ok()?;
    out.trim().strip_prefix("origin/").map(str::to_string)
}

fn ask_for_store() -> Result<StoreConfig> {
    println!();
    println!("where should published indexes live?");
    println!("  1  a directory (a shared mount, or a local path)");
    println!("  2  S3, through `aws s3` and this environment's credentials");
    println!("  3  GCS, through `gsutil`");
    println!("  4  an HTTP base URL (read-only)");
    println!("  5  nowhere: every run builds its own index");
    let choice = prompt("choice [5]: ")?;
    Ok(match choice.trim() {
        "1" => StoreConfig::Dir {
            path: PathBuf::from(prompt_required("directory: ")?),
        },
        "2" => command_store("s3", &prompt_required("bucket/prefix: ")?),
        "3" => command_store("gs", &prompt_required("bucket/prefix: ")?),
        "4" => StoreConfig::Http {
            url: prompt_required("base URL: ")?,
            headers: Vec::new(),
        },
        _ => StoreConfig::None,
    })
}

fn prompt(message: &str) -> Result<String> {
    use std::io::Write as _;
    print!("{message}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_required(message: &str) -> Result<String> {
    let answer = prompt(message)?;
    if answer.is_empty() {
        return Err(UsageError::raised(
            "that question has no default; pass the matching --store-* flag instead",
        ));
    }
    Ok(answer)
}

/// Writes the keys `init` was asked about, preserving everything else in the
/// file as text.
///
/// `toml_edit` keeps comments, key order and formatting, which matters because
/// `cambium.toml` is checked in and somebody wrote the rest of it on purpose. A
/// parse-merge-serialise through `Config` would silently reformat the whole
/// file and drop every comment in it.
fn write_config(path: &Path, store: &StoreConfig, default_branch: Option<&str>) -> Result<String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(|e| {
        anyhow::anyhow!(
            "{} does not parse, so init will not rewrite it: {e}",
            path.display()
        )
    })?;
    if existing.trim().is_empty() {
        doc.decor_mut()
            .set_prefix("# cambium.toml — checked in, and hashed into every index it produces.\n# Machine-local overrides go in cambium.local.toml.\n\n");
    }

    match store {
        // No store is the default, so the key goes rather than being written
        // as `kind = "none"`: `init --store-none` on a file that had a store
        // means "stop using it", and a config that says nothing about a store
        // is the one the defaults describe.
        StoreConfig::None => {
            if let Some(baseline) = doc.get_mut("baseline").and_then(|i| i.as_table_mut()) {
                baseline.remove("store");
                if baseline.is_empty() {
                    doc.remove("baseline");
                }
            }
        }
        other => {
            table(&mut doc, "baseline")["store"] = toml_edit::value(store_table(other)?);
        }
    }
    if let Some(branch) = default_branch {
        table(&mut doc, "git")["default_branch"] = toml_edit::value(branch);
    }

    let text = doc.to_string();
    std::fs::write(path, &text)?;
    Ok(text)
}

/// The store as an inline table, in the spelling `StoreConfig` parses — by
/// serialising the value itself, so the tag and the field names cannot drift
/// from the engine's own `serde` attributes.
fn store_table(store: &StoreConfig) -> Result<toml_edit::InlineTable> {
    #[derive(serde::Serialize)]
    struct Doc<'a> {
        store: &'a StoreConfig,
    }
    let doc = toml_edit::ser::to_document(&Doc { store })?;
    // The serializer may write the nested value as `[store]` or as an inline
    // table; `into_table` reads either.
    let table = doc
        .get("store")
        .cloned()
        .and_then(|item| item.into_table().ok())
        .ok_or_else(|| anyhow::anyhow!("a store serialises as a table"))?;
    Ok(table.into_inline_table())
}

/// The named top-level table, created if the file does not have one yet.
fn table<'a>(doc: &'a mut toml_edit::DocumentMut, name: &str) -> &'a mut toml_edit::Table {
    doc.entry(name)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .expect("a top-level section is a table")
}

/// `cambium.local.toml` is machine-local by convention and by nothing else, so
/// a repository whose `.gitignore` does not mention it will eventually commit
/// one. Said, not fixed: `.gitignore` belongs to the repository.
fn warn_about_gitignore(ctx: &Context, config_path: &Path) {
    let root = match config_path.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    let gitignore = root.join(".gitignore");
    let Ok(text) = std::fs::read_to_string(&gitignore) else {
        return;
    };
    if text.lines().any(|l| l.trim() == LOCAL_CONFIG_FILE) {
        return;
    }
    ctx.render.warn(
        &cambium_engine::Diagnostic::warning(
            cambium_engine::Code::W020GitignoreLocalConfig,
            format!("{} does not mention {LOCAL_CONFIG_FILE}", gitignore.display()),
        )
        .with_help(format!("add a `{LOCAL_CONFIG_FILE}` line to .gitignore: it holds this machine's paths and binaries, and committing one makes them everybody's")),
    );
}

fn next_command(store_configured: bool) -> &'static str {
    if store_configured {
        "cambi index build --publish"
    } else {
        "cambi doctor"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_s3_backend_is_a_command_template_and_never_a_credential() {
        let StoreConfig::Command {
            fetch_cmd,
            push_cmd,
            list_cmd,
            not_found_exit,
        } = command_store("s3", "my-bucket/cambium/")
        else {
            panic!("s3 is the command backend");
        };
        assert_eq!(
            fetch_cmd,
            ["aws", "s3", "cp", "s3://my-bucket/cambium/{name}", "{path}"]
        );
        assert_eq!(
            push_cmd,
            ["aws", "s3", "cp", "{path}", "s3://my-bucket/cambium/{name}"]
        );
        assert_eq!(
            list_cmd,
            ["aws", "s3", "ls", "--recursive", "s3://my-bucket/cambium/"]
        );
        assert_eq!(
            not_found_exit,
            Some(1),
            "aws s3 cp exits 1 for a missing key, so any other exit is a real failure"
        );
        let all = format!("{fetch_cmd:?}{push_cmd:?}{list_cmd:?}");
        for word in ["key", "secret", "token", "password"] {
            assert!(!all.contains(word), "the template mentions a {word}");
        }
    }

    #[test]
    fn gcs_drives_gsutil() {
        let StoreConfig::Command { fetch_cmd, .. } = command_store("gs", "b/p") else {
            panic!("gcs is the command backend");
        };
        assert_eq!(fetch_cmd, ["gsutil", "cp", "gs://b/p/{name}", "{path}"]);
    }

    /// The file is checked in and somebody wrote the rest of it deliberately:
    /// every comment, every key order and every unrelated key survives.
    #[test]
    fn writing_preserves_everything_it_was_not_asked_about() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &path,
            "# our own notes\n[invalidators]\n# the toolchain moves the world\nglobs = [\"tools/**\"]\n\n[limits]\nmax_dirty_packages = 900\n",
        )
        .unwrap();
        let store = StoreConfig::Dir {
            path: PathBuf::from("/srv/idx"),
        };
        let text = write_config(&path, &store, Some("trunk")).unwrap();
        assert!(text.contains("# our own notes"), "{text}");
        assert!(text.contains("# the toolchain moves the world"), "{text}");
        assert!(text.contains("max_dirty_packages = 900"), "{text}");
        assert!(text.contains("default_branch = \"trunk\""), "{text}");
        assert!(text.contains("kind = \"dir\""), "{text}");

        // And what it wrote is what the engine reads back.
        let cfg = cambium_engine::Config::load_from(&path).unwrap();
        assert_eq!(cfg.baseline.store, store);
        assert_eq!(cfg.git.default_branch, "trunk");
        assert_eq!(cfg.limits.max_dirty_packages, 900);
        assert_eq!(cfg.invalidators.globs, ["tools/**"]);
    }

    #[test]
    fn a_fresh_file_is_loadable_and_carries_a_header_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let text = write_config(&path, &StoreConfig::None, None).unwrap();
        assert!(text.starts_with("# cambium.toml"), "{text}");
        assert_eq!(
            cambium_engine::Config::load_from(&path).unwrap(),
            cambium_engine::Config::default(),
            "no store and no branch is the default configuration"
        );
    }

    #[test]
    fn the_next_command_is_the_one_that_makes_the_store_useful() {
        assert_eq!(next_command(true), "cambi index build --publish");
        assert_eq!(next_command(false), "cambi doctor");
    }

    #[test]
    fn an_s3_store_round_trips_through_the_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let store = command_store("s3", "bucket/prefix");
        write_config(&path, &store, None).unwrap();
        assert_eq!(
            cambium_engine::Config::load_from(&path)
                .unwrap()
                .baseline
                .store,
            store
        );
    }
}
