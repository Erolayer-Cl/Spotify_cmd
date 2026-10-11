//! Estado de la aplicación y manejo de eventos (teclado, respuestas de la API).

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::FutureExt;
use image::DynamicImage;
use ratatui::widgets::{ListState, TableState};
use ratatui_image::{
    errors::Errors,
    picker::Picker,
    thread::{ResizeRequest, ResizeResponse, ThreadProtocol},
};
use rspotify::model::RepeatState;
use rspotify::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::keymap::{Command, command_for, step_index};
use crate::spotify::{
    self, Action, ApiError, Client, ContextEntry, DeviceEntry, Playback, PlaylistEntry, SearchKind,
    SearchResults, TrackEntry,
};
use crate::terminal;

const STATUS_TTL: Duration = Duration::from_secs(6);
const SEEK_STEP_MS: u64 = 10_000;
const VOLUME_STEP: i32 = 5;
/// Sin red, el estado se vuelve a consultar cada 10 s en lugar de cada 3 s.
const OFFLINE_RETRY: Duration = Duration::from_secs(10);

/// Mensajes que llegan al bucle principal desde las tareas en segundo plano.
pub enum Msg {
    Playback(Result<Option<Playback>>),
    Playlists(Result<Vec<PlaylistEntry>>),
    Tracks {
        request: u64,
        result: Result<Vec<TrackEntry>>,
    },
    Search {
        request: u64,
        title: String,
        kind: SearchKind,
        result: Result<SearchResults>,
    },
    Devices(Result<Vec<DeviceEntry>>),
    ActionDone {
        done: Option<&'static str>,
        refresh_queue: bool,
        result: Result<()>,
    },
    Cover {
        url: String,
        result: Result<DynamicImage>,
    },
    Resized(std::result::Result<ResizeResponse, Errors>),
    /// Una tarea en segundo plano falló de forma inesperada (*panic*). La
    /// aplicación lo muestra y sigue funcionando.
    Crashed(String),
    /// La sesión no se pudo renovar (token revocado o sin red).
    SessionProblem(String),
}

/// Lanza una tarea que siempre responde con un mensaje: si la tarea sufre un
/// *panic*, se envía [`Msg::Crashed`] en lugar de perder la respuesta.
pub fn spawn_guarded<Fut>(tx: UnboundedSender<Msg>, task: Fut)
where
    Fut: Future<Output = Msg> + Send + 'static,
{
    tokio::spawn(async move {
        let msg = match AssertUnwindSafe(task).catch_unwind().await {
            Ok(msg) => msg,
            Err(payload) => Msg::Crashed(terminal::panic_message(payload)),
        };
        let _ = tx.send(msg);
    });
}

/// Igual que [`spawn_guarded`] pero para trabajo pesado de CPU (en otro hilo).
pub fn spawn_blocking_guarded<F>(tx: UnboundedSender<Msg>, work: F)
where
    F: FnOnce() -> Msg + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let msg = std::panic::catch_unwind(AssertUnwindSafe(work))
            .unwrap_or_else(|payload| Msg::Crashed(terminal::panic_message(payload)));
        let _ = tx.send(msg);
    });
}

/// Entradas fijas de la barra lateral, seguidas de las playlists del usuario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryItem {
    Liked,
    Recent,
    Queue,
    Playlist(usize),
}

pub const FIXED_LIBRARY: [LibraryItem; 3] =
    [LibraryItem::Liked, LibraryItem::Recent, LibraryItem::Queue];

/// Cómo reproducir las canciones de la lista mostrada.
#[derive(Debug, Clone)]
pub enum TrackSource {
    /// Playlist o álbum: se reproduce el contexto (siguiente/anterior respetan la lista).
    Context(PlayContextId<'static>),
    /// Lista "suelta" (me gusta, recientes, búsqueda...): se envían las URIs.
    Uris,
}

#[derive(Debug, Clone)]
pub enum MainContent {
    Loading(String),
    Message(String),
    Tracks {
        title: String,
        source: TrackSource,
        tracks: Vec<TrackEntry>,
    },
    Contexts {
        title: String,
        kind: SearchKind,
        items: Vec<ContextEntry>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Library,
    Main,
}

pub enum Popup {
    None,
    Search,
    Devices {
        devices: Option<Vec<DeviceEntry>>,
        state: ListState,
    },
    Help,
}

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
    pub big_cover: bool,

    pub playback: Option<Playback>,
    playback_inflight: bool,
    /// Última consulta del estado de reproducción (`None` = nunca).
    last_poll: Option<Instant>,
    refresh_at: Option<Instant>,

    pub playlists: Vec<PlaylistEntry>,
    pub library_state: ListState,
    pub active_library: Option<LibraryItem>,

