use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub interval_secs: u64,
    pub data_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self, figment::Error> {
        Figment::new()
            .merge(Toml::file("linewatch.toml"))
            .merge(Env::prefixed("LINEWATCH_").ignore(&["_".into()]))
            .extract()
    }
}
