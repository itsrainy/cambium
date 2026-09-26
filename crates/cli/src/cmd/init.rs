//! `cambi init`: write `cambium.toml`, and nothing else.
//!
//! It builds no index, runs no Bazel, and never asks for or stores a
//! credential. The store backends are a directory, an HTTP base, or a command
//! template that drives `aws s3` or `gsutil` under whatever credentials the
//! environment already has. That is the whole of what `cambi` will ever know
//! about authentication, and `init` is where someone would otherwise expect to
//! be asked for a key.
//!
//! Everything it can detect it detects and shows; the one thing it cannot infer
//! is where the indexes should live, so that is the only question it asks.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cambium_engine::config::{ConfigError, StoreConfig, CONFIG_FILE, LOCAL_CONFIG_FILE};
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
/// `aws s3 cp` and `gsutil cp` take the same shape, and both exit 1 for a
/// missing key, which `not_found_exit` records so that an exit with any other
/// status is a store error. Both also exit 1 for a refused credential or a
/// refused connection, so with these templates an outage reads as a miss;
/// docs/STORE.md ("Misses and outages") says so and gives a fetch wrapper
/// that tells the two apart.
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
    // What the detection found wrong (a mode that depends on which Bazel
    // runs) is said now, while the person is choosing, not left to `doctor`.
    for d in &probes.workspace.diagnostics {
        ctx.render.warn(d);
    }

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
    // worth saying and is not worth refusing over: the config may well be for
    // a bucket this machine cannot see.
    let configured = !matches!(store, StoreConfig::None);
    if configured {
        let mut cfg = cambium_engine::Config::default();
        cfg.baseline.store = store.clone();
        let sink = cambium_engine::Diagnostics::new();
        let backend = cambium_engine::store::from_config(
            &cfg.baseline.store,
            &workspace_root(&probes, ctx),
            &sink,
        );
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
    // Only a missing file starts empty. Any other read error (a byte that is
    // not UTF-8, a permission) is the config-read error `Config::load` gives,
    // because rewriting a file init could not read would replace everything
    // in it with the two keys init was asked about.
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(cambium_engine::EngineError::Config(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })
            .into())
        }
    };
    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(|e| {
        anyhow::anyhow!(
            "{} does not parse, so init will not rewrite it: {e}",
            path.display()
        )
    })?;
    if existing.trim().is_empty() {
        doc.decor_mut()
            .set_prefix("# cambium.toml: checked in, and hashed into every index it produces.\n# Machine-local overrides go in cambium.local.toml.\n\n");
    }

    match store {
        // No store is the default, so the key goes rather than being written
        // as `kind = "none"`: `init --store-none` on a file that had a store
        // means "stop using it", and a config that says nothing about a store
        // is the one the defaults describe.
        StoreConfig::None => {
            if doc.contains_key("baseline") {
                let baseline = section(&mut doc, "baseline", path)?;
                baseline.remove("store");
                if baseline.is_empty() {
                    doc.remove("baseline");
                }
            }
        }
        other => {
            section(&mut doc, "baseline", path)?
                .insert("store", toml_edit::value(store_table(other)?));
        }
    }
    if let Some(branch) = default_branch {
        section(&mut doc, "git", path)?.insert("default_branch", toml_edit::value(branch));
    }

    let text = doc.to_string();
    std::fs::write(path, &text).map_err(|source| cambium_engine::EngineError::IoAt {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(text)
}

/// The store as an inline table, in the spelling `StoreConfig` parses, by
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

/// The named top-level section, created as a `[name]` table if the file does
/// not have one yet.
///
/// `name = { ... }` is the same section to the engine, so it is edited where
/// it is. A section that is not a table at all is a file the engine would
/// refuse, and init refuses it too rather than guess what was meant.
fn section<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    name: &str,
    path: &Path,
) -> Result<&'a mut dyn toml_edit::TableLike> {
    doc.entry(name)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: `{name}` is not a table, so init will not rewrite it",
                path.display()
            )
        })
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
    // Bytes, as git reads it: a byte that is not UTF-8 in a comment makes the
    // file no less a .gitignore, and reading it as a string would skip the
    // warning for it. A file that cannot be read at all is said nothing about;
    // this is advice, not a check init can fail on.
    let Ok(bytes) = std::fs::read(&gitignore) else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
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

