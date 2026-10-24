//! Reproductor local con librespot: spotify_cmd es también un dispositivo de
//! Spotify Connect que suena en este equipo, así no hace falta tener Spotify
//! abierto en ningún otro sitio.
//!
//! Se prepara antes de la interfaz ([`prepare`]: salida de audio y
//! credenciales) y se conecta en segundo plano ([`Prepared::start`]). Si algo
//! falla, la app sigue como sin él: un mando a distancia de otros
//! dispositivos.

use std::{
    hash::{BuildHasher, RandomState},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{
    Error as LibrespotError, authentication::Credentials, cache::Cache, config::DeviceType,
    config::SessionConfig, error::ErrorKind, session::Session,
};
use librespot_playback::{
    audio_backend::{self, Sink, SinkBuilder, SinkError, SinkResult},
    config::{AudioFormat, Bitrate, PlayerConfig},
    convert::Converter,
    decoder::AudioPacket,
    mixer::{self, MixerConfig},
    player::Player,
};
use tokio::task::JoinHandle;

/// Client ID con el que se identifica librespot (el de las apps de
/// escritorio de Spotify): reproducir necesita el permiso `streaming`, que
/// las apps creadas en el panel de desarrolladores no pueden pedir.
pub const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
/// Redirect URI registrada para ese Client ID.
pub const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";
/// Nombre del dispositivo en Spotify Connect.
pub const DEVICE_NAME: &str = "spotify_cmd";
/// Tiempo máximo para conectar con Spotify y registrar el dispositivo.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Lo necesario para conectar el reproductor, comprobado antes de la
/// interfaz.
pub struct Prepared {
    dir: PathBuf,
    credentials: Credentials,
    /// Las credenciales venían guardadas de otra vez (si Spotify las
    /// rechaza, se borran para pedirlas de nuevo).
    cached: bool,
    device_id: String,
}

/// El dispositivo local, ya visible en Spotify Connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    /// Id del dispositivo en la Web API (para reproducir en él).
    pub id: String,
    pub name: String,
}

/// Reproductor en marcha. Al salir hay que llamar a [`LocalPlayer::shutdown`].
pub struct LocalPlayer {
    spirc: Spirc,
    task: JoinHandle<()>,
}

/// Si hay una salida de audio que usar. Sin ella, el reproductor no sirve
/// (y abrirla haría fallar a librespot). No basta con que exista: en Linux,
/// ALSA ofrece su dispositivo «default» aunque no haya tarjeta de sonido;
/// pedir su formato lo abre de verdad.
pub fn has_audio_output() -> bool {
    use cpal::traits::{DeviceTrait, HostTrait};
    catch_unwind(|| {
        cpal::default_host()
            .default_output_device()
            .is_some_and(|device| device.default_output_config().is_ok())
    })
    .unwrap_or(false)
}

/// Directorio del reproductor local dentro de la configuración (privado del
/// usuario: guarda credenciales).
pub fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("librespot")
}

/// Prepara el reproductor antes de la interfaz: credenciales guardadas o,
/// la primera vez, autorización en el navegador con `login`, que recibe el
/// Client ID y la Redirect URI y devuelve un token de acceso.
pub async fn prepare<F, Fut>(dir: PathBuf, login: F) -> Result<Prepared>
where
    F: FnOnce(&'static str, &'static str) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let cache = open_cache(&dir)?;
    let (credentials, cached) = match cache.credentials() {
        Some(credentials) => (credentials, true),
        None => {
            let token = login(CLIENT_ID, REDIRECT_URI).await?;
            (Credentials::with_access_token(token), false)
        }
    };
    let device_id = device_id(&dir);
    Ok(Prepared {
        dir,
        credentials,
        cached,
        device_id,
    })
}

