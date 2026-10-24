//! Mensajes que las tareas en segundo plano envían al bucle principal, su
//! reparto ([`App::on_msg`]) y la recuperación tras un fallo inesperado.
//!
//! Cada respuesta la atiende el módulo que lanzó la petición: el estado de
//! reproducción y las acciones, `playback`; las playlists, `library`; las
//! listas y búsquedas, `lists`; los dispositivos, `popups`; y la carátula,
//! `cover`.

use anyhow::Result;
use image::DynamicImage;
use ratatui_image::{errors::Errors, thread::ResizeResponse};

use super::{App, MainContent, Popup};
use crate::spotify::{
    Action, DeviceEntry, Playback, PlaylistEntry, SearchKind, SearchResults, TrackList,
};

/// Mensajes que llegan al bucle principal desde las tareas en segundo plano.
pub enum Msg {
    /// Estado de reproducción; `seq` es el número de la consulta que lo pidió.
    Playback {
        seq: u64,
        result: Result<Option<Playback>>,
    },
    Playlists(Result<Vec<PlaylistEntry>>),
    Tracks {
        request: u64,
        result: Result<TrackList>,
    },
    Search {
        request: u64,
        title: String,
        kind: SearchKind,
        result: Result<SearchResults>,
    },
    /// Lista de dispositivos; `request` es el número de la petición.
    Devices {
        request: u64,
        result: Result<Vec<DeviceEntry>>,
    },
    ActionDone {
        done: Option<&'static str>,
        refresh_queue: bool,
        /// La reproducción enviada, para repetirla en otro dispositivo si no
        /// había ninguno activo; `None` en las demás acciones.
        replay: Option<Box<Action>>,
        result: Result<()>,
    },
    Cover {
        url: String,
        result: Result<DynamicImage>,
    },
    /// Redimensionado de la carátula pedido para la generación `generation`
    /// (ver [`App::cover_generation`]).
    Resized {
        generation: u64,
        result: std::result::Result<ResizeResponse, Errors>,
    },
    /// Una tarea en segundo plano falló de forma inesperada (*panic*). La
    /// aplicación lo muestra y sigue funcionando; `origin` dice qué tarea
    /// era, para deshacer solo lo suyo.
    Crashed {
        origin: CrashOrigin,
        reason: String,
    },
    /// La sesión no se pudo renovar (token revocado o sin red).
    SessionProblem(String),
    /// El reproductor local terminó de conectarse (o no pudo).
    #[cfg_attr(not(feature = "local-player"), allow(dead_code))]
    LocalPlayer(Result<LocalDevice>),
}

/// El reproductor local (este equipo como dispositivo de Spotify Connect).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    /// Id del dispositivo en la Web API.
    pub id: String,
    pub name: String,
}

/// Qué tarea sufrió un *panic*: decide qué se da por perdido. Un fallo solo
/// deshace el trabajo de su propia tarea, no el de las demás.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashOrigin {
    /// Cualquier otra tarea (consultas, acciones, descargas) o la propia
    /// interfaz (al dibujar o al procesar un evento).
    Other,
    /// La carga de la lista (o búsqueda) con este identificador de petición.
    List(u64),
    /// El redimensionado de la carátula de esta generación.
    Resize(u64),
}

