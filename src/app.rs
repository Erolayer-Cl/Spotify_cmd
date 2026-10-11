//! Estado de la aplicación y manejo de eventos (teclado, respuestas de la API).

use std::{
    future::Future,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

use crate::spotify::{
    self, Action, Client, ContextEntry, DeviceEntry, Playback, PlaylistEntry, SearchKind,
    SearchResults, TrackEntry,
};

const STATUS_TTL: Duration = Duration::from_secs(6);
const SEEK_STEP_MS: u64 = 10_000;
const VOLUME_STEP: i32 = 5;

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
        result: Result<SearchResults>,
    },
    Devices(Result<Vec<DeviceEntry>>),
    ActionDone {
        done: Option<&'static str>,
        result: Result<()>,
    },
    Cover {
        url: String,
        result: Result<DynamicImage>,
    },
    Resized(std::result::Result<ResizeResponse, Errors>),
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
    last_poll: Instant,
    refresh_at: Option<Instant>,

    pub playlists: Vec<PlaylistEntry>,
    pub library_state: ListState,
    pub active_library: Option<LibraryItem>,

    pub main: MainContent,
    pub main_state: TableState,
    request_id: u64,
    /// Título y origen de la lista que se está cargando desde la búsqueda.
    pending_title: Option<(String, TrackSource)>,

    pub search_input: String,
    pub search_kind: SearchKind,

    pub picker: Picker,
    pub cover: ThreadProtocol,
    cover_url: Option<String>,

    pub status: Option<Status>,
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
            last_poll: Instant::now() - Duration::from_secs(3600),
            refresh_at: None,
            playlists: Vec::new(),
            library_state,
            active_library: None,
            main: MainContent::Message("Elige una lista en la biblioteca (Enter).".into()),
            main_state: TableState::default(),
            request_id: 0,
            pending_title: None,
            search_input: String::new(),
            search_kind: SearchKind::Tracks,
            picker,
            cover: ThreadProtocol::new(resize_tx, None),
            cover_url: None,
            status: None,
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
        tokio::spawn(async move {
            spotify::ensure_token(&sp).await;
            let _ = tx.send(f(sp).await);
        });
    }

    fn poll_playback(&mut self) {
        if self.playback_inflight {
            return;
        }
        self.playback_inflight = true;
        self.last_poll = Instant::now();
        self.spawn(|sp| async move { Msg::Playback(spotify::playback(&sp).await) });
    }

    fn schedule_refresh(&mut self, after: Duration) {
        let at = Instant::now() + after;
        self.refresh_at = Some(self.refresh_at.map_or(at, |r| r.min(at)));
    }

    fn load_playlists(&mut self) {
        self.spawn(|sp| async move { Msg::Playlists(spotify::playlists(&sp).await) });
    }

    fn next_request(&mut self, loading: impl Into<String>) -> u64 {
        self.request_id += 1;
        self.pending_title = None;
        self.main = MainContent::Loading(loading.into());
        self.main_state = TableState::default();
        self.request_id
    }

    fn do_action(&mut self, action: Action) {
        let done = action.done_message();
        self.spawn(move |sp| async move {
            Msg::ActionDone {
                done,
                result: spotify::perform(&sp, action).await,
            }
        });
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
            at: Instant::now(),
        });
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
        match item {
            LibraryItem::Liked => {
                let req = self.next_request("Cargando tus canciones favoritas…");
                self.spawn(move |sp| async move {
                    Msg::Tracks {
                        request: req,
                        result: spotify::saved_tracks(&sp).await,
                    }
                });
            }
            LibraryItem::Recent => {
                let req = self.next_request("Cargando reproducidas recientemente…");
                self.spawn(move |sp| async move {
                    Msg::Tracks {
                        request: req,
                        result: spotify::recently_played(&sp).await,
                    }
                });
            }
            LibraryItem::Queue => {
                let req = self.next_request("Cargando la cola…");
                self.spawn(move |sp| async move {
                    Msg::Tracks {
                        request: req,
                        result: spotify::queue(&sp).await,
                    }
                });
            }
            LibraryItem::Playlist(i) => {
                let Some(p) = self.playlists.get(i).cloned() else {
                    return;
                };
                let req = self.next_request(format!("Cargando «{}»…", p.name));
                let id = p.id.clone();
                self.spawn(move |sp| async move {
                    Msg::Tracks {
                        request: req,
                        result: spotify::playlist_tracks(&sp, id).await,
                    }
                });
            }
        }
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
        match entry.id.clone() {
            PlayContextId::Playlist(id) => {
                self.active_library = None;
                let req = self.next_request(format!("Cargando «{}»…", entry.name));
                self.pending_title = Some((entry.name, TrackSource::Context(entry.id)));
                self.spawn(move |sp| async move {
                    Msg::Tracks {
                        request: req,
                        result: spotify::playlist_tracks(&sp, id).await,
                    }
                });
            }
            PlayContextId::Album(id) => {
                self.active_library = None;
                let req = self.next_request(format!("Cargando «{}»…", entry.name));
                let name = entry.name.clone();
                self.pending_title = Some((entry.name, TrackSource::Context(entry.id)));
                self.spawn(move |sp| async move {
                    Msg::Tracks {
                        request: req,
                        result: spotify::album_tracks(&sp, id, &name).await,
                    }
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
        self.active_library = None;
        self.focus = Focus::Main;
        let req = self.next_request(format!(
            "Buscando {} «{query}»…",
            kind.label().to_lowercase()
        ));
        self.spawn(move |sp| async move {
            Msg::Search {
                request: req,
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
        self.do_action(Action::Seek {
            position_ms: target,
        });
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
        self.do_action(Action::Volume(new as u8));
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
        let now = Instant::now();
        if self.refresh_at.is_some_and(|at| now >= at) {
            self.refresh_at = None;
            self.poll_playback();
        } else if self.last_poll.elapsed() >= self.poll_every {
            self.poll_playback();
        }
        // Al terminar la canción, se consulta enseguida la siguiente.
        if let Some(pb) = &self.playback
            && pb.is_playing
            && pb.duration_ms > 0
            && pb.current_progress_ms() >= pb.duration_ms
            && self.last_poll.elapsed() > Duration::from_secs(1)
        {
            self.poll_playback();
        }
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Playback(result) => {
                self.playback_inflight = false;
                match result {
                    Ok(pb) => self.set_playback(pb),
                    Err(e) => {
                        self.set_status(format!("Error al consultar la reproducción: {e}"), true)
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
                let pending = self.pending_title.take();
                self.main = match result {
                    Ok(tracks) => {
                        let (title, source) = match (pending, self.active_library) {
                            (Some((title, source)), _) => (title, source),
                            (None, Some(LibraryItem::Playlist(i))) => (
                                self.library_title(LibraryItem::Playlist(i)),
                                self.playlists
                                    .get(i)
                                    .map(|p| {
                                        TrackSource::Context(PlayContextId::Playlist(p.id.clone()))
                                    })
                                    .unwrap_or(TrackSource::Uris),
                            ),
                            (None, Some(item)) => (self.library_title(item), TrackSource::Uris),
                            (None, None) => ("Canciones".into(), TrackSource::Uris),
                        };
                        if tracks.is_empty() {
                            MainContent::Message(format!("«{title}» está vacía."))
                        } else {
                            self.main_state.select(Some(0));
                            MainContent::Tracks {
                                title,
                                source,
                                tracks,
                            }
                        }
                    }
                    Err(e) => MainContent::Message(format!("Error: {e}")),
                };
            }
            Msg::Search { request, result } => {
                if request != self.request_id {
                    return;
                }
                let query = self.search_input.trim().to_string();
                let kind = self.search_kind;
                self.main = match result {
                    Ok(SearchResults::Tracks(tracks)) if !tracks.is_empty() => {
                        self.main_state.select(Some(0));
                        MainContent::Tracks {
                            title: format!("Búsqueda: «{query}» · {}", kind.label()),
                            source: TrackSource::Uris,
                            tracks,
                        }
                    }
                    Ok(SearchResults::Contexts(items)) if !items.is_empty() => {
                        self.main_state.select(Some(0));
                        MainContent::Contexts {
                            title: format!("Búsqueda: «{query}» · {}", kind.label()),
                            kind,
                            items,
                        }
                    }
                    Ok(_) => MainContent::Message(format!("Sin resultados para «{query}».")),
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
            Msg::ActionDone { done, result } => {
                match result {
                    Ok(()) => {
                        if let Some(text) = done {
                            self.set_status(text, false);
                        }
                    }
                    Err(e) => self.set_status(e.to_string(), true),
                }
                // Spotify tarda un poco en reflejar los cambios.
                self.schedule_refresh(Duration::from_millis(600));
                if self.active_library == Some(LibraryItem::Queue) {
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
        }
    }

    fn set_playback(&mut self, pb: Option<Playback>) {
        let new_url = pb.as_ref().and_then(|p| p.cover_url.clone());
        if new_url != self.cover_url {
            self.cover_url = new_url.clone();
            match new_url {
                Some(url) => {
                    let http = self.http.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let result = spotify::download_image(&http, &url).await;
                        let _ = tx.send(Msg::Cover { url, result });
                    });
                }
                None => self.cover.empty_protocol(),
            }
        }
        self.playback = pb;
    }

    pub fn on_event(&mut self, event: Event) {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            self.on_key(key);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match &mut self.popup {
            Popup::Search => return self.on_search_key(key),
            Popup::Help => {
                self.popup = Popup::None;
                return;
            }
            Popup::Devices { devices, state } => {
                let len = devices.as_ref().map_or(0, Vec::len);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') => {
                        self.popup = Popup::None
                    }
                    KeyCode::Up | KeyCode::Char('k') => move_list(state, len, -1),
                    KeyCode::Down | KeyCode::Char('j') => move_list(state, len, 1),
                    KeyCode::Enter => {
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
                return;
            }
            Popup::None => {}
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char(' ') => self.toggle_play(),
            KeyCode::Char('n') => {
                self.set_status("Siguiente ⏭", false);
                self.do_action(Action::Next);
            }
            KeyCode::Char('p') => {
                self.set_status("Anterior ⏮", false);
                self.do_action(Action::Previous);
            }
            KeyCode::Right => self.seek(true),
            KeyCode::Left => self.seek(false),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(VOLUME_STEP),
            KeyCode::Char('-') => self.change_volume(-VOLUME_STEP),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('/') => self.popup = Popup::Search,
            KeyCode::Char('d') => self.open_devices(),
            KeyCode::Char('c') => self.big_cover = !self.big_cover,
            KeyCode::Char('?') | KeyCode::F(1) => self.popup = Popup::Help,
            KeyCode::F(5) | KeyCode::Char('R') => {
                self.load_playlists();
                self.poll_playback();
                self.set_status("Actualizando…", false);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Library => Focus::Main,
                    Focus::Main => Focus::Library,
                }
            }
            KeyCode::Char('a') => self.queue_selected(),
            KeyCode::Char('o') => self.open_selected(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(i32::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(i32::MAX / 2),
            KeyCode::Enter => match self.focus {
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
            _ => {}
        }
    }

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

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Library => {
                let len = self.library_len();
                move_list(&mut self.library_state, len, delta);
            }
            Focus::Main => {
                let len = self.main_len();
                if len == 0 {
                    return;
                }
                let cur = self.main_state.selected().unwrap_or(0) as i64;
                let next = (cur + delta as i64).clamp(0, len as i64 - 1);
                self.main_state.select(Some(next as usize));
            }
        }
    }
}

fn move_list(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        return;
    }
    let cur = state.selected().unwrap_or(0) as i64;
    let next = (cur + delta as i64).clamp(0, len as i64 - 1);
    state.select(Some(next as usize));
}

pub fn repeat_label(state: RepeatState) -> &'static str {
    match state {
        RepeatState::Off => "no",
        RepeatState::Context => "lista",
        RepeatState::Track => "canción",
    }
}
