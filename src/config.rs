//! Configuración persistente: `~/.config/spotify_cmd/config.toml` y el token
//! de la sesión (`token.json`).

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
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
    /// Si spotify_cmd suena también en este equipo (reproductor local).
    /// Sin valor, se pregunta la primera vez.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_player: Option<bool>,
    /// Configuración recién escrita por el usuario: se guarda solo cuando el
    /// inicio de sesión funciona (así un Client ID mal copiado no se queda).
    #[serde(skip)]
    pub unsaved: bool,
}

/// Valor de una variable de entorno, ignorando las vacías (`export X=`).
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Los Client ID de Spotify son 32 caracteres hexadecimales.
fn looks_like_client_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
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

/// Ruta del archivo de configuración, para mostrarla (sin crear nada).
pub fn config_path_hint() -> String {
    dirs::config_dir().map_or_else(
        || "~/.config/spotify_cmd/config.toml".to_string(),
        |dir| {
            dir.join("spotify_cmd")
                .join("config.toml")
                .display()
                .to_string()
        },
    )
}

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

/// Antigüedad a partir de la cual un temporal del token se da por abandonado.
/// Una escritura tarda milisegundos: a los 10 minutos ninguna sigue en curso.
const ORPHAN_TEMP_AGE: Duration = Duration::from_secs(10 * 60);

/// Borra los temporales del token que dejó una escritura cortada (la app
/// murió entre crear el temporal y renombrarlo). Contienen un refresh token,
/// así que no deben acumularse. Solo se borran los abandonados: uno reciente
/// puede ser de otra instancia que está escribiendo. Se llama al arrancar y
/// devuelve cuántos se borraron.
pub fn remove_orphan_temps(path: &Path) -> usize {
    remove_temps(path, ORPHAN_TEMP_AGE)
}

/// Cierra la sesión guardada (`--logout`): borra el token y todos sus
/// temporales, también los recientes. Devuelve si había algo que borrar.
pub fn delete_session(path: &Path) -> Result<bool> {
    let temps = remove_temps(path, Duration::ZERO);
    let token = match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("no se pudo borrar {}", path.display())),
    };
    Ok(token || temps > 0)
}

/// Borra los temporales del token con al menos `min_age` de antigüedad y
/// devuelve cuántos. Es limpieza: un fallo (permisos, ya no existe) se
/// ignora.
fn remove_temps(path: &Path, min_age: Duration) -> usize {
    let now = SystemTime::now();
    temp_files(path)
        .into_iter()
        .filter(|tmp| age(tmp, now) >= min_age)
        .filter(|tmp| fs::remove_file(tmp).is_ok())
        .count()
}

/// Temporales de [`temp_path`] que hay junto a `path` (solo archivos
/// normales: un enlace simbólico con ese nombre no lo creó la app).
fn temp_files(path: &Path) -> Vec<PathBuf> {
    let Some(token_name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent_dir(path)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|name| is_temp_name(name, token_name))
        })
        .map(|e| e.path())
        .collect()
}

