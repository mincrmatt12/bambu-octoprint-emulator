use std::{net::IpAddr, sync::Arc};

use config::{Config, ConfigError, File};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PrinterConfig {
    pub ip: IpAddr,
    pub serial: String,
    pub access_code: String,
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    pub api_keys: Vec<String>,
}

fn default_bind() -> IpAddr {
    IpAddr::V4("0.0.0.0".parse().unwrap())
}

fn default_port() -> u16 {
    80
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub authorization: Option<AuthConfig>,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub printer: Arc<PrinterConfig>,
    pub server: Arc<ServerConfig>,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let s = Config::builder()
            .add_source(File::with_name(&std::env::var("BOE_CONFIG_FILE").map_err(
                |_| ConfigError::Message("BOE_CONFIG_FILE not set".to_string()),
            )?))
            .build()?;

        s.try_deserialize()
    }
}
