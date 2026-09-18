//! Environment-driven config. Names match the droplet's `.env` conventions
//! from Iron-Fleet; the API reads only what it needs and ignores the rest.

use crate::error::{Error, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub database_path: PathBuf,
    /// The control plane, on the compose network: `http://control-plane:8080`.
    pub control_plane_url: String,
    /// The control plane's bearer — the API's own upstream credential, never
    /// returned to or accepted from a client. Read by stage 2's upstream client.
    #[allow(dead_code)]
    pub control_plane_token: String,
}

fn required(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::Config(format!("{name} is not set")))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port = optional("PORT")
            .map(|p| {
                p.parse::<u16>()
                    .map_err(|_| Error::Config(format!("PORT must be a port number, got {p:?}")))
            })
            .transpose()?
            .unwrap_or(8100);
        Ok(Config {
            port,
            database_path: optional("DATABASE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("opus-api.db")),
            control_plane_url: required("CONTROL_PLANE_URL")?
                .trim_end_matches('/')
                .to_owned(),
            control_plane_token: required("CONTROL_PLANE_TOKEN")?,
        })
    }
}
