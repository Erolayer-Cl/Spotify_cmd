//! Estado de la aplicación: la estructura [`App`], su creación, el arranque
//! y los accesores que usa la interfaz para dibujar.
//!
//! El comportamiento se reparte en submódulos, uno por responsabilidad:
//!
//! - `messages`: mensajes de las tareas en segundo plano, su reparto y la
//!   recuperación tras un fallo inesperado.
//! - `tasks`: lanzamiento de tareas que siempre responden, aunque fallen.
//! - `commands`: teclado y órdenes de la pantalla principal.
//! - `popups`: ventanas emergentes (buscador y dispositivos).
//! - `playback`: controles de reproducción, agrupado de teclas y sondeo.
//! - `library`: barra lateral (entradas fijas y playlists).
//! - `lists`: carga y presentación de la lista principal.
//! - `selection`: acciones sobre la fila seleccionada de la lista.
//! - `cover`: descarga y preparación de la carátula.
//!
//! Cada módulo atiende también las respuestas de las peticiones que lanza.

mod commands;
mod cover;
mod library;
mod lists;
mod messages;
mod playback;
mod popups;
mod selection;
mod tasks;
#[cfg(test)]
pub mod tests;

use std::time::{Duration, Instant};

use image::DynamicImage;
use ratatui::widgets::{ListState, TableState};
use ratatui_image::{
    picker::Picker,
    thread::{ResizeRequest, ThreadProtocol},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;

use crate::spotify::{Action, Client, DeviceEntry, Playback, PlaylistEntry, SearchKind};

pub use library::{FIXED_LIBRARY, LibraryItem};
pub use lists::{MainContent, TrackSource};
pub use messages::{CrashOrigin, LocalDevice, Msg};
pub use playback::repeat_label;
pub use tasks::{spawn_blocking_guarded, spawn_guarded};

use lists::PendingList;
use playback::Debounced;

/// Tiempo que un aviso permanece visible en la barra de estado.
const STATUS_TTL: Duration = Duration::from_secs(6);
/// Spotify tarda un poco en reflejar los cambios (estado, cola...).
const SETTLE: Duration = Duration::from_millis(600);

/// Panel que recibe las teclas de movimiento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Library,
    Main,
}

/// Ventana emergente abierta sobre la pantalla principal.
pub enum Popup {
    None,
    Search,
    Devices {
        devices: Option<Vec<DeviceEntry>>,
        state: ListState,
    },
    Help,
}

/// Aviso de la barra de estado.
pub struct Status {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

pub struct App {
    pub spotify: Client,
    http: reqwest::Client,
    tx: UnboundedSender<Msg>,
    pub rx: UnboundedReceiver<Msg>,
    poll_every: Duration,

    pub should_quit: bool,
    pub focus: Focus,
    pub popup: Popup,
    /// Hay que repintar la pantalla entera antes del siguiente dibujado (ver
    /// [`App::take_full_redraw`]).
    full_redraw: bool,
    pub big_cover: bool,

    pub playback: Option<Playback>,
    playback_inflight: bool,
    /// Número de la última consulta del estado de reproducción lanzada.
    poll_seq: u64,
    /// Las respuestas de consultas con un número menor están obsoletas: se
    /// lanzaron antes de que respondiera la última acción o ya se aplicó una
    /// más reciente.
    fresh_poll: u64,
    /// Última consulta del estado de reproducción (`None` = nunca).
    last_poll: Option<Instant>,
    /// Consulta del estado programada tras una acción, cuando Spotify ya la
    /// refleje. Mientras esté pendiente no se hacen consultas periódicas.
    refresh_at: Option<Instant>,
    /// Acciones enviadas a Spotify que aún no han respondido. Mientras haya
    /// alguna no se consulta el estado: la respuesta aún no las reflejaría y
    /// desharía el cambio optimista.
    actions_inflight: u32,
    /// Fila de acciones (ver `tasks::spawn_in_order`); se crea al primer uso.
    actions: Option<UnboundedSender<tasks::ActionJob>>,
    /// Reproducción que falló por no haber dispositivo activo: se repite en
    /// el que se elija en la ventana de dispositivos (abierta para eso).
    pending_play: Option<Action>,
    /// Este equipo como dispositivo, si el reproductor local está en marcha.
    pub local_device: Option<LocalDevice>,

    pub playlists: Vec<PlaylistEntry>,
    /// Hay una carga de las playlists en curso.
    playlists_inflight: bool,
    pub library_state: ListState,
    pub active_library: Option<LibraryItem>,

