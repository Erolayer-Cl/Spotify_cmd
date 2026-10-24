//! Puesta en marcha del reproductor local desde el arranque: una fachada
//! sobre [`crate::local_player`] para que `main` sea igual con o sin la
//! función `local-player` (sin ella, todo aquí no hace nada).

#[cfg(feature = "local-player")]
mod enabled {
    use std::{
        io,
        panic::AssertUnwindSafe,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use futures::FutureExt;
    use tokio::sync::mpsc::UnboundedSender;

    use crate::app::{LocalDevice, Msg};
    use crate::config::{self, Config};
    use crate::local_player::{self, LocalPlayer, Prepared};
    use crate::{Args, auth, terminal};

    /// Tiempo máximo que se espera, al salir, a que el reproductor se pare.
    const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

    /// Reproductor local preparado antes de la interfaz (o nada, si no se
    /// quiere o no se puede usar).
    pub struct Setup(Option<Prepared>);

    /// Reproductor local conectándose o en marcha.
    pub struct Running(Arc<Mutex<Option<LocalPlayer>>>);

    /// Prepara el reproductor local si procede: no se pidió lo contrario, hay
    /// salida de audio y hay credenciales (guardadas o, la primera vez, tras
    /// preguntar y autorizar en el navegador). Si algo falla se avisa y la
    /// app sigue como mando a distancia.
    pub async fn prepare(cfg: &mut Config, args: &Args) -> Setup {
        // Con la API simulada de las pruebas no hay Spotify de verdad.
        let testing = std::env::var_os("SPOTIFY_CMD_API_URL").is_some();
        if args.no_local_player || cfg.local_player == Some(false) || testing {
            return Setup(None);
        }
        let Ok(config_dir) = config::config_dir() else {
            return Setup(None);
        };
        // ALSA escribe sus quejas en la salida de errores si no hay tarjeta
        // de sonido: van al registro, no a la consola.
        let _ = terminal::redirect_stderr(&log_path(&config_dir));
        let audio = local_player::has_audio_output();
        terminal::restore_stderr();
        if !audio {
            say!("Sin salida de audio: spotify_cmd controlará tus otros dispositivos.");
            return Setup(None);
        }
        let dir = local_player::dir(&config_dir);
        let authorized = dir.join("credentials.json").exists();
        if !authorized && cfg.local_player.is_none() {
            let wanted = ask_yes(
                "\n¿Quieres que spotify_cmd suene también en este equipo, sin abrir Spotify \
                 en otro sitio? [S/n] ",
            );
            if let Err(e) = cfg.save_local_player(wanted) {
                say_err!("Aviso: no se pudo guardar la respuesta ({e:#})");
            }
            if !wanted {
                say!(
                    "De acuerdo: controlará tus otros dispositivos. Para cambiarlo, pon \
                     `local_player = true` en {}.",
                    config::config_path_hint()
                );
                return Setup(None);
            }
        }
        let manual = args.manual_auth;
        let login =
            |client_id, redirect_uri| auth::streaming_token(client_id, redirect_uri, manual);
        match local_player::prepare(dir, login).await {
            Ok(prepared) => Setup(Some(prepared)),
            Err(e) => {
                say_err!(
                    "No se pudo preparar el reproductor local ({e:#}); spotify_cmd \
                     controlará tus otros dispositivos."
                );
                Setup(None)
            }
        }
    }

    /// Registro donde va la salida de errores mientras no debe verse.
    fn log_path(config_dir: &std::path::Path) -> std::path::PathBuf {
        config_dir.join("spotify_cmd.log")
    }

    /// Pregunta sí o no por la terminal; Enter (o cualquier cosa que no sea
    /// «n» o «no») es sí.
    fn ask_yes(question: &str) -> bool {
        say_inline!("{question}");
        let mut answer = String::new();
        match io::stdin().read_line(&mut answer) {
            Ok(0) | Err(_) => false,
            Ok(_) => !matches!(answer.trim().to_lowercase().as_str(), "n" | "no"),
        }
    }

    impl Setup {
        /// Con el reproductor local, los avisos que las bibliotecas de audio
        /// escriben en la salida de errores van a `spotify_cmd.log`: en
        /// pantalla romperían la interfaz.
        pub fn redirect_stderr(&self) {
            if self.0.is_some()
                && let Ok(dir) = config::config_dir()
            {
                let _ = terminal::redirect_stderr(&log_path(&dir));
            }
        }

        /// Conecta el reproductor en segundo plano; la app recibe
        /// [`Msg::LocalPlayer`] cuando está listo (o si falla).
        pub fn start(self, tx: UnboundedSender<Msg>) -> Running {
            let slot = Arc::new(Mutex::new(None));
            if let Some(prepared) = self.0 {
                let keep = slot.clone();
                tokio::spawn(async move {
                    let result = match AssertUnwindSafe(prepared.start()).catch_unwind().await {
                        Ok(Ok((player, device))) => {
                            if let Ok(mut slot) = keep.lock() {
                                *slot = Some(player);
                            }
                            Ok(LocalDevice {
                                id: device.id,
                                name: device.name,
                            })
                        }
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(anyhow::anyhow!("fallo interno al conectar")),
                    };
                    let _ = tx.send(Msg::LocalPlayer(result));
                });
            }
            Running(slot)
        }
    }

    impl Running {
        /// Para el reproductor y desconecta el dispositivo (con tope).
        pub async fn shutdown(self) {
            let player = self.0.lock().ok().and_then(|mut slot| slot.take());
            if let Some(player) = player {
                player.shutdown(SHUTDOWN_GRACE).await;
            }
        }
    }
}

#[cfg(not(feature = "local-player"))]
mod disabled {
    use tokio::sync::mpsc::UnboundedSender;

    use crate::Args;
    use crate::app::Msg;
    use crate::config::Config;

    /// Sin la función `local-player` no hay reproductor local.
    pub struct Setup;
    pub struct Running;

    pub async fn prepare(_cfg: &mut Config, _args: &Args) -> Setup {
        Setup
    }

    impl Setup {
        pub fn redirect_stderr(&self) {}
        pub fn start(self, _tx: UnboundedSender<Msg>) -> Running {
            Running
        }
    }

    impl Running {
        pub async fn shutdown(self) {}
    }
}

#[cfg(not(feature = "local-player"))]
pub use disabled::*;
#[cfg(feature = "local-player")]
pub use enabled::*;