    pub main: MainContent,
    pub main_state: TableState,
    request_id: u64,
    /// Título y origen de la lista que se está cargando.
    pending_list: Option<(String, TrackSource)>,
    /// Tras un 429 de Spotify, no se consulta hasta este instante.
    backoff_until: Option<Instant>,

    pub search_input: String,
    pub search_kind: SearchKind,

    pub picker: Picker,
    pub cover: ThreadProtocol,
    cover_url: Option<String>,

    pub status: Option<Status>,

    pending_seek: Debounced<u64>,
    pending_volume: Debounced<u8>,
    session_problem: Option<String>,
}

/// Espera sin cambios antes de enviar un valor agrupado.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Agrupa cambios rápidos (una tecla mantenida) y entrega solo el último
/// valor cuando pasan [`DEBOUNCE`] sin cambios nuevos.
#[derive(Debug, Default)]
struct Debounced<T> {
    pending: Option<(T, Instant)>,
}

impl<T> Debounced<T> {
    fn set(&mut self, value: T) {
        self.pending = Some((value, Instant::now()));
    }

    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn take_ready(&mut self) -> Option<T> {
        match &self.pending {
            Some((_, at)) if at.elapsed() >= DEBOUNCE => self.pending.take().map(|(v, _)| v),
            _ => None,
        }
    }
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
            big_cover: false,
            playback: None,
            playback_inflight: false,
            last_poll: None,
            refresh_at: None,
            playlists: Vec::new(),
            library_state,
            active_library: None,
            main: MainContent::Message("Elige una lista en la biblioteca (Enter).".into()),
            main_state: TableState::default(),
            request_id: 0,
            pending_list: None,
            backoff_until: None,
            search_input: String::new(),
            search_kind: SearchKind::Tracks,
            picker,
            cover: ThreadProtocol::new(resize_tx, None),
            cover_url: None,
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

    pub fn sender(&self) -> UnboundedSender<Msg> {
        self.tx.clone()
    }

    // -----------------------------------------------------------------------
    // Tareas en segundo plano
    // -----------------------------------------------------------------------

    fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        let sp = self.spotify.clone();
        let tx = self.tx.clone();
        spawn_guarded(self.tx.clone(), async move {
            // Si la sesión no se pudo renovar, se avisa (la petición sigue y
            // fallará con su propio mensaje, sin bloquear la interfaz).
            if let Err(e) = spotify::ensure_token(&sp).await {
                let _ = tx.send(Msg::SessionProblem(e.to_string()));
            }
            f(sp).await
        });
    }

    fn since_last_poll(&self) -> Duration {
        self.last_poll.map_or(Duration::MAX, |t| t.elapsed())
    }

    fn poll_playback(&mut self) {
        if self.playback_inflight || self.backoff_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.playback_inflight = true;
        self.last_poll = Some(Instant::now());
        self.spawn(|sp| async move { Msg::Playback(spotify::playback(&sp).await) });
    }

    fn schedule_refresh(&mut self, after: Duration) {
        let at = Instant::now() + after;
        self.refresh_at = Some(self.refresh_at.map_or(at, |r| r.min(at)));
    }

    fn load_playlists(&mut self) {
        self.spawn(|sp| async move { Msg::Playlists(spotify::playlists(&sp).await) });
    }

    /// Prepara la vista principal para una nueva lista; las respuestas de
    /// peticiones anteriores se descartan comparando el identificador.
    fn next_request(&mut self, title: String, source: TrackSource) -> u64 {
        self.request_id += 1;
        self.main = MainContent::Loading(format!("Cargando «{title}»…"));
        self.pending_list = Some((title, source));
        self.main_state = TableState::default();
        self.request_id
    }

    fn do_action(&mut self, action: Action) {
        let done = action.done_message();
        let refresh_queue = action.changes_queue();
        self.spawn(move |sp| async move {
            Msg::ActionDone {
                done,
                refresh_queue,
                result: spotify::perform(&sp, action).await,
            }
        });
    }