impl App {
    /// Aplica la respuesta de una tarea en segundo plano.
    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Playback { seq, result } => self.on_playback(seq, result),
            Msg::Playlists(result) => self.on_playlists(result),
            Msg::Tracks { request, result } => self.on_tracks(request, result),
            Msg::Search {
                request,
                title,
                kind,
                result,
            } => self.on_search(request, title, kind, result),
            Msg::Devices { request, result } => self.on_devices(request, result),
            Msg::ActionDone {
                done,
                refresh_queue,
                replay,
                result,
            } => self.on_action_done(done, refresh_queue, replay, result),
            Msg::Cover { url, result } => self.on_cover(url, result),
            Msg::Resized { generation, result } => self.on_resized(generation, result),
            Msg::Crashed { origin, reason } => self.recover_from_crash(origin, &reason),
            Msg::SessionProblem(reason) => self.on_session_problem(reason),
            Msg::LocalPlayer(result) => self.on_local_player(result),
        }
    }

    /// El reproductor local está listo (o no se pudo conectar: la app sigue
    /// como mando a distancia).
    fn on_local_player(&mut self, result: Result<LocalDevice>) {
        match result {
            Ok(device) => {
                self.set_status(
                    format!(
                        "Este equipo ya suena como «{}»: elígelo con 'd'",
                        device.name
                    ),
                    false,
                );
                self.local_device = Some(device);
            }
            Err(e) => self.set_status(format!("Reproductor local no disponible: {e:#}"), true),
        }
    }

    /// La sesión no se pudo renovar: se avisa hasta que una petición vaya bien.
    fn on_session_problem(&mut self, reason: String) {
        self.session_problem = Some(reason.clone());
        self.set_status(reason, true);
    }

    // -----------------------------------------------------------------------
    // Recuperación tras un fallo inesperado
    // -----------------------------------------------------------------------

    /// Registra un fallo inesperado de la interfaz (al dibujar o al procesar
    /// un evento) y deja el estado coherente para seguir funcionando.
    pub fn report_crash(&mut self, reason: &str) {
        self.recover_from_crash(CrashOrigin::Other, reason);
    }

    /// Registra un fallo inesperado de origen `origin`. Solo se deshace lo
    /// que ese fallo dejó a medias: un *panic* ajeno no cancela la carga de
    /// una lista ni gasta el reintento de la carátula.
    fn recover_from_crash(&mut self, origin: CrashOrigin, reason: &str) {
        // El fallo pudo ser de la consulta del estado o de la carga de las
        // playlists: se permite lanzar otra. Si la anterior sigue viva, su
        // respuesta no pisa a una más reciente (ver `on_playback`).
        self.playback_inflight = false;
        self.playlists_inflight = false;
        // También pudo ser de una acción, que ya no responderá: si no, las
        // consultas del estado quedarían bloqueadas. Si otra sigue en vuelo,
        // su respuesta invalida lo consultado mientras (`on_action_done`).
        self.actions_inflight = 0;
        if self.list_lost(origin) {
            self.give_up_list(reason);
        }
        if matches!(self.popup, Popup::Devices { devices: None, .. }) {
            self.close_popup();
        }
        // La imagen salió del protocolo para redimensionarse y se perdió con
        // el fallo: se vuelve a preparar a partir de la descargada.
        if self.resize_lost(origin) {
            self.rebuild_cover();
        }
        self.set_status(
            format!("Error interno (la aplicación sigue funcionando): {reason}"),
            true,
        );
    }

    /// La carga de lista en curso ya no puede terminar: el fallo fue de su
    /// propia tarea, o su respuesta ya había llegado y falló al procesarla.
    fn list_lost(&self, origin: CrashOrigin) -> bool {
        match origin {
            CrashOrigin::List(request) => request == self.request_id,
            _ => self.list_task.is_none() && matches!(self.main, MainContent::Loading(_)),
        }
    }

    /// Da por fallida la carga en curso de forma coherente: si su respuesta
    /// llega más tarde, se descarta en vez de mostrarse sin su título ni su
    /// origen (y reproducirse como una lista suelta). Una recarga deja ver la
    /// lista que había.
    fn give_up_list(&mut self, reason: &str) {
        self.abandon_request();
        if matches!(self.main, MainContent::Loading(_)) {
            self.main =
                MainContent::Message(format!("No se pudo completar la operación: {reason}"));
        }
    }

    /// El fallo fue del redimensionado de la carátula actual, que se perdió.
    fn resize_lost(&self, origin: CrashOrigin) -> bool {
        origin == CrashOrigin::Resize(self.cover_generation) && self.resizing_cover()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{crash, deliver, playlist, song, status, test_app};
    use crate::app::{LibraryItem, TrackSource};

    /// Un fallo ajeno (la carátula, otra petición, el dibujo) mientras carga
    /// una playlist no la cancela: su respuesta se muestra con su título.
    #[tokio::test]
    async fn unrelated_crash_keeps_loading_the_list() {
        let (mut app, _rx) = test_app();
        app.playlists = vec![playlist("Rock")];
        app.open_library(LibraryItem::Playlist(0));
        let request = app.request_id;
        let generation = app.cover_generation();
        app.on_msg(crash(
            CrashOrigin::Resize(generation),
            "fallo al redimensionar",
        ));
        app.on_msg(crash(CrashOrigin::Other, "fallo en otra petición"));
        app.on_msg(crash(
            CrashOrigin::List(request - 1),
            "fallo de una lista vieja",
        ));
        app.report_crash("al dibujar: fallo simulado");
        assert!(matches!(app.main, MainContent::Loading(_)));
        assert!(status(&app).is_some_and(|s| s.contains("Error interno")));

        deliver(&mut app, vec![song("Bohemian Rhapsody")]);
        assert!(matches!(
            &app.main,
            MainContent::Tracks { title, source: TrackSource::Context(_), .. } if title == "Rock"
        ));
    }

    /// Un *panic* de la propia carga da la lista por fallida: si su respuesta
    /// llega luego, no aparece como una lista suelta «Canciones».
    #[tokio::test]
    async fn crash_of_the_list_task_discards_its_late_answer() {
        let (mut app, _rx) = test_app();
        app.playlists = vec![playlist("Rock")];
        app.open_library(LibraryItem::Playlist(0));
        let request = app.request_id;
        app.on_msg(crash(
            CrashOrigin::List(request),
            "la API devolvió algo imposible",
        ));
        assert!(matches!(app.main, MainContent::Message(_)));
        app.on_msg(Msg::Tracks {
            request,
            result: Ok(vec![song("Bohemian Rhapsody")].into()),
        });
        assert!(matches!(app.main, MainContent::Message(_)));
        // Volver a abrirla funciona con normalidad.
        app.open_library(LibraryItem::Playlist(0));
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Ok(vec![song("Bohemian Rhapsody")].into()),
        });
        assert!(matches!(
            &app.main,
            MainContent::Tracks { title, source: TrackSource::Context(_), .. } if title == "Rock"
        ));
    }

    /// Si el fallo llega cuando la respuesta de la lista ya se recibió (al
    /// procesarla), la carga tampoco puede terminar: no se queda «Cargando…».
    #[tokio::test]
    async fn crash_while_loading_leaves_a_usable_screen() {
        let (mut app, _rx) = test_app();
        app.main = MainContent::Loading("Cargando…".into());
        app.playback_inflight = true;
        app.report_crash("boom");
        assert!(matches!(app.main, MainContent::Message(_)));
        assert!(
            !app.playback_inflight,
            "las consultas deben poder reanudarse"
        );
    }
}