/// Directorio que contiene `path` (`.` si es un nombre suelto).
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Si `name` tiene la forma de los temporales de [`temp_path`] para el token
/// `token_name`: `<token_name>.<pid>.<n>.tmp`.
fn is_temp_name(name: &str, token_name: &str) -> bool {
    let Some(middle) = name
        .strip_prefix(token_name)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| rest.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = middle.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(n), None) if is_number(pid) && is_number(n)
    )
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Tiempo desde la última modificación de `path`. Si no se puede saber (o
/// la fecha está en el futuro), cero: se trata como reciente.
fn age(path: &Path, now: SystemTime) -> Duration {
    fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or(Duration::ZERO)
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
        } else if let Some(id) = env_value("SPOTIFY_CLIENT_ID") {
            Config {
                client_id: id,
                redirect_uri: default_redirect_uri(),
                poll_secs: default_poll_secs(),
                local_player: None,
                unsaved: false,
            }
        } else {
            Self::ask_interactively()?
        };

        if let Some(id) = env_value("SPOTIFY_CLIENT_ID") {
            config.client_id = id;
        }
        if let Some(uri) = env_value("SPOTIFY_REDIRECT_URI") {
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

    /// Guarda en `config.toml` si se quiere el reproductor local, sin tocar
    /// el resto del archivo. Sin archivo (todo viene de variables de
    /// entorno) no se guarda: se volverá a preguntar.
    #[cfg_attr(not(feature = "local-player"), allow(dead_code))]
    pub fn save_local_player(&mut self, enabled: bool) -> Result<()> {
        self.local_player = Some(enabled);
        let path = config_path()?;
        if !path.exists() {
            return Ok(());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("no se pudo leer {}", path.display()))?;
        let mut table: toml::Table = toml::from_str(&raw)
            .with_context(|| format!("{} no es un TOML válido", path.display()))?;
        table.insert("local_player".into(), toml::Value::Boolean(enabled));
        fs::write(&path, toml::to_string_pretty(&table)?)
            .with_context(|| format!("no se pudo escribir {}", path.display()))
    }

    fn ask_interactively() -> Result<Self> {
        say!("{}\n", logo::green("=== Configuración inicial ==="));
        say!("Necesitas una app de Spotify (gratis) para conectar esta herramienta:");
        say!("  1. Entra en https://developer.spotify.com/dashboard y pulsa \"Create app\".");
        say!("  2. En \"Redirect URIs\" añade exactamente: {DEFAULT_REDIRECT_URI}");
        say!("  3. Marca \"Web API\" y guarda.");
        say!("  4. Copia el \"Client ID\" de la app y pégalo aquí.\n");
        let client_id = loop {
            say_inline!("Client ID: ");
            let mut id = String::new();
            if io::stdin().read_line(&mut id)? == 0 {
                bail!("no se introdujo ningún Client ID");
            }
            let id = id.trim().to_string();
            if id.is_empty() {
                bail!("no se introdujo ningún Client ID");
            }
            if looks_like_client_id(&id) {
                break id;
            }
            say!(
                "Eso no parece un Client ID (son 32 letras y números, p. ej. 1a2b3c…). \
                 Cópialo de nuevo desde el panel de tu app."
            );
        };
        Ok(Config {
            client_id,
            redirect_uri: default_redirect_uri(),
            poll_secs: default_poll_secs(),
            local_player: None,
            unsaved: true,
        })
    }

    /// Guarda la configuración escrita en la primera ejecución, una vez que el
    /// inicio de sesión ha funcionado.
    pub fn save_if_new(&self) -> Result<()> {
        if !self.unsaved {
            return Ok(());
        }
        self.save()?;
        let path = config_path()?;
        say!(
            "{}\n",
            logo::green(&format!("Configuración guardada en {}", path.display()))
        );
        Ok(())
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

    #[test]
    fn client_ids_are_32_hex_characters() {
        assert!(looks_like_client_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!looks_like_client_id("tu_client_id"));
        assert!(!looks_like_client_id("0123456789abcdef0123456789abcde"));
        assert!(!looks_like_client_id(
            "https://developer.spotify.com/dashboard/x"
        ));
    }

    #[test]
    fn unset_environment_variables_are_ignored() {
        assert_eq!(
            env_value("SPOTIFY_CMD_TEST_VARIABLE_THAT_DOES_NOT_EXIST"),
            None
        );
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

    /// Crea un temporal de token con `age` de antigüedad.
    fn temp_with_age(path: &Path, age: Duration) {
        fs::write(path, "refresh token a medio escribir").unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    /// Directorio con un token, un temporal abandonado, uno reciente y
    /// archivos ajenos que se parecen. Devuelve (token, abandonado, reciente,
    /// ajenos).
    fn session_with_temps(name: &str) -> (PathBuf, PathBuf, PathBuf, Vec<PathBuf>) {
        let dir = temp_dir(name);
        let path = dir.join("token.json");
        save_token(&path, &token("x")).unwrap();
        let old = dir.join("token.json.123.0.tmp");
        temp_with_age(&old, ORPHAN_TEMP_AGE + Duration::from_secs(60));
        let fresh = temp_path(&path);
        temp_with_age(&fresh, Duration::ZERO);
        let others: Vec<_> = [
            "token.json.bak",
            "otro.tmp",
            "token.json.a.0.tmp",
            "config.toml",
        ]
        .iter()
        .map(|n| dir.join(n))
        .collect();
        for other in &others {
            temp_with_age(other, ORPHAN_TEMP_AGE * 2);
        }
        (path, old, fresh, others)
    }

    /// Al arrancar se borran los temporales abandonados (llevan un refresh
    /// token), pero no uno reciente, que puede ser de otra instancia que está
    /// escribiendo, ni archivos ajenos.
    #[test]
    fn orphan_temps_are_removed_at_startup() {
        let (path, old, fresh, others) = session_with_temps("orphans");
        assert_eq!(remove_orphan_temps(&path), 1);
        assert!(!old.exists());
        assert!(fresh.exists(), "puede ser de una escritura en curso");
        assert!(path.exists() && others.iter().all(|o| o.exists()));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// `--logout` borra el token y todos sus temporales, también los
    /// recientes; los archivos ajenos se quedan.
    #[test]
    fn logout_deletes_the_token_and_every_temp() {
        let (path, old, fresh, others) = session_with_temps("logout");
        assert!(delete_session(&path).unwrap());
        assert!(!path.exists() && !old.exists() && !fresh.exists());
        assert!(others.iter().all(|o| o.exists()));
        assert!(!delete_session(&path).unwrap(), "ya no queda nada");

        // Sin token, un temporal suelto también es una sesión que borrar.
        temp_with_age(&fresh, Duration::ZERO);
        assert!(delete_session(&path).unwrap());
        assert!(!fresh.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Solo cuentan como temporales los nombres que genera [`temp_path`].
    #[test]
    fn temp_names_are_recognized() {
        let path = Path::new("/config/token.json");
        let ours = temp_path(path);
        let name = ours.file_name().unwrap().to_str().unwrap();
        assert!(is_temp_name(name, "token.json"));
        assert!(is_temp_name("token.json.1.22.tmp", "token.json"));
        for other in [
            "token.json",
            "token.json.tmp",
            "token.json.1.tmp",
            "token.json.1.2.3.tmp",
            "token.json.x.2.tmp",
            "token.json..2.tmp",
            "token.json.1.2.tmp.bak",
            "otro.json.1.2.tmp",
            "token.jsonX1.2.tmp",
        ] {
            assert!(!is_temp_name(other, "token.json"), "{other}");
        }
    }

    /// Un enlace simbólico con nombre de temporal no se borra: no lo creó la
    /// app.
    #[cfg(unix)]
    #[test]
    fn temp_cleanup_ignores_symlinks() {
        let dir = temp_dir("temp_symlink");
        let path = dir.join("token.json");
        let target = dir.join("destino");
        fs::write(&target, "ajeno").unwrap();
        let link = dir.join("token.json.1.2.tmp");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(remove_temps(&path, Duration::ZERO), 0);
        assert!(link.symlink_metadata().is_ok() && target.exists());
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