    /// Espacia las consultas periódicas cuando Spotify pide calma (429) o
    /// cuando no hay red, para no saturar ni gastar batería.
    fn note_rate_limit(&mut self, err: &anyhow::Error) {
        let Some(api) = err.downcast_ref::<ApiError>() else {
            return;
        };
        let wait = match api.status {
            Some(429) => api.retry_after.unwrap_or(Duration::from_secs(5)),
            None => OFFLINE_RETRY,
            Some(_) => return,
        };
        self.backoff_until = Some(Instant::now() + wait.min(Duration::from_secs(120)));
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    /// Último problema de sesión pendiente (se borra cuando una petición va bien).
    pub fn session_problem(&self) -> Option<&str> {
        self.session_problem.as_deref()
    }

    pub fn visible_status(&self) -> Option<&Status> {
        self.status.as_ref().filter(|s| s.at.elapsed() < STATUS_TTL)
    }

    // -----------------------------------------------------------------------
    // Biblioteca y listas
    // -----------------------------------------------------------------------

    pub fn library_len(&self) -> usize {
        FIXED_LIBRARY.len() + self.playlists.len()
    }

    pub fn library_item(&self, index: usize) -> Option<LibraryItem> {
        if index < FIXED_LIBRARY.len() {
            Some(FIXED_LIBRARY[index])
        } else if index - FIXED_LIBRARY.len() < self.playlists.len() {
            Some(LibraryItem::Playlist(index - FIXED_LIBRARY.len()))
        } else {
            None
        }
    }

    fn open_library(&mut self, item: LibraryItem) {
        self.active_library = Some(item);
        self.focus = Focus::Main;
        let title = self.library_title(item);
        match item {
            LibraryItem::Liked => {
                let req = self.next_request(title, TrackSource::Uris);
                self.load_tracks(req, |sp| async move { spotify::saved_tracks(&sp).await });
            }
            LibraryItem::Recent => {
                let req = self.next_request(title, TrackSource::Uris);
                self.load_tracks(req, |sp| async move { spotify::recently_played(&sp).await });
            }
            LibraryItem::Queue => {
                let req = self.next_request(title, TrackSource::Uris);
                self.load_tracks(req, |sp| async move { spotify::queue(&sp).await });
            }
            LibraryItem::Playlist(i) => {
                let Some(id) = self.playlists.get(i).map(|p| p.id.clone()) else {
                    return;
                };
                let source = TrackSource::Context(PlayContextId::Playlist(id.clone()));
                let req = self.next_request(title, source);
                self.load_tracks(
                    req,
                    |sp| async move { spotify::playlist_tracks(&sp, id).await },
                );
            }
        }
    }

    fn load_tracks<F, Fut>(&self, request: u64, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Vec<TrackEntry>>> + Send + 'static,
    {
        self.spawn(move |sp| async move {
            Msg::Tracks {
                request,
                result: f(sp).await,
            }
        });
    }

    pub fn library_title(&self, item: LibraryItem) -> String {
        match item {
            LibraryItem::Liked => "Tus me gusta".into(),
            LibraryItem::Recent => "Reproducidas recientemente".into(),
            LibraryItem::Queue => "Cola de reproducción".into(),
            LibraryItem::Playlist(i) => self
                .playlists
                .get(i)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Playlist".into()),
        }
    }

    fn open_context(&mut self, entry: ContextEntry) {
        let source = TrackSource::Context(entry.id.clone());
        match entry.id {
            PlayContextId::Playlist(id) => {
                self.active_library = None;
                let req = self.next_request(entry.name, source);
                self.load_tracks(
                    req,
                    |sp| async move { spotify::playlist_tracks(&sp, id).await },
                );
            }
            PlayContextId::Album(id) => {
                self.active_library = None;
                let name = entry.name.clone();
                let req = self.next_request(entry.name, source);
                self.load_tracks(req, |sp| async move {
                    spotify::album_tracks(&sp, id, &name).await
                });
            }
            _ => self.set_status("Los artistas solo se pueden reproducir (Enter)", false),
        }
    }

    fn run_search(&mut self) {
        let query = self.search_input.trim().to_string();
        if query.is_empty() {
            return;
        }
        let kind = self.search_kind;
        let title = format!("Búsqueda: «{query}» · {}", kind.label());
        self.active_library = None;
        self.focus = Focus::Main;
        let req = self.next_request(title.clone(), TrackSource::Uris);
        self.main = MainContent::Loading(format!(
            "Buscando {} «{query}»…",
            kind.label().to_lowercase()
        ));
        self.spawn(move |sp| async move {
            Msg::Search {
                request: req,
                title,
                kind,
                result: spotify::search(&sp, &query, kind).await,
            }
        });
    }

    fn main_len(&self) -> usize {
        match &self.main {
            MainContent::Tracks { tracks, .. } => tracks.len(),
            MainContent::Contexts { items, .. } => items.len(),
            _ => 0,
        }
    }

    fn activate_main(&mut self) {
        let Some(i) = self.main_state.selected() else {
            return;
        };
        let (action, name) = match &self.main {
            MainContent::Tracks { source, tracks, .. } => {
                let Some(track) = tracks.get(i) else { return };
                let action = match source {
                    TrackSource::Context(ctx) => Some(Action::PlayContext {
                        context: ctx.clone(),
                        start_uri: track.uri(),
                    }),
                    TrackSource::Uris => {
                        // Solo las entradas con ID son reproducibles (no archivos locales).
                        let start = tracks[..i].iter().filter(|t| t.id.is_some()).count();
                        let ids: Vec<_> = tracks.iter().filter_map(|t| t.id.clone()).collect();
                        track
                            .id
                            .as_ref()
                            .map(|_| Action::PlayTracks { tracks: ids, start })
                    }
                };
                (action, track.name.clone())
            }
            MainContent::Contexts { items, .. } => {
                let Some(entry) = items.get(i) else { return };
                let action = Action::PlayContext {
                    context: entry.id.clone(),
                    start_uri: None,
                };
                (Some(action), entry.name.clone())
            }
            _ => return,
        };
        match action {
            Some(action) => {
                self.set_status(format!("Reproduciendo «{name}»"), false);
                self.do_action(action);
            }
            None => self.set_status("Esta canción no se puede reproducir", true),
        }
    }

    fn queue_selected(&mut self) {
        let Some(i) = self.main_state.selected() else {
            return;
        };
        if let MainContent::Tracks { tracks, .. } = &self.main
            && let Some(t) = tracks.get(i)
        {
            match t.id.clone() {
                Some(id) => self.do_action(Action::Queue(id)),
                None => self.set_status("Esta canción no se puede añadir a la cola", true),
            }
        }
    }

    fn open_selected(&mut self) {
        let Some(i) = self.main_state.selected() else {
            return;
        };
        if let MainContent::Contexts { items, .. } = &self.main
            && let Some(entry) = items.get(i).cloned()
        {
            self.open_context(entry);
        }
    }

    // -----------------------------------------------------------------------
    // Reproducción (con actualización optimista del estado local)
    // -----------------------------------------------------------------------

    fn toggle_play(&mut self) {
        let Some(pb) = self.playback.as_mut() else {
            self.set_status(
                "No hay nada reproduciéndose. Elige una canción o un dispositivo ('d').",
                true,
            );
            return;
        };
        let playing = pb.is_playing;
        pb.progress_ms = pb.current_progress_ms();
        pb.fetched_at = Instant::now();
        pb.is_playing = !playing;
        self.do_action(Action::TogglePlay { playing });
    }

    fn seek(&mut self, forward: bool) {
        let Some(pb) = self.playback.as_mut() else {
            return;
        };
        let current = pb.current_progress_ms();
        let target = if forward {
            (current + SEEK_STEP_MS).min(pb.duration_ms.saturating_sub(1000))
        } else {
            current.saturating_sub(SEEK_STEP_MS)
        };
        pb.progress_ms = target;
        pb.fetched_at = Instant::now();
        // Se envía al soltar la tecla: mantenerla pulsada no satura la API.
        self.pending_seek.set(target);
    }

    fn change_volume(&mut self, delta: i32) {
        let Some(pb) = self.playback.as_mut() else {
            return;
        };
        let Some(vol) = pb.volume else {
            self.set_status("Este dispositivo no permite cambiar el volumen", true);
            return;
        };
        let new = (vol as i32 + delta).clamp(0, 100) as u32;
        pb.volume = Some(new);
        self.pending_volume.set(new as u8);
    }

    /// Envía los cambios agrupados de volumen y posición cuando se dejan de pulsar.
    fn flush_debounced(&mut self) {
        if let Some(position_ms) = self.pending_seek.take_ready() {
            self.do_action(Action::Seek { position_ms });
        }
        if let Some(volume) = self.pending_volume.take_ready() {
            self.do_action(Action::Volume(volume));
        }
    }

    fn toggle_shuffle(&mut self) {
        let Some(pb) = self.playback.as_mut() else {
            return;
        };
        pb.shuffle = !pb.shuffle;
        let state = pb.shuffle;
        self.set_status(
            if state {
                "Aleatorio activado"
            } else {
                "Aleatorio desactivado"
            },
            false,
        );
        self.do_action(Action::Shuffle(state));
    }

    fn cycle_repeat(&mut self) {
        let Some(pb) = self.playback.as_mut() else {
            return;
        };
        pb.repeat = match pb.repeat {
            RepeatState::Off => RepeatState::Context,
            RepeatState::Context => RepeatState::Track,
            RepeatState::Track => RepeatState::Off,
        };
        let state = pb.repeat;
        self.set_status(format!("Repetir: {}", repeat_label(state)), false);
        self.do_action(Action::Repeat(state));
    }

    fn open_devices(&mut self) {
        self.popup = Popup::Devices {
            devices: None,
            state: ListState::default(),
        };
        self.spawn(|sp| async move { Msg::Devices(spotify::devices(&sp).await) });
    }

    // -----------------------------------------------------------------------
    // Bucle de eventos
    // -----------------------------------------------------------------------

    pub fn on_tick(&mut self) {
        self.flush_debounced();
        let now = Instant::now();
        if self.refresh_at.is_some_and(|at| now >= at) {
            self.refresh_at = None;
            self.poll_playback();
        } else if self.since_last_poll() >= self.poll_every {
            self.poll_playback();
        }
        // Al terminar la canción, se consulta enseguida la siguiente.
        if let Some(pb) = &self.playback
            && pb.is_playing
            && pb.duration_ms > 0
            && pb.current_progress_ms() >= pb.duration_ms
            && self.since_last_poll() > Duration::from_secs(1)
        {
            self.poll_playback();
        }
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Playback(result) => {
                self.playback_inflight = false;
                match result {
                    Ok(pb) => {
                        self.session_problem = None;
                        self.set_playback(pb);
                    }
                    Err(e) => {
                        self.note_rate_limit(&e);
                        self.set_status(format!("Error al consultar la reproducción: {e}"), true);
                    }
                }
            }
            Msg::Playlists(result) => match result {
                Ok(list) => self.playlists = list,
                Err(e) => {
                    self.set_status(format!("No se pudieron cargar las playlists: {e}"), true)
                }
            },
            Msg::Tracks { request, result } => {
                if request != self.request_id {
                    return;
                }
                let (title, source) = self
                    .pending_list
                    .take()
                    .unwrap_or_else(|| ("Canciones".into(), TrackSource::Uris));
                self.main = match result {
                    Ok(tracks) if tracks.is_empty() => {
                        MainContent::Message(format!("«{title}» está vacía."))
                    }
                    Ok(tracks) => {
                        self.main_state.select(Some(0));
                        MainContent::Tracks {
                            title,
                            source,
                            tracks,
                        }
                    }
                    Err(e) => MainContent::Message(format!("No se pudo cargar «{title}»: {e}")),
                };
            }
            Msg::Search {
                request,
                title,
                kind,
                result,
            } => {
                if request != self.request_id {
                    return;
                }
                self.pending_list = None;
                self.main = match result {
                    Ok(SearchResults::Tracks(tracks)) if !tracks.is_empty() => {
                        self.main_state.select(Some(0));
                        MainContent::Tracks {
                            title,
                            source: TrackSource::Uris,
                            tracks,
                        }
                    }
                    Ok(SearchResults::Contexts(items)) if !items.is_empty() => {
                        self.main_state.select(Some(0));
                        MainContent::Contexts { title, kind, items }
                    }
                    Ok(_) => MainContent::Message(format!("{title}\n\nSin resultados.")),
                    Err(e) => MainContent::Message(format!("Error en la búsqueda: {e}")),
                };
            }
            Msg::Devices(result) => {
                if let Popup::Devices { devices, state } = &mut self.popup {
                    match result {
                        Ok(list) => {
                            let active = list.iter().position(|d| d.active).unwrap_or(0);
                            state.select((!list.is_empty()).then_some(active));
                            *devices = Some(list);
                        }
                        Err(e) => {
                            self.popup = Popup::None;
                            self.set_status(
                                format!("No se pudieron listar los dispositivos: {e}"),
                                true,
                            );
                        }
                    }
                }
            }
            Msg::ActionDone {
                done,
                refresh_queue,
                result,
            } => {
                match result {
                    Ok(()) => {
                        if let Some(text) = done {
                            self.set_status(text, false);
                        }
                    }
                    Err(e) => {
                        self.note_rate_limit(&e);
                        self.set_status(e.to_string(), true);
                    }
                }
                // Spotify tarda un poco en reflejar los cambios.
                self.schedule_refresh(Duration::from_millis(600));
                if refresh_queue && self.active_library == Some(LibraryItem::Queue) {
                    self.open_library(LibraryItem::Queue);
                }
            }
            Msg::Cover { url, result } => {
                if self.cover_url.as_deref() != Some(url.as_str()) {
                    return;
                }
                match result {
                    Ok(img) => self
                        .cover
                        .replace_protocol(self.picker.new_resize_protocol(img)),
                    Err(e) => self.set_status(format!("No se pudo cargar la carátula: {e}"), true),
                }
            }
            Msg::Resized(result) => match result {
                Ok(resp) => {
                    self.cover.update_resized_protocol(resp);
                }
                Err(e) => self.set_status(format!("Error al dibujar la carátula: {e}"), true),
            },
            Msg::Crashed(reason) => self.recover_from_crash(&reason),
            Msg::SessionProblem(reason) => {
                self.session_problem = Some(reason.clone());
                self.set_status(reason, true);
            }
        }
    }

    /// Deja el estado coherente tras un fallo inesperado en una tarea.
    fn recover_from_crash(&mut self, reason: &str) {
        self.playback_inflight = false;
        if matches!(self.main, MainContent::Loading(_)) {
            self.pending_list = None;
            self.main =
                MainContent::Message(format!("No se pudo completar la operación: {reason}"));
        }
        if matches!(self.popup, Popup::Devices { devices: None, .. }) {
            self.popup = Popup::None;
        }
        self.set_status(
            format!("Error interno (la aplicación sigue funcionando): {reason}"),
            true,
        );
    }

    /// Registra un fallo capturado en el bucle principal (al dibujar o al
    /// procesar un evento).
    pub fn report_crash(&mut self, reason: &str) {
        self.recover_from_crash(reason);
    }

    pub fn show_error(&mut self, text: String) {
        self.set_status(text, true);
    }

    fn set_playback(&mut self, mut pb: Option<Playback>) {
        // Mientras haya un cambio sin enviar, se conserva el valor local para
        // que la pantalla no "salte" hacia atrás.
        if let (Some(new), Some(old)) = (pb.as_mut(), self.playback.as_ref()) {
            if self.pending_seek.is_pending() {
                new.progress_ms = old.current_progress_ms();
                new.fetched_at = Instant::now();
            }
            if self.pending_volume.is_pending() {
                new.volume = old.volume;
            }
        }
        let new_url = pb.as_ref().and_then(|p| p.cover_url.clone());
        if new_url != self.cover_url {
            self.cover_url = new_url.clone();
            match new_url {
                Some(url) => {
                    let http = self.http.clone();
                    spawn_guarded(self.tx.clone(), async move {
                        let result = spotify::download_image(&http, &url).await;
                        Msg::Cover { url, result }
                    });
                }
                None => self.cover.empty_protocol(),
            }
        }
        self.playback = pb;
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Paste(text) if matches!(self.popup, Popup::Search) => {
                let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                self.search_input.push_str(&clean);
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let command = command_for(key);
        if command == Some(Command::Quit) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        match self.popup {
            Popup::Search => self.on_search_key(key),
            Popup::Help => self.popup = Popup::None,
            Popup::Devices { .. } => {
                if let Some(command) = command {
                    self.on_devices_command(command);
                }
            }
            Popup::None => {
                if let Some(command) = command {
                    self.run(command);
                }
            }
        }
    }

    /// Ejecuta una orden de la pantalla principal.
    fn run(&mut self, command: Command) {
        use Command::*;
        match command {
            Quit => self.should_quit = true,
            Back => {}
            TogglePlay => self.toggle_play(),
            Next => self.skip(Action::Next, "Siguiente ⏭"),
            Previous => self.skip(Action::Previous, "Anterior ⏮"),
            SeekForward => self.seek(true),
            SeekBackward => self.seek(false),
            VolumeUp => self.change_volume(VOLUME_STEP),
            VolumeDown => self.change_volume(-VOLUME_STEP),
            ToggleShuffle => self.toggle_shuffle(),
            CycleRepeat => self.cycle_repeat(),
            OpenSearch => self.popup = Popup::Search,
            OpenDevices => self.open_devices(),
            ToggleCover => self.big_cover = !self.big_cover,
            ShowHelp => self.popup = Popup::Help,
            Refresh => {
                self.load_playlists();
                self.poll_playback();
                self.set_status("Actualizando…", false);
            }
            SwitchFocus => {
                self.focus = match self.focus {
                    Focus::Library => Focus::Main,
                    Focus::Main => Focus::Library,
                }
            }
            QueueSelected => self.queue_selected(),
            OpenSelected => self.open_selected(),
            Move(_) | Jump(_) => self.move_selection(command),
            Activate => match self.focus {
                Focus::Library => {
                    if let Some(item) = self
                        .library_state
                        .selected()
                        .and_then(|i| self.library_item(i))
                    {
                        self.open_library(item);
                    }
                }
                Focus::Main => self.activate_main(),
            },
        }
    }

    fn skip(&mut self, action: Action, label: &str) {
        self.set_status(label, false);
        self.do_action(action);
    }

    fn on_devices_command(&mut self, command: Command) {
        let Popup::Devices { devices, state } = &mut self.popup else {
            return;
        };
        match command {
            Command::Back | Command::Quit | Command::OpenDevices => self.popup = Popup::None,
            Command::Move(_) | Command::Jump(_) => {
                let len = devices.as_ref().map_or(0, Vec::len);
                state.select(step_index(state.selected(), len, command));
            }
            Command::Activate => {
                let device = state
                    .selected()
                    .and_then(|i| devices.as_ref().and_then(|d| d.get(i)))
                    .and_then(|d| d.id.clone());
                self.popup = Popup::None;
                match device {
                    Some(device_id) => self.do_action(Action::Transfer { device_id }),
                    None => self.set_status("Ese dispositivo no se puede controlar", true),
                }
            }
            _ => {}
        }
    }

    /// En el buscador las teclas escriben texto en vez de ejecutar órdenes.
    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Enter => {
                self.popup = Popup::None;
                self.run_search();
            }
            KeyCode::Tab => self.search_kind = self.search_kind.next(),
            KeyCode::BackTab => self.search_kind = self.search_kind.prev(),
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.clear();
            }
            KeyCode::Char(c) => self.search_input.push(c),
            _ => {}
        }
    }

    fn move_selection(&mut self, command: Command) {
        match self.focus {
            Focus::Library => {
                let len = self.library_len();
                let next = step_index(self.library_state.selected(), len, command);
                self.library_state.select(next);
            }
            Focus::Main => {
                let len = self.main_len();
                let next = step_index(self.main_state.selected(), len, command);
                self.main_state.select(next);
            }
        }
    }
}

