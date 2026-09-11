pub mod affected;
pub mod doctor;
pub mod explain;
pub mod index;
pub mod init;
pub mod inspect;
pub mod plan;
pub mod status;

use std::path::PathBuf;

use anyhow::Result;
use cambium_engine::config::Overrides;
use cambium_engine::{Config, Engine};

pub struct Context {
    pub root: PathBuf,
    pub config: Option<PathBuf>,
    /// What the command line and the environment said, with the spelling to
    /// attribute each value to. The highest layer of the configuration
    /// stack, and the only way a flag reaches the engine or a probe.
    pub overrides: Overrides,
    /// The global `--format`.
    pub format: crate::Format,
    /// Where diagnostics go, and in what shape.
    pub render: crate::diag::Renderer,
}

impl Context {
    pub fn engine(&self) -> Result<Engine> {
        Ok(Engine::open_with(
            &self.root,
            self.config.as_deref(),
            &self.overrides,
        )?)
    }

    pub fn config(&self) -> Result<Config> {
        Ok(self.layered()?.config)
    }

    /// The whole configuration stack with the origin of every effective value:
    /// defaults, `cambium.toml`, `cambium.local.toml`, then the flags and
    /// environment variables this invocation carries.
    ///
    /// Rooted at the workspace when there is one and at the working directory
    /// when there is not, so `cambi config explain` answers outside a
    /// workspace too.
    pub fn layered(&self) -> Result<cambium_engine::config::Layered> {
        let root =
            cambium_engine::bazel_workspace_root(&self.root).unwrap_or_else(|| self.root.clone());
        // Through `EngineError` so a config failure keeps its own diagnostic
        // code (`CAMBI-E014` for a misplaced local key) rather than arriving at
        // the top level as an anonymous internal error.
        Ok(
            Config::load_layered(&root, self.config.as_deref(), &self.overrides)
                .map_err(cambium_engine::EngineError::from)?,
        )
    }

    /// What `status`, `doctor` and `init` probe, from this invocation's globals.
    pub fn probe_options(&self) -> cambium_engine::probe::Options {
        cambium_engine::probe::Options {
            start: self.root.clone(),
            config_path: self.config.clone(),
            overrides: self.overrides.clone(),
            run_bazel: false,
            store_round_trip: false,
        }
    }
}

/// `cambi config show|explain|hash`.
#[derive(clap::Subcommand)]
pub enum ConfigArgs {
    /// Print the effective configuration and its hash.
    Show,
    /// Print every effective value and where it came from.
    Explain,
    /// Print the config hash alone, as hex.
    Hash,
}

pub fn config(ctx: &Context, args: ConfigArgs) -> Result<u8> {
    match args {
        ConfigArgs::Show => config_show(ctx),
        ConfigArgs::Explain => config_explain(ctx),
        ConfigArgs::Hash => {
            // One line, no formatting of its own: this exists so a script can
            // compare it against an index's, and `--format json` wraps it in
            // the same document `show` prints.
            let cfg = ctx.config()?;
            match ctx.format.data("cambi config hash")? {
                None => {}
                Some(crate::DataFormat::Text) => println!("{}", hex(&cfg.hash())),
                Some(fmt) => {
                    let doc = serde_json::json!({
                        "schema": cambium_engine::report::SCHEMA_VERSION,
                        "config_hash": hex(&cfg.hash()),
                    });
                    if fmt == crate::DataFormat::Jsonl {
                        println!("{}", result_event(&doc)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&doc)?);
                    }
                }
            }
            Ok(crate::EXIT_OK)
        }
    }
}

pub fn config_show(ctx: &Context) -> Result<u8> {
    let cfg = ctx.config()?;
    let Some(fmt) = ctx.format.data("cambi config show")? else {
        return Ok(crate::EXIT_OK);
    };
    match fmt {
        crate::DataFormat::Text => {
            print!("{}", cfg.to_toml());
            println!("\n# config_hash = {}", hex(&cfg.hash()));
        }
        // `show` is the file, not the attribution: the JSON forms print the
        // canonical TOML as one string field beside the hash every index
        // carries, and `explain` is the document with a field per value.
        crate::DataFormat::Json | crate::DataFormat::Jsonl => {
            let doc = serde_json::json!({
                "schema": cambium_engine::report::SCHEMA_VERSION,
                "config_hash": hex(&cfg.hash()),
                "toml": cfg.to_toml(),
            });
            if fmt == crate::DataFormat::Jsonl {
                println!("{}", result_event(&doc)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            }
        }
    }
    Ok(crate::EXIT_OK)
}

/// `cambi config explain`: one line per leaf key, with the layer that decided
/// it.
///
/// The question it answers is "why is this value this value", which on a
/// machine with a `cambium.local.toml`, a `--cache-dir` and a checked-in
/// `cambium.toml` is not guessable from any one of them.
pub fn config_explain(ctx: &Context) -> Result<u8> {
    let layered = ctx.layered()?;
    for d in &layered.diagnostics {
        ctx.render.warn(d);
    }
    let values = layered.explain();
    let Some(fmt) = ctx.format.data("cambi config explain")? else {
        return Ok(crate::EXIT_OK);
    };
    match fmt {
        crate::DataFormat::Text => {
            let width = values.iter().map(|v| v.key.len()).max().unwrap_or(0);
            for v in &values {
                println!(
                    "{:<width$} = {}  ({})",
                    v.key,
                    v.value,
                    v.origin_label,
                    width = width
                );
            }
            println!();
            println!("# config_hash = {}", hex(&layered.config.hash()));
        }
        crate::DataFormat::Json | crate::DataFormat::Jsonl => {
            let doc = serde_json::json!({
                "schema": cambium_engine::report::SCHEMA_VERSION,
                "config_hash": hex(&layered.config.hash()),
                "config_file": layered.path,
                "local_file": layered.local_exists().then(|| layered.local_path.clone()),
                "values": values,
            });
            if fmt == crate::DataFormat::Jsonl {
                println!("{}", result_event(&doc)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&doc)?);
            }
        }
    }
    Ok(crate::EXIT_OK)
}

/// One `jsonl` result line: the document, with `"event": "result"` in front of
/// its own fields.
pub fn result_event<T: serde::Serialize>(value: &T) -> Result<String> {
    let mut doc = serde_json::json!({"event": "result"});
    let map = doc.as_object_mut().expect("an object");
    match serde_json::to_value(value)? {
        serde_json::Value::Object(fields) => map.extend(fields),
        other => {
            map.insert("result".into(), other);
        }
    }
    Ok(serde_json::to_string(&doc)?)
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}
