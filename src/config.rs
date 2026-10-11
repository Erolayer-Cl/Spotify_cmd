//! Configuración persistente: `~/.config/spotify_cmd/config.toml` y el token
//! de la sesión (`token.json`).

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rspotify::Token;
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

/// Directorio donde se guardan la configuración y el token. Solo es accesible
/// para el usuario, porque el token permite controlar su cuenta.
pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no se pudo determinar el directorio de configuración")?
        .join("spotify_cmd");
    create_private_dir(&dir).with_context(|| format!("no se pudo crear {}", dir.display()))?;
    Ok(dir)
}

/// Crea el directorio (y los que falten por encima) con permisos 0700. Si ya
/// existía con permisos más abiertos (versiones anteriores), los restringe.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    restrict_to_owner(dir);
    Ok(())
}

/// Quita los permisos de grupo y de otros usuarios. Es una medida extra: si no
/// se puede (sistema de archivos de solo lectura, otro dueño), se sigue igual.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o700));
    }
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("token.json"))
}

/// Lee el token guardado. Devuelve `None` si no existe, está dañado o no
/// incluye todos los permisos (`scopes`) que necesita la app.
///
/// De paso corrige los permisos de un `token.json` antiguo escrito como 0644.
pub fn load_token(path: &Path, required_scopes: &HashSet<String>) -> Option<Token> {
    restrict_to_owner(path);
    let token = Token::from_cache(path).ok()?;
    required_scopes.is_subset(&token.scopes).then_some(token)
}

/// Guarda el token de forma atómica y legible solo por el usuario: se escribe
/// en un archivo temporal con permisos 0600 y luego se renombra. Si algo falla
/// a medias (disco lleno, la app se cierra), el `token.json` anterior sigue
/// intacto en lugar de quedar vacío.
pub fn save_token(path: &Path, token: &Token) -> Result<()> {
    let json = serde_json::to_vec(token).context("no se pudo serializar el token")?;
    let tmp = temp_path(path);
    let result = write_private(&tmp, &json).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("no se pudo escribir {}", path.display()))
}

/// `token.json` → `token.json.tmp`, en el mismo directorio (para que el
/// renombrado sea atómico).
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Crea `path` desde cero con permisos 0600 y escribe `data` hasta el disco.
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    // Un temporal de un intento anterior podría tener otros permisos: se
    // borra y se crea de nuevo (`create_new` tampoco sigue enlaces simbólicos).
    let _ = fs::remove_file(path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
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
            say!(
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
        say!("{}\n", logo::green("=== Configuración inicial ==="));
        say!("Necesitas una app de Spotify (gratis) para conectar esta herramienta:");
        say!("  1. Entra en https://developer.spotify.com/dashboard y pulsa \"Create app\".");
        say!("  2. En \"Redirect URIs\" añade exactamente: {DEFAULT_REDIRECT_URI}");
        say!("  3. Marca \"Web API\" y guarda.");
        say!("  4. Copia el \"Client ID\" de la app y pégalo aquí.\n");
        say_inline!("Client ID: ");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Directorio temporal propio de cada prueba (se vacía al empezar).
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spotify_cmd_test_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scopes(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn token(access: &str) -> Token {
        Token {
            access_token: access.into(),
            refresh_token: Some("refresco".into()),
            scopes: scopes(&["a", "b"]),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn saved_token_is_read_back() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("token.json");
        save_token(&path, &token("uno")).unwrap();
        save_token(&path, &token("dos")).unwrap();

        let loaded = load_token(&path, &scopes(&["a"])).unwrap();
        assert_eq!(loaded.access_token, "dos");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresco"));
        // Sin todos los permisos que pide la app, hay que volver a entrar.
        assert!(load_token(&path, &scopes(&["a", "c"])).is_none());
        // No quedan temporales.
        assert!(!temp_path(&path).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_damaged_token_is_ignored() {
        let dir = temp_dir("damaged");
        let path = dir.join("token.json");
        assert!(load_token(&path, &HashSet::new()).is_none());
        fs::write(&path, "").unwrap();
        assert!(load_token(&path, &HashSet::new()).is_none());
        fs::write(&path, "{\"access_token\":").unwrap();
        assert!(load_token(&path, &HashSet::new()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Si no se puede escribir el token nuevo, el anterior sigue intacto (la
    /// escritura no trunca `token.json` antes de tener el nuevo completo).
    #[test]
    fn failed_save_keeps_previous_token() {
        let dir = temp_dir("failed_save");
        let path = dir.join("token.json");
        save_token(&path, &token("viejo")).unwrap();
        // El temporal no se puede crear: ocupa su sitio un directorio.
        fs::create_dir(temp_path(&path)).unwrap();
        assert!(save_token(&path, &token("nuevo")).is_err());
        let loaded = load_token(&path, &HashSet::new()).unwrap();
        assert_eq!(loaded.access_token, "viejo");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `token.json` no se puede reemplazar (es un directorio): error y sin
    /// temporales olvidados.
    #[test]
    fn save_error_is_reported_and_cleaned_up() {
        let dir = temp_dir("save_error");
        let path = dir.join("token.json");
        fs::create_dir(&path).unwrap();
        assert!(save_token(&path, &token("x")).is_err());
        assert!(!temp_path(&path).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn token_is_private_to_the_user() {
        let dir = temp_dir("private_token");
        let path = dir.join("token.json");
        save_token(&path, &token("x")).unwrap();
        assert_eq!(mode(&path), 0o600);

        // Un token antiguo, legible por todos, se corrige al leerlo.
        let old = dir.join("old.json");
        fs::write(&old, serde_json::to_string(&token("y")).unwrap()).unwrap();
        fs::set_permissions(&old, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        assert!(load_token(&old, &HashSet::new()).is_some());
        assert_eq!(mode(&old), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn config_dir_is_private_to_the_user() {
        let dir = temp_dir("private_dir");
        let new = dir.join("nuevo").join("spotify_cmd");
        create_private_dir(&new).unwrap();
        assert_eq!(mode(&new), 0o700);

        // Directorio creado por una versión anterior con 0755.
        let old = dir.join("viejo");
        fs::create_dir(&old).unwrap();
        fs::set_permissions(&old, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        create_private_dir(&old).unwrap();
        assert_eq!(mode(&old), 0o700);
        let _ = fs::remove_dir_all(&dir);
    }
}
