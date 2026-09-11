pub mod affected;
pub mod bench;
pub mod index;
pub mod inspect;
pub mod replay;
pub mod synth;

use std::path::PathBuf;

use anyhow::Result;
use cambium_engine::{Config, Engine};

pub struct Context {
    pub root: PathBuf,
    pub config: Option<PathBuf>,
    pub bazel_binary: Option<String>,
    pub cache_dir: Option<PathBuf>,
    /// The global `--format`.
    pub format: crate::Format,
    /// Where diagnostics go, and in what shape.
    pub render: crate::diag::Renderer,
}

impl Context {
    pub fn engine(&self) -> Result<Engine> {
        let engine = Engine::open(
            &self.root,
            self.config.as_deref(),
            self.bazel_binary.as_deref(),
        )?;
        Ok(match &self.cache_dir {
            Some(d) => engine.with_cache_dir(d.clone()),
            None => engine,
        })
    }

    pub fn config(&self) -> Result<Config> {
        Ok(match &self.config {
            Some(p) => Config::load_from(p)?,
            None => match cambium_engine::engine::Engine::open(&self.root, None, None) {
                Ok(e) => e.cfg,
                // Outside a workspace, fall back to the config next to cwd.
                Err(_) => Config::load(&self.root)?,
            },
        })
    }
}

/// `cambi config show|hash`. `explain` — every value and where it came from —
/// is a later step.
#[derive(clap::Subcommand)]
pub enum ConfigArgs {
    /// Print the effective configuration and its hash.
    Show,
    /// Print the config hash alone, as hex.
    Hash,
}

pub fn config(ctx: &Context, args: ConfigArgs) -> Result<u8> {
    match args {
        ConfigArgs::Show => config_show(ctx),
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
        // The config has no JSON document of its own yet (`config explain` is
        // a later step), so the JSON forms print the TOML as one string field
        // beside the hash every index carries.
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
