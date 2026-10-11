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
use rspotify::model::{PlaylistId, RepeatState};
use rspotify::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;

use crate::keymap::{Command, command_for, step_index};
use crate::spotify::{
    self, Action, ApiError, Client, ContextEntry, ContextStart, DeviceEntry, Playback,
    PlaylistEntry, SearchKind, SearchResults, TrackEntry,
};
use crate::terminal;

const STATUS_TTL: Duration = Duration::from_secs(6);
const SEEK_STEP_MS: u64 = 10_000;
const VOLUME_STEP: i32 = 5;
/// Sin red, el estado se vuelve a consultar cada 10 s en lugar de cada 3 s.
const OFFLINE_RETRY: Duration = Duration::from_secs(10);
/// Spotify tarda un poco en reflejar los cambios (estado, cola...).
const SETTLE: Duration = Duration::from_millis(600);
/// Tras fallar la descarga de una carátula, espera antes de reintentarla.
const COVER_RETRY: Duration = Duration::from_secs(30);

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

/// Lanza una tarea que siempre responde con un mensaje: si la tarea sufre un
/// *panic*, se envía [`Msg::Crashed`] (con su `origin`) en lugar de perder la
/// respuesta. Solo una tarea cancelada (con el `AbortHandle` devuelto) no
/// responde.
pub fn spawn_guarded<Fut>(tx: UnboundedSender<Msg>, origin: CrashOrigin, task: Fut) -> AbortHandle
where
    Fut: Future<Output = Msg> + Send + 'static,
{
    tokio::spawn(async move {
        let msg = match AssertUnwindSafe(task).catch_unwind().await {
            Ok(msg) => msg,
            Err(payload) => crashed(origin, payload),
        };
        let _ = tx.send(msg);
    })
    .abort_handle()
}

/// Mensaje de un *panic* en una tarea de origen `origin`.
fn crashed(origin: CrashOrigin, payload: Box<dyn std::any::Any + Send>) -> Msg {
    Msg::Crashed {
        origin,
        reason: terminal::panic_message(payload),
    }
}

/// Renueva la sesión si hace falta, en su propia tarea: si se cancela la
/// petición que la necesitaba (una lista que ya no interesa), la renovación
/// termina igualmente. Cortarla a medias perdería la sesión, porque con PKCE
/// cada refresh token solo sirve una vez.
///
/// Si no se pudo renovar, se avisa y la petición sigue (fallará con su propio
/// mensaje, sin bloquear la interfaz). Devuelve el *panic* de la renovación,
/// si lo hubo: la petición que la esperaba lo comunica como suyo.
async fn renew_session(
    sp: Client,
    tx: &UnboundedSender<Msg>,
) -> Option<Box<dyn std::any::Any + Send>> {
    match tokio::spawn(async move { spotify::ensure_token(&sp).await }).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => {
            let _ = tx.send(Msg::SessionProblem(e.to_string()));
            None
        }
        Err(e) => e.try_into_panic().ok(),
    }
}

/// Igual que [`spawn_guarded`] pero para trabajo pesado de CPU (en otro hilo).
pub fn spawn_blocking_guarded<F>(tx: UnboundedSender<Msg>, origin: CrashOrigin, work: F)
where
    F: FnOnce() -> Msg + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let msg = std::panic::catch_unwind(AssertUnwindSafe(work))
            .unwrap_or_else(|payload| crashed(origin, payload));
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
    /// No se pudieron cargar las canciones de una playlist o álbum (p. ej. una
    /// playlist ajena, que Spotify no deja ver), pero Enter la reproduce entera.
    Unlisted {
        title: String,
        message: String,
        context: PlayContextId<'static>,
    },
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

/// Cómo se presenta la carga de una lista de canciones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    /// Lista nueva: «Cargando…» y selección en la primera fila.
    Open,
    /// Recarga en segundo plano de la lista visible: no la borra mientras
    /// carga y conserva la fila seleccionada.
    Reload,
}

/// Lista de canciones que se está cargando.
struct PendingList {
    title: String,
    source: TrackSource,
    mode: LoadMode,
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
    /// Número de la última consulta del estado de reproducción lanzada.
    poll_seq: u64,
    /// Las respuestas de consultas con un número menor están obsoletas: se
    /// lanzaron antes de la última acción o ya se aplicó una más reciente.
    fresh_poll: u64,
    /// Última consulta del estado de reproducción (`None` = nunca).
    last_poll: Option<Instant>,
    refresh_at: Option<Instant>,

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
    pub search_kind: SearchKind,

    pub picker: Picker,
    pub cover: ThreadProtocol,
    cover_url: Option<String>,
    /// Carátula descargada: permite volver a prepararla si falla su dibujo.
    cover_image: Option<DynamicImage>,
    /// Veces seguidas que se ha vuelto a preparar la carátula actual (vuelve
    /// a 0 cuando un redimensionado sale bien).
    cover_rebuilds: u8,
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
            poll_seq: 0,
            fresh_poll: 0,
            last_poll: None,
            refresh_at: None,
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
            search_kind: SearchKind::Tracks,
            picker,
            cover: ThreadProtocol::new(resize_tx, None),
            cover_url: None,
            cover_image: None,
            cover_rebuilds: 0,
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

    pub fn sender(&self) -> UnboundedSender<Msg> {
        self.tx.clone()
    }

    // -----------------------------------------------------------------------
    // Tareas en segundo plano
    // -----------------------------------------------------------------------

