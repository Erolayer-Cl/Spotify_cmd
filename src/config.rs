//! Configuración persistente: `~/.config/spotify_cmd/config.toml`.

use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::logo;

pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8888/callback";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Client ID de la app creada en <https://developer.spotify.com/dashboard>.
    pub client_id: String,
    /// Redirect URI registrada en la app de Spotify (debe coincidir exactamente).
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
    /// Segundos entre cada consulta del estado de reproducción.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

fn default_redirect_uri() -> String {
    DEFAULT_REDIRECT_URI.to_string()
}

fn default_poll_secs() -> u64 {
    3
}

/// Directorio donde se guardan la configuración y el token.
pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no se pudo determinar el directorio de configuración")?
        .join("spotify_cmd");
    fs::create_dir_all(&dir).with_context(|| format!("no se pudo crear {}", dir.display()))?;
    Ok(dir)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("token.json"))
}

impl Config {
    /// Carga la configuración. Si no existe, la pide por terminal y la guarda.
    /// La variable de entorno `SPOTIFY_CLIENT_ID` tiene prioridad sobre el archivo.
    pub fn load_or_create() -> Result<Self> {
        let path = config_path()?;
        let mut config = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("no se pudo leer {}", path.display()))?;
            toml::from_str::<Config>(&raw)
                .with_context(|| format!("{} no es un TOML válido", path.display()))?
        } else if let Ok(id) = std::env::var("SPOTIFY_CLIENT_ID") {
            Config {
                client_id: id,
                redirect_uri: default_redirect_uri(),
                poll_secs: default_poll_secs(),
            }
        } else {
            let config = Self::ask_interactively()?;
            config.save()?;
            println!(
                "{}\n",
                logo::green(&format!("Configuración guardada en {}", path.display()))
            );
            config
        };

        if let Ok(id) = std::env::var("SPOTIFY_CLIENT_ID") {
            config.client_id = id;
        }
        if let Ok(uri) = std::env::var("SPOTIFY_REDIRECT_URI") {
            config.redirect_uri = uri;
        }
        config.client_id = config.client_id.trim().to_string();
        if config.client_id.is_empty() {
            bail!(
                "falta `client_id` en {} (o en la variable SPOTIFY_CLIENT_ID)",
                path.display()
            );
        }
        config.poll_secs = config.poll_secs.max(1);
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("no se pudo escribir {}", path.display()))
    }

    fn ask_interactively() -> Result<Self> {
        println!("{}\n", logo::green("=== Configuración inicial ==="));
        println!("Necesitas una app de Spotify (gratis) para conectar esta herramienta:");
        println!("  1. Entra en https://developer.spotify.com/dashboard y pulsa \"Create app\".");
        println!("  2. En \"Redirect URIs\" añade exactamente: {DEFAULT_REDIRECT_URI}");
        println!("  3. Marca \"Web API\" y guarda.");
        println!("  4. Copia el \"Client ID\" de la app y pégalo aquí.\n");
        print!("Client ID: ");
        io::stdout().flush()?;
        let mut id = String::new();
        io::stdin().read_line(&mut id)?;
        let id = id.trim().to_string();
        if id.is_empty() {
            bail!("no se introdujo ningún Client ID");
        }
        Ok(Config {
            client_id: id,
            redirect_uri: default_redirect_uri(),
            poll_secs: default_poll_secs(),
        })
    }
}