/// With a store, the profile first: it measures the repository and its
/// `--out` is the first cold index, so publishing that file costs no second
/// build. `index build --publish` stays the way to skip the measuring.
fn next_command(store_configured: bool) -> &'static str {
    if store_configured {
        "cambi profile --commits 30 --out ./cambium-index.bin, then cambi index publish \
         ./cambium-index.bin (or cambi index build --publish to skip the profile)"
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

    /// A file init cannot read is not an empty file. Treating it as one
    /// replaced every key somebody wrote with the header and the two keys
    /// init was asked about, and said "only the keys init was asked about
    /// changed".
    #[test]
    fn an_unreadable_file_is_refused_and_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        // `caf\xe9` is Latin-1, so reading it as UTF-8 is `InvalidData`.
        let before = b"# caf\xe9 notes\n[limits]\nmax_dirty_packages = 900\n".to_vec();
        std::fs::write(&path, &before).unwrap();
        let store = StoreConfig::Dir {
            path: PathBuf::from("/srv/idx"),
        };
        let err = write_config(&path, &store, Some("trunk"))
            .expect_err("a file that cannot be read must not be rewritten");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the file init could not read was changed"
        );
        let d = crate::diag::of_error(&err);
        assert_eq!(
            d.code,
            cambium_engine::Code::E002Config,
            "the config-read code Config::load uses: {}",
            d.message
        );
        assert!(
            d.message.contains(&path.display().to_string()),
            "the path is not named: {}",
            d.message
        );
        assert!(
            d.message.contains("UTF-8"),
            "the cause is not named: {}",
            d.message
        );
    }

    /// A write that fails says which file it was writing.
    #[test]
    fn a_failed_write_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        // No parent directory: the read is `NotFound`, so the write is what
        // fails.
        let path = dir.path().join("no-such-dir").join(CONFIG_FILE);
        let err = write_config(&path, &StoreConfig::None, None)
            .expect_err("the parent directory does not exist");
        let d = crate::diag::of_error(&err);
        assert!(
            d.message.contains(&path.display().to_string()),
            "the path is not named: {}",
            d.message
        );
    }

    /// `baseline = { ... }` and `git = { ... }` are the same sections as
    /// `[baseline]` and `[git]`, and the engine reads them the same way, so
    /// init edits them in place.
    #[test]
    fn inline_sections_are_edited_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &path,
            "baseline = { store = { kind = \"dir\", path = \"/old\" } }\ngit = { default_branch = \"main\" }\n",
        )
        .unwrap();
        let store = StoreConfig::Dir {
            path: PathBuf::from("/srv/idx"),
        };
        write_config(&path, &store, Some("trunk")).unwrap();
        let cfg = cambium_engine::Config::load_from(&path).unwrap();
        assert_eq!(cfg.baseline.store, store);
        assert_eq!(cfg.git.default_branch, "trunk");
    }

    /// `--store-none` on an inline `baseline` table removes the store it
    /// names; leaving it would report the file updated while every run went on
    /// using the store.
    #[test]
    fn store_none_removes_a_store_from_an_inline_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(
            &path,
            "baseline = { store = { kind = \"dir\", path = \"/old\" } }\n",
        )
        .unwrap();
        write_config(&path, &StoreConfig::None, None).unwrap();
        assert_eq!(
            cambium_engine::Config::load_from(&path)
                .unwrap()
                .baseline
                .store,
            StoreConfig::None,
            "{}",
            std::fs::read_to_string(&path).unwrap()
        );
    }

    /// A section that is not a table at all is refused with the path and the
    /// key, not a panic, and the file is left as it was.
    #[test]
    fn a_section_that_is_not_a_table_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "git = \"main\"\n").unwrap();
        let err =
            write_config(&path, &StoreConfig::None, Some("trunk")).expect_err("`git` is a string");
        let message = crate::diag::of_error(&err).message;
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("`git`"), "{message}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "git = \"main\"\n");
    }

    #[test]
    fn the_next_command_is_the_one_that_makes_the_store_useful() {
        let next = next_command(true);
        assert!(
            next.starts_with("cambi profile --commits 30 --out "),
            "{next}"
        );
        assert!(
            next.contains("then cambi index publish ./cambium-index.bin"),
            "{next}"
        );
        assert!(next.ends_with("(or cambi index build --publish to skip the profile)"));
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