    /// Lanza una petición a Spotify en segundo plano (renovando antes la
    /// sesión si hace falta). Devuelve el `AbortHandle` para cancelarla.
    fn spawn<F, Fut>(&self, f: F) -> AbortHandle
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        self.spawn_as(CrashOrigin::Other, f)
    }

    /// Como [`Self::spawn`]; un *panic* (también el de la renovación de la
    /// sesión) se comunica como de origen `origin`.
    fn spawn_as<F, Fut>(&self, origin: CrashOrigin, f: F) -> AbortHandle
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        let sp = self.spotify.clone();
        let tx = self.tx.clone();
        spawn_guarded(self.tx.clone(), origin, async move {
            if let Some(payload) = renew_session(sp.clone(), &tx).await {
                return crashed(origin, payload);
            }
            f(sp).await
        })
    }

    fn since_last_poll(&self) -> Duration {
        self.last_poll.map_or(Duration::MAX, |t| t.elapsed())
    }

    /// Consulta el estado de reproducción, salvo que ya haya una consulta en
    /// curso o haya que esperar (429 o sin red). Devuelve si se lanzó.
    fn poll_playback(&mut self) -> bool {
        if self.playback_inflight || self.backing_off() {
            return false;
        }
        self.playback_inflight = true;
        self.last_poll = Some(Instant::now());
        self.poll_seq += 1;
        let seq = self.poll_seq;
        self.spawn(move |sp| async move {
            Msg::Playback {
                seq,
                result: spotify::playback(&sp).await,
            }
        });
        true
    }

    /// Hay que esperar antes de consultar (429 de Spotify o sin red).
    fn backing_off(&self) -> bool {
        self.backoff_until.is_some_and(|t| Instant::now() < t)
    }

    fn schedule_refresh(&mut self, after: Duration) {
        let at = Instant::now() + after;
        self.refresh_at = Some(self.refresh_at.map_or(at, |r| r.min(at)));
    }

    fn load_playlists(&mut self) {
        // Mantener pulsada R no lanza una carga por cada repetición de la tecla.
        if self.playlists_inflight {
            return;
        }
        self.playlists_inflight = true;
        self.spawn(|sp| async move { Msg::Playlists(spotify::playlists(&sp).await) });
    }

    /// Cancela la carga de lista en curso e invalida su identificador: si su
    /// respuesta llega igualmente, se descarta.
    fn abandon_request(&mut self) {
        if let Some(task) = self.list_task.take() {
            task.abort();
        }
        self.request_id += 1;
        self.pending_list = None;
    }

    /// Prepara la vista principal para una nueva petición mostrando `loading`.
    /// La anterior deja de interesar: se cancela para no seguir pidiendo
    /// páginas a Spotify, y su respuesta se descarta por el identificador.
    /// También se olvida la recarga programada de la lista anterior.
    fn begin_request(&mut self, loading: String) -> u64 {
        self.abandon_request();
        self.list_reload_at = None;
        self.main = MainContent::Loading(loading);
        self.main_state = TableState::default();
        self.request_id
    }

    /// Como [`Self::begin_request`], para una lista de canciones con su título
    /// y su origen. Una recarga no borra lo que se ve, salvo que la lista aún
    /// se estuviera cargando (entonces equivale a abrirla de nuevo).
    fn next_request(&mut self, title: String, source: TrackSource, mode: LoadMode) -> u64 {
        let mode = match self.main {
            MainContent::Loading(_) => LoadMode::Open,
            _ => mode,
        };
        match mode {
            LoadMode::Open => {
                self.begin_request(format!("Cargando «{title}»…"));
            }
            LoadMode::Reload => self.abandon_request(),
        }
        self.pending_list = Some(PendingList {
            title,
            source,
            mode,
        });
        self.request_id
    }

    /// Hay una carga de lista (o búsqueda) en curso.
    fn list_loading(&self) -> bool {
        self.list_task.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Lanza la carga de la petición en curso y guarda su tarea para poder
    /// cancelarla. Si sufre un *panic*, el fallo se atribuye a esta petición.
    fn spawn_list<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        let origin = CrashOrigin::List(self.request_id);
        self.list_task = Some(self.spawn_as(origin, f));
    }

    /// Si `request` es la petición de lista en curso. Si lo es, su respuesta
    /// ya llegó: no queda tarea que cancelar.
    fn is_current_list(&mut self, request: u64) -> bool {
        if request != self.request_id {
            return false;
        }
        self.list_task = None;
        true
    }

    fn do_action(&mut self, action: Action) {
        // Las consultas del estado ya lanzadas no reflejan esta acción: su
        // respuesta desharía el cambio optimista (p. ej. la pausa recién pulsada).
        self.fresh_poll = self.poll_seq + 1;
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

    /// Muestra la vista principal con el foco: lo que se abra a continuación
    /// tiene que verse (la carátula grande la taparía).
    fn show_list(&mut self) {
        self.focus = Focus::Main;
        self.big_cover = false;
    }

    /// La lista principal tiene el foco pero la tapa la carátula grande.
    fn list_hidden(&self) -> bool {
        self.big_cover && self.focus == Focus::Main
    }

    fn open_library(&mut self, item: LibraryItem) {
        self.active_library = Some(item);
        self.show_list();
        self.load_library(item, LoadMode::Open);
    }

    /// Recarga en segundo plano la lista de la biblioteca que se está viendo,
    /// sin mover el foco, la carátula ni la selección.
    fn reload_active(&mut self) {
        if let Some(item) = self.active_library {
            self.load_library(item, LoadMode::Reload);
        }
    }

    /// Pide en segundo plano las canciones de una entrada de la biblioteca.
    fn load_library(&mut self, item: LibraryItem, mode: LoadMode) {
        let title = self.library_title(item);
        let loose = TrackSource::Uris;
        match item {
            LibraryItem::Liked => self.load_tracks(title, loose, mode, |sp| async move {
                spotify::saved_tracks(&sp).await
            }),
            LibraryItem::Recent => self.load_tracks(title, loose, mode, |sp| async move {
                spotify::recently_played(&sp).await
            }),
            LibraryItem::Queue => {
                self.load_tracks(
                    title,
                    loose,
                    mode,
                    |sp| async move { spotify::queue(&sp).await },
                )
            }
            LibraryItem::Playlist(i) => {
                if let Some(id) = self.playlists.get(i).map(|p| p.id.clone()) {
                    self.load_playlist(title, id, mode);
                }
            }
        }
    }

    /// Pide en segundo plano las canciones de la lista (con «Cargando…» si
    /// es una lista nueva).
    fn load_tracks<F, Fut>(&mut self, title: String, source: TrackSource, mode: LoadMode, fetch: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Vec<TrackEntry>>> + Send + 'static,
    {
        let request = self.next_request(title, source, mode);
        self.spawn_list(move |sp| async move {
            Msg::Tracks {
                request,
                result: fetch(sp).await,
            }
        });
    }

    /// Carga una playlist: al reproducirla se usa su contexto.
    fn load_playlist(&mut self, title: String, id: PlaylistId<'static>, mode: LoadMode) {
        let source = TrackSource::Context(PlayContextId::Playlist(id.clone()));
        self.load_tracks(title, source, mode, |sp| async move {
            spotify::playlist_tracks(&sp, id).await
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
                self.show_list();
                self.load_playlist(entry.name, id, LoadMode::Open);
            }
            PlayContextId::Album(id) => {
                self.active_library = None;
                self.show_list();
                let name = entry.name.clone();
                self.load_tracks(entry.name, source, LoadMode::Open, move |sp| async move {
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
        self.show_list();
        // El título viaja con la respuesta (`Msg::Search`), no en `pending_list`.
        let request = self.begin_request(format!(
            "Buscando {} «{query}»…",
            kind.label().to_lowercase()
        ));
        self.spawn_list(move |sp| async move {
            Msg::Search {
                request,
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
        if let MainContent::Unlisted { title, context, .. } = &self.main {
            let action = Action::PlayContext {
                context: context.clone(),
                start: None,
            };
            self.set_status(format!("Reproduciendo «{title}»"), false);
            self.do_action(action);
            return;
        }
        let Some(i) = self.main_state.selected() else {
            return;
        };
        let (action, name) = match &self.main {
            MainContent::Tracks { source, tracks, .. } => {
                let Some(track) = tracks.get(i) else { return };
                let action = match source {
                    // Un archivo local (sin id) no se puede elegir: Spotify
                    // empezaría la lista por el principio.
                    TrackSource::Context(ctx) => {
                        context_start(tracks, i).map(|start| Action::PlayContext {
                            context: ctx.clone(),
                            start: Some(start),
                        })
                    }
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
                    start: None,
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
            // Si hay una consulta en curso (o hay que esperar), el refresco no
            // se pierde: se reintenta en el siguiente tick.
            if self.poll_playback() {
                self.refresh_at = None;
            }
        } else if self.since_last_poll() >= self.poll_every {
            self.poll_playback();
        }
        // La recarga programada espera también si Spotify pidió calma.
        if self.list_reload_at.is_some_and(|at| now >= at) && !self.backing_off() {
            self.list_reload_at = None;
            self.reload_active();
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
            Msg::Playback { seq, result } => self.on_playback(seq, result),
            Msg::Playlists(result) => {
                self.playlists_inflight = false;
                match result {
                    Ok(list) => self.replace_playlists(list),
                    Err(e) => {
                        self.set_status(format!("No se pudieron cargar las playlists: {e}"), true)
                    }
                }
            }
            Msg::Tracks { request, result } => {
                if !self.is_current_list(request) {
                    return;
                }
                let pending = self.pending_list.take().unwrap_or(PendingList {
                    title: "Canciones".into(),
                    source: TrackSource::Uris,
                    mode: LoadMode::Open,
                });
                self.show_tracks(pending, result);
            }
            Msg::Search {
                request,
                title,
                kind,
                result,
            } => {
                if !self.is_current_list(request) {
                    return;
                }
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
                let succeeded = result.is_ok();
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
                self.schedule_refresh(SETTLE);
                // La cola visible se recarga sin quitar el foco ni la selección.
                if succeeded && refresh_queue && self.active_library == Some(LibraryItem::Queue) {
                    self.schedule_list_reload();
                }
            }
            Msg::Cover { url, result } => {
                if self.cover_url.as_deref() != Some(url.as_str()) {
                    return;
                }
                match result {
                    Ok(img) => self.set_cover_image(img),
                    Err(e) => {
                        // Mejor el logo que la carátula de la canción anterior;
                        // la descarga se reintentará más tarde.
                        self.clear_cover();
                        self.cover_retry_at = Some(Instant::now() + COVER_RETRY);
                        self.set_status(format!("No se pudo cargar la carátula: {e}"), true);
                    }
                }
            }
            Msg::Resized { generation, result } => self.on_resized(generation, result),
            Msg::Crashed { origin, reason } => self.recover_from_crash(origin, &reason),
            Msg::SessionProblem(reason) => {
                self.session_problem = Some(reason.clone());
                self.set_status(reason, true);
            }
        }
    }

    /// Respuesta de la consulta número `seq` del estado de reproducción.
    fn on_playback(&mut self, seq: u64, result: Result<Option<Playback>>) {
        // Tras un fallo inesperado puede haber dos consultas en curso: la
        // marca solo se quita con la respuesta de la última.
        if seq == self.poll_seq {
            self.playback_inflight = false;
        }
        match result {
            Ok(pb) => {
                self.session_problem = None;
                // Una respuesta obsoleta desharía la última acción o pisaría
                // una más reciente: se ignora.
                if seq >= self.fresh_poll {
                    self.fresh_poll = seq + 1;
                    self.set_playback(pb);
                }
            }
            Err(e) => {
                self.note_rate_limit(&e);
                self.set_status(format!("Error al consultar la reproducción: {e}"), true);
            }
        }
    }

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
        if self.list_lost(origin) {
            self.give_up_list(reason);
        }
        if matches!(self.popup, Popup::Devices { devices: None, .. }) {
            self.popup = Popup::None;
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

    /// La carátula actual está fuera del protocolo, redimensionándose.
    fn resizing_cover(&self) -> bool {
        self.has_cover() && self.cover.protocol_type().is_none()
    }

    /// Respuesta del redimensionado pedido para la generación `generation`.
    /// La de una carátula ya sustituida (o ya preparada) no cuenta: aplicar
    /// su error cancelaría el redimensionado sano de la actual.
    fn on_resized(&mut self, generation: u64, result: std::result::Result<ResizeResponse, Errors>) {
        if generation != self.cover_generation || !self.resizing_cover() {
            return;
        }
        match result {
            Ok(resp) => {
                if self.cover.update_resized_protocol(resp) {
                    self.cover_rebuilds = 0;
                }
            }
            Err(e) => {
                if !self.rebuild_cover() {
                    self.set_status(format!("Error al dibujar la carátula: {e}"), true);
                }
            }
        }
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
        // Con otra canción cambian la cola y las recientes que se ven.
        let old_uri = self.playback.as_ref().and_then(|p| p.item_uri.as_deref());
        let new_uri = pb.as_ref().and_then(|p| p.item_uri.as_deref());
        if old_uri != new_uri
            && matches!(
                self.active_library,
                Some(LibraryItem::Queue | LibraryItem::Recent)
            )
        {
            self.schedule_list_reload();
        }
        let new_url = pb.as_ref().and_then(|p| p.cover_url.clone());
        if self.cover_needs_download(&new_url) {
            self.cover_url = new_url.clone();
            self.cover_retry_at = None;
            match new_url {
                // Mientras se descarga se sigue viendo la carátula anterior.
                Some(url) => {
                    let http = self.http.clone();
                    spawn_guarded(self.tx.clone(), CrashOrigin::Other, async move {
                        let result = spotify::download_image(&http, &url).await;
                        Msg::Cover { url, result }
                    });
                }
                None => self.clear_cover(),
            }
        }
        self.playback = pb;
    }

    /// Si hay que descargar la carátula `url`: es otra, o la anterior falló y
    /// ya toca reintentarlo.
    fn cover_needs_download(&self, url: &Option<String>) -> bool {
        *url != self.cover_url || self.cover_retry_at.is_some_and(|t| Instant::now() >= t)
    }

    /// Hay una carátula que mostrar (aunque ahora se esté redimensionando).
    pub fn has_cover(&self) -> bool {
        self.cover_image.is_some()
    }

    /// Muestra `img` como carátula.
    pub fn set_cover_image(&mut self, img: DynamicImage) {
        self.cover_rebuilds = 0;
        self.prepare_cover(img.clone());
        self.cover_image = Some(img);
    }

    /// Generación de la carátula preparada: los redimensionados que se pidan
    /// ahora la llevan en su respuesta ([`Msg::Resized`]).
    pub fn cover_generation(&self) -> u64 {
        self.cover_generation
    }

    /// Prepara `img` para dibujarla; los redimensionados pendientes de la
    /// anterior quedan obsoletos.
    fn prepare_cover(&mut self, img: DynamicImage) {
        let protocol = self.picker.new_resize_protocol(img);
        self.cover.replace_protocol(protocol);
        self.cover_generation = self.cover_generation.wrapping_add(1);
    }

    /// Quita la carátula: en su lugar se ve el logo.
    fn clear_cover(&mut self) {
        self.cover_image = None;
        self.cover.empty_protocol();
        self.cover_generation = self.cover_generation.wrapping_add(1);
    }

    /// Tras un fallo al redimensionar, la carátula se vuelve a preparar una
    /// sola vez; si vuelve a fallar sin un éxito entre medias, se quita (sale
    /// el logo). Devuelve si se pudo volver a preparar.
    fn rebuild_cover(&mut self) -> bool {
        let img = match &self.cover_image {
            Some(img) if self.cover_rebuilds == 0 => img.clone(),
            _ => {
                self.clear_cover();
                return false;
            }
        };
        self.cover_rebuilds += 1;
        self.prepare_cover(img);
        true
    }

    /// Programa la recarga de la lista visible cuando Spotify ya refleje el
    /// cambio. Una petición posterior la vuelve a retrasar.
    fn schedule_list_reload(&mut self) {
        self.list_reload_at = Some(Instant::now() + SETTLE);
    }

    /// Sustituye las playlists conservando, por su id, la lista activa y la
    /// que señala el cursor: el orden de Spotify cambia al crear o seguir una
    /// playlist desde otro dispositivo.
    fn replace_playlists(&mut self, list: Vec<PlaylistEntry>) {
        let active_id = self.playlist_id(self.active_library);
        let cursor = self.library_state.selected();
        let cursor_id = self.playlist_id(cursor.and_then(|i| self.library_item(i)));
        self.playlists = list;

        if matches!(self.active_library, Some(LibraryItem::Playlist(_))) {
            // Si ya no existe, la vista sigue mostrándola, pero sin marca.
            self.active_library = self.playlist_index(active_id).map(LibraryItem::Playlist);
        }
        // Si la playlist del cursor ya no existe, se queda en la misma fila.
        let old = cursor.unwrap_or(0);
        let cursor = self
            .playlist_index(cursor_id)
            .map_or(old, |i| i + FIXED_LIBRARY.len());
        let last = self.library_len().saturating_sub(1);
        self.library_state.select(Some(cursor.min(last)));
    }

    /// Id de la playlist de `item`, si es una playlist.
    fn playlist_id(&self, item: Option<LibraryItem>) -> Option<PlaylistId<'static>> {
        match item {
            Some(LibraryItem::Playlist(i)) => self.playlists.get(i).map(|p| p.id.clone()),
            _ => None,
        }
    }

    /// Posición actual de la playlist con ese id.
    fn playlist_index(&self, id: Option<PlaylistId<'static>>) -> Option<usize> {
        let id = id?;
        self.playlists.iter().position(|p| p.id == id)
    }

    /// Muestra las canciones recibidas para la lista `pending`.
    fn show_tracks(&mut self, pending: PendingList, result: Result<Vec<TrackEntry>>) {
        let PendingList {
            title,
            source,
            mode,
        } = pending;
        let reload = mode == LoadMode::Reload;
        self.main = match result {
            // Si falla una recarga, se sigue viendo la lista que había.
            Err(e) if reload => {
                self.set_status(format!("No se pudo actualizar «{title}»: {e}"), true);
                return;
            }
            Err(e) => load_error(title, source, &e),
            Ok(tracks) if tracks.is_empty() => {
                MainContent::Message(format!("«{title}» está vacía."))
            }
            Ok(tracks) => {
                let row = if reload {
                    kept_row(self.main_state.selected(), tracks.len())
                } else {
                    0
                };
                self.main_state.select(Some(row));
                MainContent::Tracks {
                    title,
                    source,
                    tracks,
                }
            }
        };
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Paste(text) if matches!(self.popup, Popup::Search) => {
                self.search_input.push_str(&pasted_text(&text));
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
            // Con la carátula grande la lista no se ve: no se actúa a ciegas
            // sobre ella (la carátula se cierra con 'c').
            Activate | QueueSelected | OpenSelected | Move(_) | Jump(_) if self.list_hidden() => {}
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
                // También la lista que se ve, salvo que ya se esté cargando.
                if !self.list_loading() {
                    self.reload_active();
                }
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
            Command::Back | Command::Quit => self.popup = Popup::None,
            // 'd' vuelve a buscar (como indica la ventana sin dispositivos),
            // salvo que ya se esté buscando.
            Command::OpenDevices => {
                if devices.is_some() {
                    self.open_devices();
                }
            }
            Command::Move(_) | Command::Jump(_) => {
                let len = devices.as_ref().map_or(0, Vec::len);
                state.select(step_index(state.selected(), len, command));
            }
            Command::Activate => {
                // Mientras se busca, o sin dispositivos, la ventana sigue
                // abierta con su explicación.
                let Some(list) = devices.as_ref().filter(|l| !l.is_empty()) else {
                    return;
                };
                let device = state
                    .selected()
                    .and_then(|i| list.get(i))
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
            // Algunas terminales envían Ctrl+H como retroceso.
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.clear();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                delete_last_word(&mut self.search_input);
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => self.search_input.push(c),
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

/// Lo que se muestra cuando no se pudo cargar una lista. Si Spotify no deja
/// ver sus canciones (403, una playlist ajena), se conserva para poder
/// reproducirla entera con Enter; con cualquier otro error (sin red, 5xx...)
/// esa acción también fallaría, así que solo se explica el error.
fn load_error(title: String, source: TrackSource, err: &anyhow::Error) -> MainContent {
    let message = format!("No se pudo cargar «{title}»: {err}");
    match source {
        TrackSource::Context(context) if is_forbidden(err) => MainContent::Unlisted {
            message: format!("{message}\n\nPulsa Enter para reproducir todas sus canciones."),
            title,
            context,
        },
        _ => MainContent::Message(message),
    }
}

/// Spotify denegó el acceso (403).
fn is_forbidden(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(|api| api.status == Some(403))
}

/// Desde dónde reproducir la canción `index` de un contexto; `None` si no se
/// puede (un archivo local, sin id). Spotify empieza por la primera aparición
/// de la URI: si la canción se repite en la lista y se conoce su posición, se
/// usa la posición.
fn context_start(tracks: &[TrackEntry], index: usize) -> Option<ContextStart> {
    let track = tracks.get(index)?;
    let uri = track.uri()?;
    let repeated = tracks.iter().filter(|t| t.id == track.id).nth(1).is_some();
    match track.position {
        Some(position) if repeated => Some(ContextStart::Position(position)),
        _ => Some(ContextStart::Uri(uri)),
    }
}

/// Fila que queda seleccionada al recargar una lista que ahora tiene `len`
/// filas (`len` > 0): la misma de antes, o la última si ya no existe.
fn kept_row(selected: Option<usize>, len: usize) -> usize {
    selected.unwrap_or(0).min(len.saturating_sub(1))
}

/// Texto pegado en el buscador: los saltos de línea y tabuladores separan
/// palabras (se cambian por un espacio) y el resto de caracteres de control
/// se quitan.
fn pasted_text(text: &str) -> String {
    text.split(char::is_control)
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Si una tecla con estos modificadores escribe texto: sin Ctrl ni Alt, o
/// con los dos a la vez (AltGr en Windows, para @, # o €).
fn is_text_input(modifiers: KeyModifiers) -> bool {
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    !modifiers.intersects(ctrl_alt) || modifiers.contains(ctrl_alt)
}

/// Borra la última palabra y los espacios que la siguen (Ctrl+W).
fn delete_last_word(text: &mut String) {
    let end = text.trim_end().len();
    text.truncate(end);
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    text.truncate(start);
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

    /// *Panic* simulado de una tarea de origen `origin`.
    pub fn crash(origin: CrashOrigin, reason: &str) -> Msg {
        Msg::Crashed {
            origin,
            reason: reason.into(),
        }
    }

    /// Error de la API al pedir las canciones de una playlist.
    fn api_error(status: Option<u16>) -> anyhow::Error {
        anyhow::Error::new(ApiError::playlist_items(status))
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
                position: None,
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

    /// Spotify no deja ver las canciones de una playlist ajena: se explica y
    /// Enter la reproduce entera en lugar de no hacer nada.
    #[tokio::test]
    async fn unlisted_playlist_can_still_be_played() {
        let (mut app, _rx) = test_app();
        app.playlists = vec![PlaylistEntry {
            id: rspotify::model::PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap(),
            name: "Today's Top Hits".into(),
            owner: "Spotify".into(),
            total: 50,
        }];
        app.library_state.select(Some(FIXED_LIBRARY.len()));
        app.on_event(key(KeyCode::Enter));
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Err(api_error(Some(403))),
        });
        match &app.main {
            MainContent::Unlisted {
                title,
                message,
                context,
            } => {
                assert_eq!(title, "Today's Top Hits");
                assert!(message.contains("Enter"));
                assert!(matches!(context, PlayContextId::Playlist(_)));
            }
            other => panic!("se esperaba poder reproducirla, no {other:?}"),
        }
        assert_eq!(app.focus, Focus::Main);
        app.on_event(key(KeyCode::Enter));
        let status = app.visible_status().map(|s| s.text.as_str());
        assert_eq!(status, Some("Reproduciendo «Today's Top Hits»"));
    }

    /// Sin red, la playlist no se ofrece para reproducirla entera (también
    /// fallaría): solo se explica el error.
    #[tokio::test]
    async fn offline_playlist_only_shows_the_error() {
        let (mut app, _rx) = test_app();
        app.playlists = vec![playlist("Rock")];
        app.open_library(LibraryItem::Playlist(0));
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Err(api_error(None)),
        });
        match &app.main {
            MainContent::Message(m) => {
                assert!(m.contains("Rock") && m.contains("sin conexión"));
                assert!(!m.contains("Enter"));
            }
            other => panic!("se esperaba solo el error, no {other:?}"),
        }
    }

    /// Solo un 403 de una playlist o un álbum se puede reproducir entero; el
    /// resto de errores (sin red, 429, 5xx, otros) solo se explican.
    #[test]
    fn only_forbidden_contexts_are_offered_to_play() {
        let context = || {
            let id = PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap();
            TrackSource::Context(PlayContextId::Playlist(id))
        };
        let shown = |source, err| load_error("Lista".into(), source, &err);
        assert!(matches!(
            shown(context(), api_error(Some(403))),
            MainContent::Unlisted { .. }
        ));
        for err in [
            api_error(None),
            api_error(Some(429)),
            api_error(Some(500)),
            api_error(Some(404)),
            anyhow::anyhow!("error inesperado"),
        ] {
            assert!(matches!(shown(context(), err), MainContent::Message(_)));
        }
        assert!(matches!(
            shown(TrackSource::Uris, api_error(Some(403))),
            MainContent::Message(_)
        ));
    }

    /// Las listas sin contexto (me gusta, recientes...) no tienen nada que
    /// reproducir si fallan: solo se muestra el error.
    #[tokio::test]
    async fn failed_loose_list_only_shows_the_error() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Enter));
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Err(anyhow::anyhow!("sin red")),
        });
        assert!(matches!(&app.main, MainContent::Message(m) if !m.contains("Enter")));
    }

    #[tokio::test]
    async fn rate_limit_pauses_polling() {
        let (mut app, _rx) = test_app();
        let err = anyhow::Error::new(ApiError::rate_limited(Some(Duration::from_secs(30))));
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Err(err),
        });
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

    fn playlist(name: &str) -> PlaylistEntry {
        PlaylistEntry {
            id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap(),
            name: name.into(),
            owner: "o".into(),
            total: 1,
        }
    }

    fn song(name: &str) -> TrackEntry {
        TrackEntry {
            name: name.into(),
            artists: "Artista".into(),
            album: "Álbum".into(),
            duration_ms: 200_000,
            id: None,
            position: None,
        }
    }

    fn now_playing(title: &str, is_playing: bool) -> Playback {
        Playback {
            is_playing,
            progress_ms: 1_000,
            duration_ms: 200_000,
            title: title.into(),
            artists: "Artista".into(),
            album: "Álbum".into(),
            cover_url: None,
            item_uri: None,
            shuffle: false,
            repeat: RepeatState::Off,
            device_name: "Equipo".into(),
            device_type: "Computer".into(),
            volume: Some(50),
            fetched_at: Instant::now(),
        }
    }

    /// Deja terminar las tareas en segundo plano y recoge sus respuestas. Con
    /// el cliente de prueba (sin sesión) fallan enseguida, sin usar la red.
    pub async fn responses(app: &mut App) -> Vec<Msg> {
        let mut msgs = Vec::new();
        let wait = Duration::from_millis(300);
        while let Ok(Some(msg)) = tokio::time::timeout(wait, app.rx.recv()).await {
            msgs.push(msg);
        }
        msgs
    }

    /// Mantener pulsada R no apila cargas de playlists.
    #[tokio::test]
    async fn holding_refresh_loads_the_playlists_once() {
        let (mut app, _rx) = test_app();
        for _ in 0..5 {
            app.on_event(key(KeyCode::Char('R')));
        }
        let msgs = responses(&mut app).await;
        let loads = msgs
            .iter()
            .filter(|m| matches!(m, Msg::Playlists(_)))
            .count();
        assert_eq!(loads, 1);
        for msg in msgs {
            app.on_msg(msg);
        }
        assert!(!app.playlists_inflight, "la respuesta permite otra carga");
        app.on_event(key(KeyCode::Char('R')));
        assert!(app.playlists_inflight);
    }

    /// Al abrir otra lista (o buscar) la carga anterior se cancela: no sigue
    /// pidiendo páginas a Spotify ni responde.
    #[tokio::test]
    async fn opening_another_list_cancels_the_previous_load() {
        let (mut app, _rx) = test_app();
        app.open_library(LibraryItem::Liked);
        app.open_library(LibraryItem::Recent);
        let lists = responses(&mut app).await;
        let answered: Vec<u64> = lists
            .iter()
            .filter_map(|m| match m {
                Msg::Tracks { request, .. } => Some(*request),
                _ => None,
            })
            .collect();
        assert_eq!(answered, vec![app.request_id]);

        app.open_library(LibraryItem::Queue);
        app.search_input = "queen".into();
        app.run_search();
        let msgs = responses(&mut app).await;
        assert!(!msgs.iter().any(|m| matches!(m, Msg::Tracks { .. })));
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Msg::Search { request, .. } if *request == app.request_id))
        );
    }

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
            result: Ok(vec![song("Bohemian Rhapsody")]),
        });
        assert!(matches!(app.main, MainContent::Message(_)));
        // Volver a abrirla funciona con normalidad.
        app.open_library(LibraryItem::Playlist(0));
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Ok(vec![song("Bohemian Rhapsody")]),
        });
        assert!(matches!(
            &app.main,
            MainContent::Tracks { title, source: TrackSource::Context(_), .. } if title == "Rock"
        ));
    }

    /// El refresco tras una acción no se pierde si coincide con una consulta
    /// en curso: se hace en cuanto esta termina.
    #[tokio::test]
    async fn refresh_waits_for_the_poll_in_flight() {
        let (mut app, _rx) = test_app();
        assert!(app.poll_playback());
        app.schedule_refresh(Duration::ZERO);
        app.on_tick();
        assert!(app.refresh_at.is_some(), "el refresco debe esperar");
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(None),
        });
        assert!(!app.playback_inflight);
        app.on_tick();
        assert!(app.playback_inflight, "se consulta en cuanto se puede");
        assert!(app.refresh_at.is_none());
    }

    /// Una consulta enviada antes de pausar no vuelve a mostrar «reproduciendo».
    #[tokio::test]
    async fn poll_sent_before_a_pause_does_not_undo_it() {
        let (mut app, _rx) = test_app();
        app.playback = Some(now_playing("A", true));
        assert!(app.poll_playback());
        let before_pause = app.poll_seq;
        app.on_event(key(KeyCode::Char(' ')));
        app.on_msg(Msg::Playback {
            seq: before_pause,
            result: Ok(Some(now_playing("A", true))),
        });
        assert!(app.playback.as_ref().is_some_and(|pb| !pb.is_playing));
        assert!(!app.playback_inflight, "se puede volver a consultar");
        // Las consultas posteriores a la acción sí se aplican.
        assert!(app.poll_playback());
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(Some(now_playing("B", true))),
        });
        assert_eq!(app.playback.as_ref().map(|pb| pb.title.as_str()), Some("B"));
    }

    /// Si tras un fallo inesperado hay dos consultas en vuelo, la respuesta
    /// vieja nunca pisa a la nueva, llegue en el orden que llegue.
    #[tokio::test]
    async fn older_poll_never_overwrites_a_newer_one() {
        let (mut app, _rx) = test_app();
        let title = |app: &App| app.playback.as_ref().map(|pb| pb.title.clone());

        // La vieja llega primero: se aplica, pero sigue esperando a la nueva.
        assert!(app.poll_playback());
        let old = app.poll_seq;
        app.on_msg(crash(CrashOrigin::Other, "boom"));
        assert!(app.poll_playback());
        let new = app.poll_seq;
        app.on_msg(Msg::Playback {
            seq: old,
            result: Ok(Some(now_playing("Vieja", true))),
        });
        assert!(app.playback_inflight);
        app.on_msg(Msg::Playback {
            seq: new,
            result: Ok(Some(now_playing("Nueva", true))),
        });
        assert_eq!(title(&app).as_deref(), Some("Nueva"));
        assert!(!app.playback_inflight);

        // La nueva llega primero: la vieja se ignora.
        assert!(app.poll_playback());
        let old = app.poll_seq;
        app.on_msg(crash(CrashOrigin::Other, "boom"));
        assert!(app.poll_playback());
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(Some(now_playing("Nueva 2", true))),
        });
        app.on_msg(Msg::Playback {
            seq: old,
            result: Ok(Some(now_playing("Vieja 2", true))),
        });
        assert_eq!(title(&app).as_deref(), Some("Nueva 2"));
        assert!(!app.playback_inflight);
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

    const ID_A: &str = "4u7EnebtmKWzUH433cf5Qv";
    const ID_B: &str = "5ChkMS8OtdzJeqyybCc9R5";

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    /// Canción reproducible con su posición en la lista.
    fn track(name: &str, id: &str, position: Option<u32>) -> TrackEntry {
        TrackEntry {
            id: rspotify::model::TrackId::from_id(id.to_string())
                .ok()
                .map(rspotify::model::PlayableId::Track),
            position,
            ..song(name)
        }
    }

    fn playlist_with_id(name: &str, id: &str) -> PlaylistEntry {
        PlaylistEntry {
            id: PlaylistId::from_id(id.to_string()).unwrap(),
            ..playlist(name)
        }
    }

    fn tiny_image() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
    }

    /// Muestra `tracks` como la lista cargada de la petición en curso.
    fn deliver(app: &mut App, tracks: Vec<TrackEntry>) {
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Ok(tracks),
        });
    }

    fn status(app: &App) -> Option<&str> {
        app.visible_status().map(|s| s.text.as_str())
    }

    /// Con la carátula grande, buscar o abrir una lista la cierra: lo que se
    /// carga tiene que verse.
    #[tokio::test]
    async fn opening_a_list_closes_the_big_cover() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('c')));
        assert!(app.big_cover);
        app.on_event(key(KeyCode::Char('/')));
        for c in "queen".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_event(key(KeyCode::Enter));
        assert!(!app.big_cover);
        assert_eq!(app.focus, Focus::Main);

        app.big_cover = true;
        app.focus = Focus::Library;
        app.on_event(key(KeyCode::Enter));
        assert!(!app.big_cover, "abrir «Tus me gusta» también la cierra");

        app.big_cover = true;
        app.open_context(album("Bad"));
        assert!(!app.big_cover, "abrir un álbum también la cierra");
    }

    /// La lista que tapa la carátula grande no se reproduce ni se mueve a
    /// ciegas; al cerrar la carátula vuelve a responder.
    #[tokio::test]
    async fn hidden_list_ignores_play_queue_and_moves() {
        let (mut app, _rx) = test_app();
        app.main = MainContent::Tracks {
            title: "Lista".into(),
            source: TrackSource::Uris,
            tracks: vec![track("A", ID_A, None), track("B", ID_B, None)],
        };
        app.main_state.select(Some(0));
        app.focus = Focus::Main;
        app.big_cover = true;
        for code in [KeyCode::Char('j'), KeyCode::Enter, KeyCode::Char('a')] {
            app.on_event(key(code));
        }
        assert_eq!(app.main_state.selected(), Some(0));
        assert_eq!(status(&app), None);
        assert_eq!(app.fresh_poll, 0, "no se envió ninguna acción");
        app.on_event(key(KeyCode::Char('c')));
        app.on_event(key(KeyCode::Char('j')));
        assert_eq!(app.main_state.selected(), Some(1));
    }

    /// Tras saltar de canción con la cola abierta, la cola se recarga en
    /// segundo plano: sin quitar el foco a la biblioteca ni mover la
    /// selección, y solo si la acción fue bien.
    #[tokio::test]
    async fn queue_reload_keeps_focus_and_selection() {
        let (mut app, _rx) = test_app();
        app.playlists = vec![playlist("Rock")];
        app.open_library(LibraryItem::Queue);
        deliver(&mut app, vec![song("A"), song("B"), song("C")]);
        app.main_state.select(Some(2));
        app.on_event(key(KeyCode::Tab));
        app.library_state.select(Some(FIXED_LIBRARY.len()));

        app.on_msg(Msg::ActionDone {
            done: None,
            refresh_queue: true,
            result: Err(anyhow::anyhow!("sin dispositivo")),
        });
        assert!(
            app.list_reload_at.is_none(),
            "una acción fallida no recarga"
        );
        app.on_msg(Msg::ActionDone {
            done: None,
            refresh_queue: true,
            result: Ok(()),
        });
        app.on_tick();
        assert!(
            app.pending_list.is_none(),
            "espera a que Spotify lo refleje"
        );
        app.list_reload_at = Some(Instant::now());
        app.on_tick();
        assert!(
            app.pending_list
                .as_ref()
                .is_some_and(|p| p.mode == LoadMode::Reload)
        );
        assert_eq!(app.focus, Focus::Library);
        assert!(
            matches!(app.main, MainContent::Tracks { .. }),
            "sigue a la vista"
        );

        deliver(&mut app, vec![song("B"), song("C")]);
        assert_eq!(
            app.main_state.selected(),
            Some(1),
            "acotada al nuevo tamaño"
        );
        assert_eq!(app.focus, Focus::Library);
        assert_eq!(app.library_state.selected(), Some(FIXED_LIBRARY.len()));

        // Si la recarga falla, se sigue viendo la cola que había.
        app.reload_active();
        app.on_msg(Msg::Tracks {
            request: app.request_id,
            result: Err(anyhow::anyhow!("sin red")),
        });
        assert!(matches!(&app.main, MainContent::Tracks { tracks, .. } if tracks.len() == 2));
        assert!(status(&app).is_some_and(|s| s.contains("sin red")));

        // Enter en la biblioteca abre la playlist señalada.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.active_library, Some(LibraryItem::Playlist(0)));
    }

    /// R/F5 también recarga la lista que se ve, y con la cola (o las
    /// recientes) abierta un cambio de canción programa su recarga.
    #[tokio::test]
    async fn refresh_and_song_changes_reload_the_visible_list() {
        let (mut app, _rx) = test_app();
        app.open_library(LibraryItem::Queue);
        // Deja terminar la carga real (falla sin sesión) y muestra una lista.
        responses(&mut app).await;
        deliver(&mut app, vec![song("A"), song("B")]);
        app.main_state.select(Some(1));
        app.on_event(key(KeyCode::Char('R')));
        assert!(
            app.pending_list
                .as_ref()
                .is_some_and(|p| p.mode == LoadMode::Reload)
        );
        assert_eq!(app.main_state.selected(), Some(1));

        let playing = |uri: &str| Playback {
            item_uri: Some(uri.into()),
            ..now_playing("A", true)
        };
        app.set_playback(Some(playing("spotify:track:a")));
        app.list_reload_at = None;
        app.set_playback(Some(playing("spotify:track:a")));
        assert!(app.list_reload_at.is_none(), "la misma canción no recarga");
        app.set_playback(Some(playing("spotify:track:b")));
        assert!(app.list_reload_at.is_some());

        // Abrir otra lista olvida la recarga programada, y en «Tus me gusta»
        // un cambio de canción no recarga nada.
        app.open_library(LibraryItem::Liked);
        assert!(app.list_reload_at.is_none());
        app.set_playback(Some(playing("spotify:track:c")));
        assert!(app.list_reload_at.is_none());
    }

    /// Al recargar las playlists cambia su orden: la marca de activa y el
    /// cursor siguen a la misma playlist, por su id.
    #[tokio::test]
    async fn reloaded_playlists_keep_the_active_one_and_the_cursor() {
        let (mut app, _rx) = test_app();
        let a = playlist_with_id("A", "37i9dQZF1DXcBWIGoYBM5A");
        let b = playlist_with_id("B", "37i9dQZF1DXcBWIGoYBM5B");
        let c = playlist_with_id("C", "37i9dQZF1DXcBWIGoYBM5C");
        let new = playlist_with_id("Nueva", "37i9dQZF1DXcBWIGoYBM5N");
        let fixed = FIXED_LIBRARY.len();
        app.playlists = vec![a.clone(), b.clone(), c.clone()];
        app.active_library = Some(LibraryItem::Playlist(1));
        app.library_state.select(Some(fixed + 2));

        app.on_msg(Msg::Playlists(Ok(vec![
            new.clone(),
            a.clone(),
            b.clone(),
            c.clone(),
        ])));
        assert_eq!(app.active_library, Some(LibraryItem::Playlist(2)));
        assert_eq!(app.library_state.selected(), Some(fixed + 3));

        // B y C desaparecen: B deja de estar marcada y el cursor no se sale.
        app.on_msg(Msg::Playlists(Ok(vec![a.clone()])));
        assert_eq!(app.active_library, None);
        assert_eq!(app.library_state.selected(), Some(fixed));

        // Las entradas fijas no se mueven.
        app.active_library = Some(LibraryItem::Queue);
        app.library_state.select(Some(1));
        app.on_msg(Msg::Playlists(Ok(vec![new, a])));
        assert_eq!(app.active_library, Some(LibraryItem::Queue));
        assert_eq!(app.library_state.selected(), Some(1));
    }

    /// Enter mientras se buscan dispositivos (o sin ninguno) no cierra la
    /// ventana con un aviso engañoso, y 'd' vuelve a buscar.
    #[tokio::test]
    async fn devices_popup_waits_for_a_device_and_d_searches_again() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('d')));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        assert_eq!(status(&app), None);

        app.on_msg(Msg::Devices(Ok(Vec::new())));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            app.popup,
            Popup::Devices {
                devices: Some(_),
                ..
            }
        ));
        assert_eq!(status(&app), None);

        app.on_event(key(KeyCode::Char('d')));
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(app.popup, Popup::None));
    }

    /// Si falla la descarga de la carátula nueva no se queda la anterior: se
    /// ve el logo y se reintenta pasado un tiempo, no en cada sondeo.
    #[tokio::test]
    async fn failed_cover_download_shows_the_logo_and_retries_later() {
        let (mut app, _rx) = test_app();
        // Puerto local cerrado: si la descarga llegara a lanzarse, falla ya.
        let url = "http://127.0.0.1:9/carátula.jpg".to_string();
        let with_cover = Playback {
            cover_url: Some(url.clone()),
            ..now_playing("B", true)
        };
        app.set_cover_image(tiny_image());
        app.set_playback(Some(with_cover.clone()));
        assert!(app.has_cover(), "mientras descarga se ve la anterior");

        app.on_msg(Msg::Cover {
            url: url.clone(),
            result: Err(anyhow::anyhow!("tiempo agotado")),
        });
        assert!(!app.has_cover());
        assert!(app.cover.protocol_type().is_none());

        app.set_playback(Some(with_cover.clone()));
        assert!(app.cover_retry_at.is_some(), "aún no toca reintentar");
        app.cover_retry_at = Some(Instant::now());
        app.set_playback(Some(with_cover));
        assert!(app.cover_retry_at.is_none(), "se vuelve a descargar");
        assert_eq!(app.cover_url.as_deref(), Some(url.as_str()));
    }

    /// Si falla el redimensionado de la carátula (o sufre un panic), se
    /// vuelve a preparar una vez; si falla otra vez, se muestra el logo.
    #[tokio::test]
    async fn failed_cover_resize_is_rebuilt_once() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let _ = start_cover_resize(&mut app, &mut rx);
        assert!(app.has_cover(), "redimensionando no es «sin carátula»");
        app.on_msg(encode_error(&app));
        assert!(app.cover.protocol_type().is_some(), "se vuelve a preparar");
        assert_eq!(status(&app), None);

        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert!(!app.has_cover(), "al segundo fallo, el logo");
        assert!(status(&app).is_some_and(|s| s.contains("carátula")));

        app.set_cover_image(tiny_image());
        let _ = start_cover_resize(&mut app, &mut rx);
        let origin = CrashOrigin::Resize(app.cover_generation());
        app.on_msg(crash(origin, "panic al codificar"));
        assert!(app.cover.protocol_type().is_some());
    }

    /// Error de redimensionado de la carátula de la generación actual.
    fn encode_error(app: &App) -> Msg {
        resize_error(app.cover_generation())
    }

    fn resize_error(generation: u64) -> Msg {
        Msg::Resized {
            generation,
            result: Err(Errors::Sixel("fallo simulado".into())),
        }
    }

    /// Como al dibujar: la imagen sale del protocolo para redimensionarse.
    /// Devuelve la petición, para poder completarla.
    fn start_cover_resize(
        app: &mut App,
        rx: &mut UnboundedReceiver<ResizeRequest>,
    ) -> ResizeRequest {
        use ratatui::layout::Size;
        use ratatui_image::{Resize, ResizeEncodeRender};
        app.cover.resize_encode(&Resize::Fit(None), Size::new(8, 4));
        assert!(app.cover.protocol_type().is_none());
        rx.try_recv().expect("se pidió el redimensionado")
    }

    /// Completa `request` como el hilo de redimensionado del bucle principal.
    fn finish_resize(request: ResizeRequest, generation: u64) -> Msg {
        Msg::Resized {
            generation,
            result: request.resize_encode(),
        }
    }

    /// El error tardío del redimensionado de la carátula anterior no toca la
    /// nueva: ni cancela su redimensionado ni gasta su reintento.
    #[tokio::test]
    async fn stale_resize_error_does_not_spend_the_retry() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let _old_request = start_cover_resize(&mut app, &mut rx);
        let old = app.cover_generation();

        app.set_cover_image(tiny_image());
        let request = start_cover_resize(&mut app, &mut rx);
        let current = app.cover_generation();
        app.on_msg(resize_error(old));
        assert!(
            app.cover.protocol_type().is_none(),
            "sigue redimensionándose"
        );
        assert_eq!(app.cover_rebuilds, 0);
        assert_eq!(status(&app), None);

        app.on_msg(finish_resize(request, current));
        assert!(
            app.cover.protocol_type().is_some(),
            "la respuesta sana vale"
        );
        // El reintento sigue disponible para un fallo de verdad.
        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert!(app.has_cover() && app.cover.protocol_type().is_some());
    }

    /// Un *panic* ajeno (u obsoleto) mientras se redimensiona la carátula no
    /// la vuelve a preparar: el redimensionado en curso sigue valiendo.
    #[tokio::test]
    async fn unrelated_crash_does_not_rebuild_the_cover() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let request = start_cover_resize(&mut app, &mut rx);
        let current = app.cover_generation();
        app.on_msg(crash(CrashOrigin::Other, "fallo en otra tarea"));
        app.on_msg(crash(
            CrashOrigin::Resize(current - 1),
            "redimensionado viejo",
        ));
        app.report_crash("al dibujar: fallo simulado");
        assert!(app.cover.protocol_type().is_none());
        assert_eq!(app.cover_rebuilds, 0);

        app.on_msg(finish_resize(request, current));
        assert!(app.cover.protocol_type().is_some());
    }

    /// Un redimensionado que sale bien devuelve el reintento: un fallo
    /// pasajero posterior (al cambiar el tamaño de la ventana) no quita la
    /// carátula.
    #[tokio::test]
    async fn successful_resize_restores_the_retry() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert_eq!(app.cover_rebuilds, 1);

        let request = start_cover_resize(&mut app, &mut rx);
        app.on_msg(finish_resize(request, app.cover_generation()));
        assert_eq!(app.cover_rebuilds, 0);

        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert!(app.has_cover() && app.cover.protocol_type().is_some());
        assert_eq!(status(&app), None);
    }

    /// Enter sobre un archivo local de una playlist no la reproduce desde el
    /// principio diciendo que suena el archivo.
    #[tokio::test]
    async fn local_file_in_a_playlist_is_not_played() {
        let (mut app, _rx) = test_app();
        app.playlists = vec![playlist("Rock")];
        app.open_library(LibraryItem::Playlist(0));
        deliver(
            &mut app,
            vec![track("A", ID_A, Some(0)), song("archivo local.mp3")],
        );
        app.main_state.select(Some(1));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(status(&app), Some("Esta canción no se puede reproducir"));
        assert_eq!(app.fresh_poll, 0, "no se envió ninguna acción");
    }

    /// Una canción repetida en el contexto empieza en la fila elegida (por
    /// posición); el resto, por su URI.
    #[test]
    fn repeated_song_starts_at_its_own_position() {
        let uri = |id: &str| format!("spotify:track:{id}");
        let tracks = vec![
            track("A", ID_A, Some(0)),
            track("B", ID_B, Some(1)),
            track("A", ID_A, Some(5)),
        ];
        assert_eq!(context_start(&tracks, 2), Some(ContextStart::Position(5)));
        assert_eq!(context_start(&tracks, 0), Some(ContextStart::Position(0)));
        assert_eq!(
            context_start(&tracks, 1),
            Some(ContextStart::Uri(uri(ID_B)))
        );
        // Repetida, pero sin posición fiable: por URI.
        let unknown = vec![track("A", ID_A, None), track("A", ID_A, None)];
        assert_eq!(
            context_start(&unknown, 1),
            Some(ContextStart::Uri(uri(ID_A)))
        );
        assert_eq!(context_start(&[song("local")], 0), None);
        assert_eq!(context_start(&tracks, 99), None);
    }

    #[test]
    fn reload_keeps_the_row_within_the_new_list() {
        assert_eq!(kept_row(Some(2), 3), 2);
        assert_eq!(kept_row(Some(7), 3), 2);
        assert_eq!(kept_row(None, 3), 0);
        assert_eq!(kept_row(Some(4), 0), 0);
    }

    /// En el buscador, los atajos con Ctrl o Alt no escriben su letra; AltGr
    /// (Ctrl+Alt) y Mayúsculas sí escriben, y Ctrl+W borra una palabra.
    #[tokio::test]
    async fn search_ignores_shortcuts_but_accepts_altgr() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('/')));
        for c in "queen live".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_event(key_with(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.on_event(key_with(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.on_event(key_with(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(app.search_input, "queen live");
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        app.on_event(key_with(KeyCode::Char('@'), altgr));
        app.on_event(key_with(KeyCode::Char('Q'), KeyModifiers::SHIFT));
        assert_eq!(app.search_input, "queen live@Q");
        app.on_event(key_with(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.search_input, "queen ");
        app.on_event(key_with(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(app.search_input, "queen");
        assert!(matches!(app.popup, Popup::Search));
    }

    #[test]
    fn ctrl_w_deletes_the_last_word() {
        for (before, after) in [
            ("queen live", "queen "),
            ("queen live   ", "queen "),
            ("queen", ""),
            ("", ""),
            ("   ", ""),
            ("東京 事変", "東京 "),
            ("a\u{3000}b", "a\u{3000}"),
        ] {
            let mut text = before.to_string();
            delete_last_word(&mut text);
            assert_eq!(text, after, "desde «{before}»");
        }
    }

    /// Al pegar varias líneas o una fila con tabuladores, las palabras no se
    /// juntan.
    #[test]
    fn pasted_lines_and_tabs_become_spaces() {
        assert_eq!(
            pasted_text("Bohemian Rhapsody\nQueen"),
            "Bohemian Rhapsody Queen"
        );
        assert_eq!(pasted_text("a\tb\r\nc\n"), "a b c");
        assert_eq!(pasted_text("en\n"), "en");
        assert_eq!(pasted_text("\n\t\u{0}"), "");
    }
}

#[cfg(test)]
mod robustness {
    use super::tests::{crash, responses, test_app};
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
        let seq = app.poll_seq.wrapping_sub((rng.next() % 2) as u64);
        match rng.next() % 8 {
            0 => Msg::Playback {
                seq,
                result: Ok(None),
            },
            1 => Msg::Playback { seq, result: err() },
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
            _ if rng.next().is_multiple_of(2) => {
                crash(CrashOrigin::List(request), "panic simulado")
            }
            _ => crash(CrashOrigin::Other, "panic simulado"),
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
        spawn_guarded(tx.clone(), CrashOrigin::List(7), async {
            panic!("la API devolvió algo imposible")
        });
        match rx.recv().await {
            Some(Msg::Crashed {
                origin: CrashOrigin::List(7),
                reason,
            }) => assert!(reason.contains("imposible")),
            _ => panic!("se esperaba Msg::Crashed de la lista 7"),
        }
        spawn_blocking_guarded(tx, CrashOrigin::Resize(3), || {
            panic!("fallo al redimensionar")
        });
        assert!(matches!(
            rx.recv().await,
            Some(Msg::Crashed {
                origin: CrashOrigin::Resize(3),
                ..
            })
        ));
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

    /// El *panic* de la tarea de una lista llega etiquetado con su petición.
    #[tokio::test]
    async fn list_task_panics_are_attributed_to_their_request() {
        let (mut app, _rx) = test_app();
        app.begin_request("Cargando…".into());
        app.spawn_list(|_| async { panic!("página imposible") });
        let request = app.request_id;
        let msgs = responses(&mut app).await;
        assert!(msgs.iter().any(|m| matches!(
            m,
            Msg::Crashed { origin: CrashOrigin::List(r), reason }
                if *r == request && reason.contains("imposible")
        )));
        for msg in msgs {
            app.on_msg(msg);
        }
        assert!(matches!(app.main, MainContent::Message(_)));
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