    pub main: MainContent,
    pub main_state: TableState,
    request_id: u64,
    /// Lista que se está cargando.
    pending_list: Option<PendingList>,
    /// Recarga programada de la lista visible (la cola tras saltar de
    /// canción, por ejemplo).
    list_reload_at: Option<Instant>,
    /// Tarea de la lista (o búsqueda) que se está cargando, para cancelarla
    /// si se pide otra antes de que termine.
    list_task: Option<AbortHandle>,
    /// Tras un 429 de Spotify, no se consulta hasta este instante.
    backoff_until: Option<Instant>,

    pub search_input: String,
    /// La búsqueda anterior se muestra atenuada y lo que se escriba la
    /// sustituye (así no se pega al texto viejo).
    pub search_fresh: bool,
    pub search_kind: SearchKind,
    /// Número de la última petición de la lista de dispositivos: la
    /// respuesta de una anterior (la ventana se cerró y se volvió a abrir)
    /// se descarta.
    devices_request: u64,

    pub picker: Picker,
    pub cover: ThreadProtocol,
    cover_url: Option<String>,
    /// Carátula descargada: permite volver a prepararla si falla su dibujo.
    cover_image: Option<DynamicImage>,
    /// Veces seguidas que se ha vuelto a preparar la carátula actual (vuelve
    /// a 0 cuando un redimensionado sale bien).
    cover_rebuilds: u8,
    /// Con Kitty, la carátula se envía a la terminal una sola vez, dentro de
    /// su primera celda: si se envió con una ventana abierta encima, la
    /// ventana pudo tapar esa celda y la imagen no llegó. Al cerrarla se
    /// vuelve a enviar.
    cover_sent_under_popup: bool,
    /// Cambia cada vez que se sustituye (o se quita) la carátula preparada:
    /// identifica los redimensionados ya obsoletos.
    cover_generation: u64,
    /// Tras fallar la descarga de la carátula, cuándo se puede reintentar.
    cover_retry_at: Option<Instant>,

    pub status: Option<Status>,

    pending_seek: Debounced<u64>,
    pending_volume: Debounced<u8>,
    session_problem: Option<String>,
}

impl App {
    pub fn new(
        spotify: Client,
        picker: Picker,
        resize_tx: UnboundedSender<ResizeRequest>,
        poll_secs: u64,
    ) -> Self {
        let (tx, rx) = unbounded_channel();
        let mut library_state = ListState::default();
        library_state.select(Some(0));
        Self {
            spotify,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            tx,
            rx,
            poll_every: Duration::from_secs(poll_secs),
            should_quit: false,
            focus: Focus::Library,
            popup: Popup::None,
            full_redraw: false,
            big_cover: false,
            playback: None,
            playback_inflight: false,
            poll_seq: 0,
            fresh_poll: 0,
            last_poll: None,
            refresh_at: None,
            actions_inflight: 0,
            actions: None,
            pending_play: None,
            local_device: None,
            playlists: Vec::new(),
            playlists_inflight: false,
            library_state,
            active_library: None,
            main: MainContent::Message("Elige una lista en la biblioteca (Enter).".into()),
            main_state: TableState::default(),
            request_id: 0,
            pending_list: None,
            list_reload_at: None,
            list_task: None,
            backoff_until: None,
            search_input: String::new(),
            search_fresh: false,
            search_kind: SearchKind::Tracks,
            devices_request: 0,
            picker,
            cover: ThreadProtocol::new(resize_tx, None),
            cover_url: None,
            cover_image: None,
            cover_rebuilds: 0,
            cover_sent_under_popup: false,
            cover_generation: 0,
            cover_retry_at: None,
            status: None,
            pending_seek: Debounced::default(),
            pending_volume: Debounced::default(),
            session_problem: None,
        }
    }

    /// Peticiones iniciales al arrancar.
    pub fn start(&mut self) {
        self.load_playlists();
        self.poll_playback();
        self.open_library(LibraryItem::Liked);
        self.focus = Focus::Library;
    }

    /// Canal por el que las tareas en segundo plano responden al bucle
    /// principal.
    pub fn sender(&self) -> UnboundedSender<Msg> {
        self.tx.clone()
    }

    // -----------------------------------------------------------------------
    // Barra de estado
    // -----------------------------------------------------------------------

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    /// Muestra un error en la barra de estado.
    pub fn show_error(&mut self, text: String) {
        self.set_status(text, true);
    }

    /// Último problema de sesión pendiente (se borra cuando una petición va bien).
    pub fn session_problem(&self) -> Option<&str> {
        self.session_problem.as_deref()
    }

    /// Aviso que aún se debe ver (los más antiguos caducan).
    pub fn visible_status(&self) -> Option<&Status> {
        self.status.as_ref().filter(|s| s.at.elapsed() < STATUS_TTL)
    }
}
