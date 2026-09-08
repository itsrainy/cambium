pub mod affected;
pub mod baseline;
pub mod bench;
pub mod index;
pub mod synth;

use std::path::PathBuf;

use anyhow::Result;
use cambium_engine::{Config, Engine};

pub struct Context {
    pub root: PathBuf,
    pub config: Option<PathBuf>,
    pub bazel_binary: Option<String>,
    pub cache_dir: Option<PathBuf>,
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

pub fn config_show(ctx: &Context) -> Result<u8> {
    let cfg = ctx.config()?;
    print!("{}", cfg.to_toml());
    println!("\n# config_hash = {}", hex(&cfg.hash()));
    Ok(crate::EXIT_OK)
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}