impl Prepared {
    /// Conecta con Spotify y registra el dispositivo (tarda un segundo o
    /// dos: se hace en segundo plano, con la interfaz ya abierta).
    pub async fn start(self) -> Result<(LocalPlayer, LocalDevice)> {
        let cache = open_cache(&self.dir)?;
        let session_config = SessionConfig {
            device_id: self.device_id.clone(),
            ..SessionConfig::default()
        };
        let session = Session::new(session_config, Some(cache));
        let mixer = mixer::find(None).context("librespot no tiene mezclador")?;
        let mixer = mixer(MixerConfig::default()).map_err(librespot)?;
        let backend = audio_backend::find(None).context("librespot no tiene salida de audio")?;
        let player_config = PlayerConfig {
            bitrate: Bitrate::Bitrate320,
            ..PlayerConfig::default()
        };
        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || Box::new(GuardedSink::open(backend)),
        );
        let connect_config = ConnectConfig {
            name: DEVICE_NAME.into(),
            device_type: DeviceType::Computer,
            ..ConnectConfig::default()
        };
        let connect = Spirc::new(
            connect_config,
            session.clone(),
            self.credentials,
            player,
            mixer,
        );
        let (spirc, task) = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(started)) => started,
            Ok(Err(e)) => {
                // Credenciales guardadas que Spotify ya no acepta (contraseña
                // cambiada, acceso retirado): se borran para pedirlas otra vez.
                if self.cached && e.kind == ErrorKind::PermissionDenied {
                    let _ = std::fs::remove_file(self.dir.join("credentials.json"));
                    return Err(anyhow!(
                        "Spotify rechazó la sesión guardada del reproductor local \
                         (se pedirá de nuevo al volver a abrir)"
                    ));
                }
                return Err(librespot(e));
            }
            Err(_) => return Err(anyhow!("Spotify no respondió a tiempo")),
        };
        let task = tokio::spawn(task);
        let device = LocalDevice {
            id: session.device_id().to_string(),
            name: DEVICE_NAME.into(),
        };
        Ok((LocalPlayer { spirc, task }, device))
    }
}

impl LocalPlayer {
    /// Para la reproducción local y desconecta el dispositivo, esperando
    /// como mucho `max`.
    pub async fn shutdown(self, max: Duration) {
        if self.spirc.shutdown().is_ok() {
            let _ = tokio::time::timeout(max, self.task).await;
        }
    }
}

/// Caché de librespot (credenciales y volumen; sin guardar audio).
fn open_cache(dir: &Path) -> Result<Cache> {
    Cache::new(Some(dir), Some(dir), None, None).map_err(librespot)
}

/// Id estable del dispositivo: si cambiara en cada arranque, Spotify vería
/// un dispositivo nuevo cada vez. Se guarda en `dir/device_id`.
fn device_id(dir: &Path) -> String {
    let path = dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit()) {
            return id.to_string();
        }
    }
    let id = random_hex();
    let _ = std::fs::write(&path, &id);
    id
}

/// 40 caracteres hexadecimales al azar (como los ids de Spotify Connect).
fn random_hex() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let part = |n: u32| RandomState::new().hash_one((now, std::process::id(), n));
    format!("{:016x}{:016x}{:08x}", part(0), part(1), part(2) as u32)
}

/// Error de librespot como `anyhow`.
fn librespot(e: LibrespotError) -> anyhow::Error {
    anyhow!("{e}")
}

/// Salida de audio protegida. librespot termina el proceso (`exit(1)`) si
/// parar la salida falla, y abrirla sin dispositivo es un *panic*: aquí un
/// fallo al parar se ignora, y si la salida no se pudo abrir (o falla con
/// un *panic*), cada intento de sonar devuelve un error y librespot pausa,
/// sin cerrar la app.
struct GuardedSink(Option<Box<dyn Sink>>);

impl GuardedSink {
    fn open(backend: SinkBuilder) -> Self {
        GuardedSink(catch_unwind(|| backend(None, AudioFormat::default())).ok())
    }