pub fn repeat_label(state: RepeatState) -> &'static str {
    match state {
        RepeatState::Off => "no",
        RepeatState::Context => "lista",
        RepeatState::Track => "canción",
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::spotify::ContextEntry;
    use rspotify::model::AlbumId;

    /// App sin conexión real, con dibujado de imágenes por bloques.
    pub fn test_app() -> (App, UnboundedReceiver<ResizeRequest>) {
        let (tx, rx) = unbounded_channel();
        (App::new(Client::default(), Picker::halfblocks(), tx, 3), rx)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn album(name: &str) -> ContextEntry {
        ContextEntry {
            name: name.into(),
            detail: String::new(),
            id: PlayContextId::Album(
                AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy".to_string()).unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn search_typing_paste_and_stale_results() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('/')));
        assert!(matches!(app.popup, Popup::Search));
        for c in "que".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        // 'q' dentro del buscador escribe, no sale de la aplicación.
        assert!(!app.should_quit);
        app.on_event(Event::Paste("en\n".into()));
        assert_eq!(app.search_input, "queen");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.search_kind, SearchKind::Albums);

        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.popup, Popup::None));
        assert!(matches!(app.main, MainContent::Loading(_)));
        let current = app.request_id;

        // Una respuesta de una petición anterior se ignora.
        app.on_msg(Msg::Search {
            request: current - 1,
            title: "vieja".into(),
            kind: SearchKind::Albums,
            result: Ok(SearchResults::Contexts(vec![album("Viejo")])),
        });
        assert!(matches!(app.main, MainContent::Loading(_)));

        app.on_msg(Msg::Search {
            request: current,
            title: "Búsqueda: «queen» · Álbumes".into(),
            kind: SearchKind::Albums,
            result: Ok(SearchResults::Contexts(vec![album("A Night at the Opera")])),
        });
        match &app.main {
            MainContent::Contexts { title, items, .. } => {
                assert!(title.contains("queen"));
                assert_eq!(items[0].name, "A Night at the Opera");
            }
            other => panic!("se esperaban resultados, no {other:?}"),
        }
        assert_eq!(app.main_state.selected(), Some(0));
    }

    #[tokio::test]
    async fn opening_an_album_keeps_its_title_and_context() {
        let (mut app, _rx) = test_app();
        app.main = MainContent::Contexts {
            title: "Búsqueda".into(),
            kind: SearchKind::Albums,
            items: vec![album("Thriller")],
        };
        app.main_state.select(Some(0));
        app.focus = Focus::Main;
        app.on_event(key(KeyCode::Char('o')));
        // El usuario edita la búsqueda mientras carga: no debe afectar al título.
        app.search_input = "otra cosa".into();
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Ok(vec![TrackEntry {
                name: "Billie Jean".into(),
                artists: "Michael Jackson".into(),
                album: "Thriller".into(),
                duration_ms: 294_000,
                id: None,
            }]),
        });
        match &app.main {
            MainContent::Tracks { title, source, .. } => {
                assert_eq!(title, "Thriller");
                assert!(matches!(
                    source,
                    TrackSource::Context(PlayContextId::Album(_))
                ));
            }
            other => panic!("se esperaba la lista del álbum, no {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_limit_pauses_polling() {
        let (mut app, _rx) = test_app();
        let err = anyhow::Error::new(ApiError::rate_limited(Some(Duration::from_secs(30))));
        app.on_msg(Msg::Playback(Err(err)));
        assert!(
            app.backoff_until
                .is_some_and(|t| t > Instant::now() + Duration::from_secs(25))
        );
        app.poll_playback();
        assert!(
            !app.playback_inflight,
            "no debe consultar durante la espera"
        );
    }

    #[test]
    fn library_items_map_to_indexes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let (mut app, _rx) = test_app();
        app.playlists = vec![PlaylistEntry {
            id: rspotify::model::PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap(),
            name: "P".into(),
            owner: "o".into(),
            total: 1,
        }];
        assert_eq!(app.library_item(0), Some(LibraryItem::Liked));
        assert_eq!(app.library_item(3), Some(LibraryItem::Playlist(0)));
        assert_eq!(app.library_item(4), None);
        assert_eq!(app.library_len(), 4);
    }
}

