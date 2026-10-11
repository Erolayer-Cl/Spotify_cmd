//! Configuración persistente: `~/.config/spotify_cmd/config.toml` y el token
//! de la sesión (`token.json`).

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
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

/// Intentos de crear un temporal con un nombre libre al guardar el token.
const TEMP_ATTEMPTS: usize = 8;

/// Guarda el token de forma atómica y legible solo por el usuario: se escribe
/// en un archivo temporal con permisos 0600 y luego se renombra. Si algo falla
/// a medias (disco lleno, la app se cierra), el `token.json` anterior sigue
/// intacto en lugar de quedar vacío.
pub fn save_token(path: &Path, token: &Token) -> Result<()> {
    save_token_via(path, token, (0..TEMP_ATTEMPTS).map(|_| temp_path(path)))
}

/// Como [`save_token`], usando el primer temporal libre de `temps`. Solo se
/// borra el temporal creado aquí: uno ocupado es de otra escritura en curso
/// (otra instancia de la app) y no se toca.
fn save_token_via(
    path: &Path,
    token: &Token,
    temps: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    let json = serde_json::to_vec(token).context("no se pudo serializar el token")?;
    let written = create_temp(temps).and_then(|(tmp, file)| {
        let result = write_synced(file, &json).and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    });
    written.with_context(|| format!("no se pudo escribir {}", path.display()))
}

/// `token.json` → `token.json.<pid>.<n>.tmp`, en el mismo directorio (para
/// que el renombrado sea atómico). El nombre es distinto en cada escritura:
/// dos instancias, o dos guardados de la misma, no comparten temporal.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// Crea el primero de `candidates` que no exista. Si todos existen, devuelve
/// el error de «ya existe» sin borrar ninguno.
fn create_temp(candidates: impl IntoIterator<Item = PathBuf>) -> io::Result<(PathBuf, fs::File)> {
    let mut last = io::Error::new(io::ErrorKind::AlreadyExists, "sin nombre de temporal libre");
    for tmp in candidates {
        match create_private(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Crea `path` desde cero con permisos 0600. Falla si ya existe
/// (`create_new` tampoco sigue enlaces simbólicos).
fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Escribe `data` en `file` y espera a que llegue al disco.
fn write_synced(mut file: fs::File, data: &[u8]) -> io::Result<()> {
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

    /// Temporales que quedan en `dir` (deberían ser ninguno).
    fn leftover_temps(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "tmp"))
            .collect()
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
        assert!(leftover_temps(&dir).is_empty());
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
        // El temporal no se puede crear: ocupa su sitio un directorio, que
        // además no se borra (podría ser de otra escritura).
        let busy = dir.join("ocupado.tmp");
        fs::create_dir(&busy).unwrap();
        assert!(save_token_via(&path, &token("nuevo"), [busy.clone()]).is_err());
        assert!(busy.is_dir());
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
        assert!(leftover_temps(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Cada escritura usa su propio temporal, en el mismo directorio.
    #[test]
    fn temp_names_are_unique_per_write() {
        let path = Path::new("/config/token.json");
        let (a, b) = (temp_path(path), temp_path(path));
        assert_ne!(a, b);
        assert_eq!(a.parent(), path.parent());
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        let pid = std::process::id().to_string();
        assert!(name.starts_with(&format!("token.json.{pid}.")));
        assert!(name.ends_with(".tmp"));
    }

    /// El temporal de otra instancia que está escribiendo no se borra ni se
    /// usa: se elige el siguiente nombre libre.
    #[test]
    fn temp_of_another_instance_is_left_alone() {
        let dir = temp_dir("other_instance");
        let path = dir.join("token.json");
        let theirs = dir.join("token.json.1.0.tmp");
        fs::write(&theirs, "a medio escribir").unwrap();
        let ours = dir.join("token.json.2.0.tmp");
        save_token_via(&path, &token("nuevo"), [theirs.clone(), ours.clone()]).unwrap();

        assert_eq!(fs::read_to_string(&theirs).unwrap(), "a medio escribir");
        assert!(!ours.exists(), "el temporal propio se renombró");
        let loaded = load_token(&path, &HashSet::new()).unwrap();
        assert_eq!(loaded.access_token, "nuevo");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Varias escrituras a la vez (dos instancias de la app) no se pisan el
    /// temporal: todas terminan bien y `token.json` siempre es válido.
    #[test]
    fn concurrent_saves_never_publish_a_partial_token() {
        let dir = temp_dir("concurrent");
        let path = dir.join("token.json");
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for j in 0..25 {
                        save_token(&path, &token(&format!("{i}-{j}"))).unwrap();
                        assert!(load_token(&path, &HashSet::new()).is_some());
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(load_token(&path, &HashSet::new()).is_some());
        assert!(leftover_temps(&dir).is_empty());
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