    /// Ejecuta `f` sobre la salida; un *panic* la da por perdida.
    fn with_sink(
        &mut self,
        f: impl FnOnce(&mut Box<dyn Sink>) -> SinkResult<()>,
    ) -> SinkResult<()> {
        let Some(sink) = self.0.as_mut() else {
            return Err(no_output());
        };
        match catch_unwind(AssertUnwindSafe(|| f(sink))) {
            Ok(result) => result,
            Err(_) => {
                self.0 = None;
                Err(no_output())
            }
        }
    }
}

impl Sink for GuardedSink {
    fn start(&mut self) -> SinkResult<()> {
        self.with_sink(|sink| sink.start())
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Nunca un error: librespot cerraría la app entera.
        let _ = self.with_sink(|sink| sink.stop());
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        self.with_sink(|sink| sink.write(packet, converter))
    }
}

fn no_output() -> SinkError {
    SinkError::NotConnected("no hay salida de audio".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El id del dispositivo se crea una vez y se conserva entre arranques;
    /// uno dañado se sustituye.
    #[test]
    fn device_id_is_stable() {
        let dir =
            std::env::temp_dir().join(format!("spotify_cmd_test_{}_device", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("device_id"));
        let first = device_id(&dir);
        assert_eq!(first.len(), 40);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(device_id(&dir), first);
        std::fs::write(dir.join("device_id"), "no es un id\n").unwrap();
        let replaced = device_id(&dir);
        assert_ne!(replaced, first);
        assert_eq!(device_id(&dir), replaced);
        assert_ne!(random_hex(), random_hex());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Buscar la salida de audio no se cuelga ni falla, haya o no.
    #[test]
    fn audio_output_check_is_quick() {
        let start = std::time::Instant::now();
        let _ = has_audio_output();
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// Sin poder hablar con Spotify (red cortada, credenciales falsas), la
    /// conexión falla con un error y con tope de tiempo, sin colgarse
    /// (cargo test connect_failure -- --ignored --nocapture).
    #[tokio::test]
    #[ignore]
    async fn connect_failure_is_an_error_not_a_hang() {
        let dir = std::env::temp_dir().join(format!("spotify_cmd_test_{}_lp", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prepared = Prepared {
            device_id: device_id(&dir),
            dir: dir.clone(),
            credentials: Credentials::with_access_token("falso"),
            cached: false,
        };
        let start = std::time::Instant::now();
        let result = prepared.start().await;
        println!("{:?} en {:?}", result.as_ref().err(), start.elapsed());
        assert!(result.is_err());
        assert!(start.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sin salida de audio, la salida protegida responde con errores (y
    /// librespot pausa) en vez de un *panic* o de cerrar la app.
    #[test]
    fn guarded_sink_never_panics_or_fails_to_stop() {
        fn broken(_: Option<String>, _: AudioFormat) -> Box<dyn Sink> {
            panic!("no hay dispositivo de audio")
        }
        let mut sink = GuardedSink::open(broken);
        assert!(sink.start().is_err());
        assert!(sink.stop().is_ok());

        struct Panicky;
        impl Sink for Panicky {
            fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
                panic!("fallo del controlador")
            }
            fn stop(&mut self) -> SinkResult<()> {
                Err(SinkError::StateChange("no se pudo parar".into()))
            }
        }
        fn panicky(_: Option<String>, _: AudioFormat) -> Box<dyn Sink> {
            Box::new(Panicky)
        }
        let mut sink = GuardedSink::open(panicky);
        assert!(sink.start().is_ok());
        assert!(sink.stop().is_ok(), "parar nunca falla");
        let mut converter = Converter::new(None);
        assert!(
            sink.write(AudioPacket::Samples(vec![0.0; 4]), &mut converter)
                .is_err()
        );
        assert!(
            sink.0.is_none(),
            "tras el panic, la salida se da por perdida"
        );
    }
}