#[cfg(test)]
mod robustness {
    use super::tests::test_app;
    use super::*;

    /// Generador pseudoaleatorio mínimo (reproducible, sin dependencias).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
    }

    const KEYS: [KeyCode; 30] = [
        KeyCode::Char(' '),
        KeyCode::Char('n'),
        KeyCode::Char('p'),
        KeyCode::Char('s'),
        KeyCode::Char('r'),
        KeyCode::Char('/'),
        KeyCode::Char('d'),
        KeyCode::Char('c'),
        KeyCode::Char('?'),
        KeyCode::Char('R'),
        KeyCode::Char('a'),
        KeyCode::Char('o'),
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char('g'),
        KeyCode::Char('G'),
        KeyCode::Char('+'),
        KeyCode::Char('-'),
        KeyCode::Char('x'),
        KeyCode::Char('é'),
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Enter,
        KeyCode::Esc,
    ];

    fn err<T>() -> Result<T> {
        Err(anyhow::anyhow!("fallo simulado"))
    }

    fn random_msg(rng: &mut Lcg, app: &App) -> Msg {
        let request = app.request_id.wrapping_sub((rng.next() % 2) as u64);
        match rng.next() % 8 {
            0 => Msg::Playback(Ok(None)),
            1 => Msg::Playback(err()),
            2 => Msg::Tracks {
                request,
                result: Ok(Vec::new()),
            },
            3 => Msg::Tracks {
                request,
                result: err(),
            },
            4 => Msg::Devices(Ok(Vec::new())),
            5 => Msg::ActionDone {
                done: Some("ok"),
                refresh_queue: true,
                result: err(),
            },
            6 => Msg::Search {
                request,
                title: "t".into(),
                kind: SearchKind::Albums,
                result: Ok(SearchResults::Contexts(Vec::new())),
            },
            _ => Msg::Crashed("panic simulado".into()),
        }
    }

    /// Miles de teclas y respuestas aleatorias: nada debe provocar un panic.
    #[tokio::test]
    async fn survives_random_keys_and_messages() {
        let (mut app, _rx) = test_app();
        let mut rng = Lcg(42);
        for _ in 0..5_000 {
            if rng.next().is_multiple_of(3) {
                let msg = random_msg(&mut rng, &app);
                app.on_msg(msg);
            } else {
                let code = KEYS[rng.next() % KEYS.len()];
                app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
            }
            app.on_tick();
            // No salir: se reabre si alguna tecla pidió cerrar.
            app.should_quit = false;
        }
    }

    #[tokio::test]
    async fn a_panicking_task_is_reported_not_fatal() {
        let (tx, mut rx) = unbounded_channel();
        spawn_guarded(tx.clone(), async {
            panic!("la API devolvió algo imposible")
        });
        match rx.recv().await {
            Some(Msg::Crashed(reason)) => assert!(reason.contains("imposible")),
            _ => panic!("se esperaba Msg::Crashed"),
        }
        spawn_blocking_guarded(tx, || panic!("fallo al redimensionar"));
        assert!(matches!(rx.recv().await, Some(Msg::Crashed(_))));
    }

    #[test]
    fn debounced_delivers_only_the_last_value() {
        let mut d = Debounced::default();
        d.set(10);
        d.set(15);
        d.set(20);
        assert_eq!(d.take_ready(), None, "aún no pasó el tiempo de espera");
        std::thread::sleep(DEBOUNCE + Duration::from_millis(20));
        assert_eq!(d.take_ready(), Some(20));
        assert_eq!(d.take_ready(), None);
        assert!(!d.is_pending());
    }

    #[tokio::test]
    async fn crash_while_loading_leaves_a_usable_screen() {
        let (mut app, _rx) = test_app();
        app.main = MainContent::Loading("Cargando…".into());
        app.playback_inflight = true;
        app.on_msg(Msg::Crashed("boom".into()));
        assert!(matches!(app.main, MainContent::Message(_)));
        assert!(
            !app.playback_inflight,
            "las consultas deben poder reanudarse"
        );
    }
}
