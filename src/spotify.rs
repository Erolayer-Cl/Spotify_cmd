//! Capa fina sobre `rspotify`: convierte los modelos de la API en estructuras
//! sencillas para la interfaz y traduce los errores a mensajes legibles.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::{StreamExt, TryFutureExt, TryStreamExt};
use rspotify::{
    AuthCodePkceSpotify, ClientError,
    http::HttpError,
    model::{
        AdditionalType, AlbumId, CurrentPlaybackContext, EpisodeId, FullTrack, Image, Market,
        Offset, Page, PlayableItem, PlaylistId, RepeatState, SearchResult, SearchType,
        SimplifiedArtist, SimplifiedPlaylist, TrackId,
    },
    prelude::*,
};
use std::future::Future;

pub type Client = AuthCodePkceSpotify;

/// Límite de resultados por búsqueda. Las apps en "Development mode" de
/// Spotify aceptan como máximo 10 desde febrero de 2026.
const SEARCH_LIMIT: u32 = 10;
/// Máximo de URIs que se envían al reproducir una lista sin contexto.
const MAX_URIS: usize = 100;

/// Estado de reproducción simplificado.
#[derive(Debug, Clone)]
pub struct Playback {
    pub is_playing: bool,
    pub progress_ms: u64,
    pub duration_ms: u64,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub cover_url: Option<String>,
    pub item_uri: Option<String>,
    pub shuffle: bool,
    pub repeat: RepeatState,
    pub device_name: String,
    pub device_type: String,
    pub volume: Option<u32>,
    pub fetched_at: Instant,
}

impl Playback {
    /// Progreso estimado ahora mismo, interpolando desde la última consulta.
    pub fn current_progress_ms(&self) -> u64 {
        let mut p = self.progress_ms;
        if self.is_playing {
            p += self.fetched_at.elapsed().as_millis() as u64;
        }
        p.min(self.duration_ms)
    }
}

/// Canción (o episodio) mostrada en una lista.
#[derive(Debug, Clone)]
pub struct TrackEntry {
    pub name: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u64,
    pub id: Option<PlayableId<'static>>,
    /// Posición real en su playlist o álbum, si se conoce. Sirve para empezar
    /// por la fila elegida cuando la canción aparece varias veces.
    pub position: Option<u32>,
}

impl TrackEntry {
    pub fn uri(&self) -> Option<String> {
        self.id.as_ref().map(|id| id.uri())
    }
}

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub id: PlaylistId<'static>,
    pub name: String,
    pub owner: String,
    pub total: u32,
}

/// Fila genérica de resultados de búsqueda de álbumes, artistas o playlists.
#[derive(Debug, Clone)]
pub struct ContextEntry {
    pub name: String,
    pub detail: String,
    pub id: PlayContextId<'static>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Tracks,
    Albums,
    Artists,
    Playlists,
}

impl SearchKind {
    pub const ALL: [SearchKind; 4] = [
        SearchKind::Tracks,
        SearchKind::Albums,
        SearchKind::Artists,
        SearchKind::Playlists,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SearchKind::Tracks => "Canciones",
            SearchKind::Albums => "Álbumes",
            SearchKind::Artists => "Artistas",
            SearchKind::Playlists => "Playlists",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone)]
pub enum SearchResults {
    Tracks(Vec<TrackEntry>),
    Contexts(Vec<ContextEntry>),
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Conversión de modelos
// ---------------------------------------------------------------------------

fn join_artists<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}

/// Elige una imagen de tamaño medio (~300 px): suficiente nitidez sin descargar
/// la de 640 px.
fn pick_image(images: &[Image]) -> Option<String> {
    images
        .iter()
        .filter(|i| i.width.unwrap_or(300) >= 250)
        .min_by_key(|i| i.width.unwrap_or(300))
        .or_else(|| images.first())
        .map(|i| i.url.clone())
}

/// Nombres de los artistas separados por comas.
fn artist_names(artists: &[SimplifiedArtist]) -> String {
    join_artists(artists.iter().map(|a| a.name.as_str()))
}

/// Duración en milisegundos; una negativa (no debería llegar) cuenta como 0.
fn millis(d: chrono::Duration) -> u64 {
    d.num_milliseconds().max(0) as u64
}

fn track_entry(t: &FullTrack) -> TrackEntry {
    track_info(t).into()
}

/// Lo que la interfaz necesita de una canción o episodio, venga como venga.
struct ItemInfo {
    title: String,
    artists: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
    id: Option<PlayableId<'static>>,
}

impl From<ItemInfo> for TrackEntry {
    fn from(info: ItemInfo) -> Self {
        TrackEntry {
            name: info.title,
            artists: info.artists,
            album: info.album,
            duration_ms: info.duration_ms,
            id: info.id,
            position: None,
        }
    }
}

/// Información de una canción: la usan las listas y el estado de reproducción.
fn track_info(t: &FullTrack) -> ItemInfo {
    ItemInfo {
        title: t.name.clone(),
        artists: artist_names(&t.artists),
        album: t.album.name.clone(),
        cover_url: pick_image(&t.album.images),
        duration_ms: millis(t.duration),
        id: t.id.clone().map(PlayableId::Track),
    }
}

/// Extrae la información de cualquier elemento reproducible. Si `rspotify` no
/// lo entendió (`Unknown`, p. ej. porque Spotify cambió el formato), se lee el
/// JSON crudo de forma tolerante en vez de descartarlo.
fn item_info(item: &PlayableItem) -> Option<ItemInfo> {
    match item {
        PlayableItem::Track(t) => Some(track_info(t)),
        PlayableItem::Episode(e) => Some(ItemInfo {
            title: e.name.clone(),
            artists: e.show.name.clone(),
            album: "Podcast".into(),
            cover_url: pick_image(&e.images).or_else(|| pick_image(&e.show.images)),
            duration_ms: millis(e.duration),
            id: Some(PlayableId::Episode(e.id.clone())),
        }),
        PlayableItem::Unknown(raw) => lenient_info(raw),
    }
}

fn lenient_info(v: &serde_json::Value) -> Option<ItemInfo> {
    let text = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_str()).map(str::to_string);
    let show = v.get("show");
    let artists = match v.get("artists").and_then(|a| a.as_array()) {
        Some(list) => join_artists(list.iter().filter_map(|a| a.get("name")?.as_str())),
        None => text(show.and_then(|s| s.get("name"))).unwrap_or_default(),
    };
    let album = text(v.get("album").and_then(|a| a.get("name"))).unwrap_or_else(|| {
        if show.is_some() {
            "Podcast".into()
        } else {
            String::new()
        }
    });
    let images = v
        .get("album")
        .and_then(|a| a.get("images"))
        .or_else(|| v.get("images"))
        .and_then(|imgs| serde_json::from_value::<Vec<Image>>(imgs.clone()).ok())
        .unwrap_or_default();
    Some(ItemInfo {
        title: text(v.get("name"))?,
        artists,
        album,
        cover_url: pick_image(&images),
        duration_ms: v.get("duration_ms").and_then(|d| d.as_u64()).unwrap_or(0),
        id: v
            .get("uri")
            .and_then(|u| u.as_str())
            .and_then(playable_id_from_uri),
    })
}

/// `spotify:track:…` o `spotify:episode:…` → id reproducible.
fn playable_id_from_uri(uri: &str) -> Option<PlayableId<'static>> {
    if let Ok(id) = TrackId::from_uri(uri) {
        return Some(PlayableId::Track(id.into_static()));
    }
    EpisodeId::from_uri(uri)
        .ok()
        .map(|id| PlayableId::Episode(id.into_static()))
}

fn playable_entry(item: &PlayableItem) -> Option<TrackEntry> {
    item_info(item).map(Into::into)
}

fn playlist_entry(p: SimplifiedPlaylist) -> PlaylistEntry {
    PlaylistEntry {
        owner: p
            .owner
            .display_name
            .clone()
            .unwrap_or_else(|| p.owner.id.id().to_string()),
        total: p.items.total,
        name: p.name,
        id: p.id,
    }
}

// ---------------------------------------------------------------------------
// Errores
// ---------------------------------------------------------------------------

/// Error de la API con el código HTTP, para que la interfaz pueda reaccionar
/// (por ejemplo, esperar lo que indique `Retry-After` ante un 429).
#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
impl ApiError {
    pub fn rate_limited(retry_after: Option<Duration>) -> Self {
        ApiError {
            status: Some(429),
            retry_after,
            message: status_message(429, Kind::Query, retry_after),
        }
    }
}

/// `true` si el error es de conexión (sin red, DNS, tiempo agotado), no una
/// respuesta de Spotify.
pub fn is_network_error(err: &ClientError) -> bool {
    matches!(err, ClientError::Http(http) if matches!(http.as_ref(), HttpError::Client(_)))
}

/// Aviso cuando no se puede hablar con Spotify (sin red, DNS...).
const OFFLINE: &str = "sin conexión con Spotify: revisa tu red (se reintentará solo)";

/// Qué tipo de petición falló: cambia el significado de algunos códigos.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Query,
    Player,
    /// Canciones de una playlist: en modo desarrollo, Spotify solo las da
    /// para las playlists del usuario o colaborativas (403 con el resto).
    PlaylistItems,
}

/// Traduce un error de la API a un mensaje en español con una pista útil.
fn friendly(err: ClientError, kind: Kind) -> anyhow::Error {
    let ClientError::Http(http) = &err else {
        return anyhow!("{err}");
    };
    let resp = match http.as_ref() {
        HttpError::StatusCode(resp) => resp,
        HttpError::Client(e) => {
            let message = if e.is_timeout() {
                "Spotify no respondió a tiempo; se reintentará"
            } else {
                OFFLINE
            };
            return anyhow::Error::new(ApiError {
                status: None,
                retry_after: None,
                message: message.into(),
            });
        }
        #[allow(unreachable_patterns)]
        _ => return anyhow!("error de red: {err}"),
    };
    let status = resp.status().as_u16();
    let retry_after = retry_after(resp);
    let message = status_message(status, kind, retry_after);
    anyhow::Error::new(ApiError {
        status: Some(status),
        retry_after,
        message,
    })
}

/// Segundos que pide esperar la cabecera `Retry-After` de una respuesta.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn status_message(status: u16, kind: Kind, retry_after: Option<Duration>) -> String {
    match (status, kind) {
        (401, _) => {
            "sesión caducada o inválida (ejecuta `spotify_cmd --logout` y vuelve a entrar)".into()
        }
        (403, Kind::Player) => {
            "Spotify denegó la acción (los controles requieren Spotify Premium, \
                                o la acción no está permitida ahora)"
                .into()
        }
        (403, Kind::PlaylistItems) => {
            "Spotify solo deja ver las canciones de tus playlists o de las colaborativas".into()
        }
        (403, _) => "Spotify no permite este acceso a la app (en modo desarrollo solo se \
                     ven tus propias playlists; revisa también que tu usuario esté en \
                     «User Management» de la app)"
            .into(),
        (404, Kind::Player) => {
            "no hay ningún dispositivo activo: abre Spotify en algún dispositivo \
                                o pulsa 'd' para elegir uno"
                .into()
        }
        (404, _) => "Spotify no encontró ese contenido".into(),
        (429, _) => match retry_after {
            Some(d) => format!(
                "demasiadas peticiones a Spotify, reintentando en {} s",
                d.as_secs()
            ),
            None => "demasiadas peticiones a Spotify, espera unos segundos".into(),
        },
        (500..=599, _) => format!("Spotify tiene problemas ahora mismo (error {status})"),
        (code, _) => format!("Spotify respondió con el código {code}"),
    }
}

trait Friendly<T> {
    fn friendly(self) -> Result<T>;
    fn friendly_player(self) -> Result<T>;
}

impl<T> Friendly<T> for std::result::Result<T, ClientError> {
    fn friendly(self) -> Result<T> {
        self.map_err(|e| friendly(e, Kind::Query))
    }
    fn friendly_player(self) -> Result<T> {
        self.map_err(|e| friendly(e, Kind::Player))
    }
}

// ---------------------------------------------------------------------------
// Consultas
// ---------------------------------------------------------------------------

pub async fn playback(sp: &Client) -> Result<Option<Playback>> {
    let types = [AdditionalType::Track, AdditionalType::Episode];
    let ctx = sp.current_playback(None, Some(&types)).await.friendly()?;
    Ok(ctx.map(playback_from_context))
}

/// Convierte la respuesta de Spotify en el estado que muestra la interfaz.
/// Función pura: se prueba con respuestas reales de la API.
fn playback_from_context(ctx: CurrentPlaybackContext) -> Playback {
    let info = ctx.item.as_ref().and_then(item_info);
    let (title, artists, album, cover_url, duration_ms, item_uri) = match info {
        Some(i) => (
            i.title,
            i.artists,
            i.album,
            i.cover_url,
            i.duration_ms,
            i.id.map(|id| id.uri()),
        ),
        None => (
            "(sin información)".into(),
            String::new(),
            String::new(),
            None,
            0,
            None,
        ),
    };
    Playback {
        is_playing: ctx.is_playing,
        progress_ms: ctx.progress.map(millis).unwrap_or(0),
        duration_ms,
        title,
        artists,
        album,
        cover_url,
        item_uri,
        shuffle: ctx.shuffle_state,
        repeat: ctx.repeat_state,
        device_name: ctx.device.name,
        device_type: format!("{:?}", ctx.device._type),
        volume: ctx.device.volume_percent,
        fetched_at: Instant::now(),
    }
}

/// Elementos por página (el máximo que acepta Spotify en estas rutas).
const PAGE_SIZE: u32 = 50;
/// Páginas que se piden a la vez.
const PARALLEL_PAGES: usize = 4;
/// Límites para que las bibliotecas enormes carguen rápido.
const MAX_PLAYLISTS: u32 = 500;
const MAX_PLAYLIST_TRACKS: u32 = 2000;
const MAX_SAVED_TRACKS: u32 = 1000;

/// Descarga una lista paginada: primero la página 0 (que trae el total) y
/// después el resto en paralelo, conservando el orden. Mucho más rápido que
/// pedirlas una detrás de otra. Cada página va con el desplazamiento con el
/// que se pidió.
async fn fetch_pages<T, F, Fut>(max_items: u32, fetch_page: F) -> Result<Vec<(u32, Page<T>)>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Page<T>>>,
    T: serde::de::DeserializeOwned,
{
    let first = fetch_page(0).await?;
    let wanted = first.total.min(max_items);

    let offsets = (PAGE_SIZE..wanted).step_by(PAGE_SIZE as usize);
    let rest: Vec<(u32, Page<T>)> = futures::stream::iter(offsets)
        .map(|offset| fetch_page(offset).map_ok(move |page| (offset, page)))
        .buffered(PARALLEL_PAGES)
        .try_collect()
        .await?;
    Ok(std::iter::once((0, first)).chain(rest).collect())
}

/// Como [`fetch_pages`], pero solo los elementos, en orden.
async fn fetch_paged<T, F, Fut>(max_items: u32, fetch_page: F) -> Result<Vec<T>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Page<T>>>,
    T: serde::de::DeserializeOwned,
{
    let pages = fetch_pages(max_items, fetch_page).await?;
    Ok(pages
        .into_iter()
        .flat_map(|(_, page)| page.items)
        .take(max_items as usize)
        .collect())
}

/// Los elementos de las páginas con su posición real en la lista. Spotify
/// omite los elementos nulos sin avisar: si a una página le falta alguno,
/// las posiciones de esa página no son fiables y quedan en `None`.
fn with_positions<T>(pages: Vec<(u32, Page<T>)>, max_items: u32) -> Vec<(Option<u32>, T)>
where
    T: serde::de::DeserializeOwned,
{
    pages
        .into_iter()
        .flat_map(|(offset, page)| {
            let expected = page.total.saturating_sub(offset).min(PAGE_SIZE);
            let exact = page.items.len() == expected as usize;
            page.items.into_iter().enumerate().map(move |(i, item)| {
                let position = offset.checked_add(i as u32).filter(|_| exact);
                (position, item)
            })
        })
        .take(max_items as usize)
        .collect()
}

pub async fn playlists(sp: &Client) -> Result<Vec<PlaylistEntry>> {
    let items = fetch_paged(MAX_PLAYLISTS, |offset| async move {
        sp.current_user_playlists_manual(Some(PAGE_SIZE), Some(offset))
            .await
            .friendly()
    })
    .await?;
    Ok(items.into_iter().map(playlist_entry).collect())
}

pub async fn playlist_tracks(sp: &Client, id: PlaylistId<'static>) -> Result<Vec<TrackEntry>> {
    let id = &id;
    let pages = fetch_pages(MAX_PLAYLIST_TRACKS, |offset| async move {
        sp.playlist_items_manual(
            id.as_ref(),
            None,
            Some(Market::FromToken),
            Some(PAGE_SIZE),
            Some(offset),
        )
        .await
        .map_err(|e| friendly(e, Kind::PlaylistItems))
    })
    .await?;
    // La posición se calcula antes de descartar los elementos ilegibles.
    Ok(with_positions(pages, MAX_PLAYLIST_TRACKS)
        .into_iter()
        .filter_map(|(position, i)| {
            let entry = i.item.as_ref().and_then(playable_entry)?;
            Some(TrackEntry { position, ..entry })
        })
        .collect())
}

pub async fn album_tracks(
    sp: &Client,
    id: AlbumId<'static>,
    album_name: &str,
) -> Result<Vec<TrackEntry>> {
    let items: Vec<_> = sp
        .album_track(id, Some(Market::FromToken))
        .try_collect()
        .await
        .friendly()?;
    Ok(items
        .into_iter()
        .enumerate()
        .map(|(i, t)| TrackEntry {
            artists: artist_names(&t.artists),
            album: album_name.to_string(),
            duration_ms: millis(t.duration),
            name: t.name,
            id: t.id.map(PlayableId::Track),
            position: u32::try_from(i).ok(),
        })
        .collect())
}

/// Canciones guardadas ("Tus me gusta"), las más recientes primero.
pub async fn saved_tracks(sp: &Client) -> Result<Vec<TrackEntry>> {
    let items = fetch_paged(MAX_SAVED_TRACKS, |offset| async move {
        sp.current_user_saved_tracks_manual(Some(Market::FromToken), Some(PAGE_SIZE), Some(offset))
            .await
            .friendly()
    })
    .await?;
    Ok(items.iter().map(|s| track_entry(&s.track)).collect())
}

pub async fn recently_played(sp: &Client) -> Result<Vec<TrackEntry>> {
    let page = sp
        .current_user_recently_played(Some(50), None)
        .await
        .friendly()?;
    Ok(page.items.iter().map(|h| track_entry(&h.track)).collect())
}

pub async fn queue(sp: &Client) -> Result<Vec<TrackEntry>> {
    let q = sp.current_user_queue().await.friendly()?;
    Ok(q.queue.iter().filter_map(playable_entry).collect())
}

pub async fn search(sp: &Client, query: &str, kind: SearchKind) -> Result<SearchResults> {
    let search_type = match kind {
        SearchKind::Tracks => SearchType::Track,
        SearchKind::Albums => SearchType::Album,
        SearchKind::Artists => SearchType::Artist,
        SearchKind::Playlists => SearchType::Playlist,
    };
    let result = sp
        .search(
            query,
            search_type,
            Some(Market::FromToken),
            None,
            Some(SEARCH_LIMIT),
            None,
        )
        .await
        .friendly()?;

    Ok(match result {
        SearchResult::Tracks(page) => {
            SearchResults::Tracks(page.items.iter().map(track_entry).collect())
        }
        SearchResult::Albums(page) => SearchResults::Contexts(
            page.items
                .into_iter()
                .filter_map(|a| {
                    let year = a
                        .release_date
                        .as_deref()
                        .map(|d| d.chars().take(4).collect::<String>());
                    let artists = artist_names(&a.artists);
                    Some(ContextEntry {
                        detail: match year {
                            Some(y) if !y.is_empty() => format!("{artists} · {y}"),
                            _ => artists,
                        },
                        name: a.name,
                        id: PlayContextId::Album(a.id?),
                    })
                })
                .collect(),
        ),
        SearchResult::Artists(page) => SearchResults::Contexts(
            page.items
                .into_iter()
                .map(|a| ContextEntry {
                    detail: "Artista".into(),
                    name: a.name,
                    id: PlayContextId::Artist(a.id),
                })
                .collect(),
        ),
        SearchResult::Playlists(page) => SearchResults::Contexts(
            page.items
                .into_iter()
                .map(|p| {
                    let p = playlist_entry(p);
                    ContextEntry {
                        detail: format!("de {} · {} canciones", p.owner, p.total),
                        name: p.name,
                        id: PlayContextId::Playlist(p.id),
                    }
                })
                .collect(),
        ),
        _ => SearchResults::Tracks(Vec::new()),
    })
}

pub async fn devices(sp: &Client) -> Result<Vec<DeviceEntry>> {
    let devices = sp.device().await.friendly()?;
    Ok(devices
        .into_iter()
        .map(|d| DeviceEntry {
            id: d.id,
            name: d.name,
            kind: format!("{:?}", d._type),
            active: d.is_active,
        })
        .collect())
}

/// Tamaño máximo aceptado para una carátula (las de Spotify pesan ~50 KB).
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub async fn download_image(http: &reqwest::Client, url: &str) -> Result<image::DynamicImage> {
    let response = http.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|len| len as usize > MAX_IMAGE_BYTES)
    {
        anyhow::bail!("la imagen es demasiado grande");
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("la imagen es demasiado grande");
    }
    let img = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??;
    Ok(img)
}

// ---------------------------------------------------------------------------
// Acciones de reproducción
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Action {
    TogglePlay {
        playing: bool,
    },
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
    Volume(u8),
    Shuffle(bool),
    Repeat(RepeatState),
    /// Reproduce una playlist/álbum/artista, opcionalmente desde una canción.
    PlayContext {
        context: PlayContextId<'static>,
        start: Option<ContextStart>,
    },
    /// Reproduce una lista de canciones sin contexto (me gusta, recientes, búsqueda).
    PlayTracks {
        tracks: Vec<PlayableId<'static>>,
        start: usize,
    },
    Queue(PlayableId<'static>),
    Transfer {
        device_id: String,
    },
}

/// Canción por la que empieza a sonar un contexto (playlist o álbum).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextStart {
    /// La primera aparición de esta URI en el contexto.
    Uri(String),
    /// Una posición exacta (para canciones que aparecen varias veces).
    Position(u32),
}

impl ContextStart {
    fn offset(self) -> Offset {
        match self {
            ContextStart::Uri(uri) => Offset::Uri(uri),
            // rspotify envía los milisegundos de este `Duration` como posición.
            ContextStart::Position(position) => {
                Offset::Position(chrono::Duration::milliseconds(i64::from(position)))
            }
        }
    }
}

impl Action {
    /// Si la acción modifica la cola (para recargarla si se está mostrando).
    pub fn changes_queue(&self) -> bool {
        matches!(
            self,
            Action::Next
                | Action::Previous
                | Action::Queue(_)
                | Action::PlayContext { .. }
                | Action::PlayTracks { .. }
        )
    }

    /// Mensaje que se muestra cuando la acción termina bien.
    pub fn done_message(&self) -> Option<&'static str> {
        match self {
            Action::Queue(_) => Some("Añadida a la cola"),
            Action::Transfer { .. } => Some("Reproducción transferida"),
            _ => None,
        }
    }
}

pub async fn perform(sp: &Client, action: Action) -> Result<()> {
    match action {
        Action::TogglePlay { playing } => {
            if playing {
                sp.pause_playback(None).await
            } else {
                sp.resume_playback(None, None).await
            }
        }
        Action::Next => sp.next_track(None).await,
        Action::Previous => sp.previous_track(None).await,
        Action::Seek { position_ms } => {
            sp.seek_track(chrono::Duration::milliseconds(position_ms as i64), None)
                .await
        }
        Action::Volume(v) => sp.volume(v, None).await,
        Action::Shuffle(state) => sp.shuffle(state, None).await,
        Action::Repeat(state) => sp.repeat(state, None).await,
        Action::PlayContext { context, start } => {
            sp.start_context_playback(context, None, start.map(ContextStart::offset), None)
                .await
        }
        Action::PlayTracks { tracks, start } => {
            // La API limita el tamaño del cuerpo: se envía una ventana alrededor
            // de la canción elegida para que "siguiente/anterior" funcionen.
            let (from, to) = uri_window(tracks.len(), start, MAX_URIS);
            let window = tracks[from..to].to_vec();
            let offset = Offset::Position(chrono::Duration::milliseconds((start - from) as i64));
            sp.start_uris_playback(window, None, Some(offset), None)
                .await
        }
        Action::Queue(id) => sp.add_item_to_queue(id, None).await,
        Action::Transfer { device_id } => sp.transfer_playback(&device_id, Some(true)).await,
    }
    .friendly_player()
}

/// Ventana `[from, to)` de como mucho `max` elementos que contiene `start`,
/// dejando algunas canciones anteriores para que "anterior" también funcione.
fn uri_window(len: usize, start: usize, max: usize) -> (usize, usize) {
    let from = start.saturating_sub(max / 4).min(len.saturating_sub(max));
    (from, (from + max).min(len))
}

/// Guarda en disco el token que hay en memoria (rspotify no lo hace: la caché
/// de `token.json` la gestiona [`crate::config::save_token`]).
pub async fn save_session(sp: &Client) -> Result<()> {
    let token = match sp.token.lock().await {
        Ok(token) => token.clone(),
        Err(_) => None,
    };
    match token {
        Some(token) => crate::config::save_token(&sp.config.cache_path, &token),
        None => Ok(()),
    }
}

/// Espera tras un rechazo de Spotify al renovar la sesión.
const REFRESH_RETRY_REJECTED: Duration = Duration::from_secs(30);
/// Espera tras un fallo de red al renovar la sesión.
const REFRESH_RETRY_OFFLINE: Duration = Duration::from_secs(10);
/// Espera tras un fallo pasajero de Spotify al renovar (5xx, respuesta rara).
const REFRESH_RETRY_TRANSIENT: Duration = Duration::from_secs(15);
/// Espera máxima que se acepta del `Retry-After` de un 429.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Mensaje cuando Spotify no acepta renovar la sesión (token revocado, etc.).
const SESSION_EXPIRED: &str = "La sesión de Spotify caducó y no se pudo renovar: \
     sal con q y ejecuta `spotify_cmd --logout` para volver a entrar";
/// Mensaje cuando Spotify falla al renovar pero la sesión sigue siendo válida.
const REFRESH_TROUBLE: &str = "Spotify tiene problemas para renovar la sesión; se reintentará solo";

/// Por qué falló la renovación del token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailure {
    /// No se pudo hablar con Spotify (sin red, DNS, tiempo agotado).
    Offline,
    /// Spotify rechazó el refresh token (revocado o ya usado): hay que volver
    /// a iniciar sesión.
    Rejected,
    /// Fallo pasajero de Spotify (5xx, 429...): el refresh token sigue
    /// valiendo y se reintenta tras la espera indicada.
    Transient(Duration),
}

impl RefreshFailure {
    /// Clasifica el error de una renovación.
    pub fn of(err: &ClientError) -> Self {
        if is_network_error(err) {
            return RefreshFailure::Offline;
        }
        match error_response(err) {
            Some(resp) => Self::from_status(resp.status().as_u16(), retry_after(resp)),
            // Una respuesta ilegible no demuestra que la sesión no valga.
            None => RefreshFailure::Transient(REFRESH_RETRY_TRANSIENT),
        }
    }

    /// Solo un 400 o un 401 (`invalid_grant`, `invalid_client`) rechazan el
    /// refresh token. Un 429 espera lo que pida Spotify (con tope) y el resto
    /// (5xx...) se trata como un fallo pasajero.
    fn from_status(status: u16, retry_after: Option<Duration>) -> Self {
        match status {
            400 | 401 => RefreshFailure::Rejected,
            429 => RefreshFailure::Transient(
                retry_after
                    .unwrap_or(REFRESH_RETRY_TRANSIENT)
                    .min(MAX_RETRY_AFTER),
            ),
            _ => RefreshFailure::Transient(REFRESH_RETRY_TRANSIENT),
        }
    }

    /// Cuánto esperar antes de reintentar y qué avisar mientras tanto.
    fn retry(self) -> (Duration, &'static str) {
        match self {
            RefreshFailure::Offline => (REFRESH_RETRY_OFFLINE, OFFLINE),
            RefreshFailure::Rejected => (REFRESH_RETRY_REJECTED, SESSION_EXPIRED),
            RefreshFailure::Transient(wait) => (wait, REFRESH_TROUBLE),
        }
    }
}

/// Respuesta de Spotify con código de error, si la hubo.
fn error_response(err: &ClientError) -> Option<&reqwest::Response> {
    match err {
        ClientError::Http(http) => match http.as_ref() {
            HttpError::StatusCode(resp) => Some(resp),
            _ => None,
        },
        _ => None,
    }
}

async fn token_is_valid(sp: &Client) -> bool {
    match sp.token.lock().await {
        Ok(token) => token.as_ref().is_some_and(|t| !t.is_expired()),
        Err(_) => false,
    }
}

/// Refresh token que hay en memoria.
async fn refresh_token_in_memory(sp: &Client) -> Option<String> {
    match sp.token.lock().await {
        Ok(token) => token.as_ref().and_then(|t| t.refresh_token.clone()),
        Err(_) => None,
    }
}

async fn set_token(sp: &Client, token: rspotify::Token) {
    if let Ok(mut slot) = sp.token.lock().await {
        *slot = Some(token);
    }
}

/// Próximo reintento permitido tras un fallo al renovar y el aviso que se da
/// hasta entonces.
type RetryGate = tokio::sync::Mutex<Option<(Instant, &'static str)>>;

/// Renueva el token si ha caducado, de uno en uno. Con PKCE cada refresh token
/// solo sirve una vez, así que varias tareas renovándolo a la vez fallarían.
///
/// Si la renovación falla, no se reintenta en cada petición: se espera un
/// rato (según [`RefreshFailure`]).
pub async fn ensure_token(sp: &Client) -> Result<()> {
    static RETRY: RetryGate = tokio::sync::Mutex::const_new(None);
    ensure_token_with(sp, &RETRY).await
}

async fn ensure_token_with(sp: &Client, gate: &RetryGate) -> Result<()> {
    let mut retry = gate.lock().await;
    if token_is_valid(sp).await {
        *retry = None;
        return Ok(());
    }
    if let Some((at, reason)) = *retry
        && Instant::now() < at
    {
        anyhow::bail!(reason);
    }
    match refresh_session(sp).await {
        Ok(()) => {
            *retry = None;
            // Si no se puede guardar en disco (disco lleno, permisos), la app
            // sigue con el token en memoria; solo habrá que volver a entrar la
            // próxima vez que se abra.
            let _ = save_session(sp).await;
            Ok(())
        }
        Err(failure) => {
            let (wait, reason) = failure.retry();
            *retry = Some((Instant::now() + wait, reason));
            anyhow::bail!(reason)
        }
    }
}

/// Renueva el token. Si Spotify rechaza el refresh token, puede que otra
/// instancia de la app ya lo usara y guardara uno nuevo en `token.json`: en
/// ese caso se adopta el del disco antes de dar la sesión por perdida.
async fn refresh_session(sp: &Client) -> Result<(), RefreshFailure> {
    let failure = match sp.refresh_token().await {
        Ok(()) => return Ok(()),
        Err(e) => RefreshFailure::of(&e),
    };
    if failure != RefreshFailure::Rejected {
        return Err(failure);
    }
    adopt_saved_token(sp).await
}

/// Usa el token de `token.json` si su refresh token es distinto del que hay
/// en memoria: tal cual si sigue vigente o, si ha caducado, renovándolo una
/// sola vez más.
async fn adopt_saved_token(sp: &Client) -> Result<(), RefreshFailure> {
    let saved = crate::config::load_token(&sp.config.cache_path, &sp.oauth.scopes)
        .filter(|t| t.refresh_token.is_some())
        .ok_or(RefreshFailure::Rejected)?;
    if saved.refresh_token == refresh_token_in_memory(sp).await {
        return Err(RefreshFailure::Rejected);
    }
    let expired = saved.is_expired();
    set_token(sp, saved).await;
    if !expired {
        return Ok(());
    }
    sp.refresh_token().await.map_err(|e| RefreshFailure::of(&e))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn page(items: Vec<u32>, total: u32) -> Page<u32> {
        Page {
            href: String::new(),
            items,
            limit: PAGE_SIZE,
            next: None,
            offset: 0,
            previous: None,
            total,
        }
    }

    /// Simula una lista de `total` elementos numerados desde 0.
    async fn fake_page(offset: u32, total: u32) -> Result<Page<u32>> {
        let end = (offset + PAGE_SIZE).min(total);
        Ok(page((offset..end).collect(), total))
    }

    #[tokio::test]
    async fn fetch_paged_keeps_order_and_limits() {
        let all = fetch_paged(1000, |o| fake_page(o, 237)).await.unwrap();
        assert_eq!(all, (0..237).collect::<Vec<_>>());

        let capped = fetch_paged(120, |o| fake_page(o, 5000)).await.unwrap();
        assert_eq!(capped, (0..120).collect::<Vec<_>>());

        let empty = fetch_paged(1000, |o| fake_page(o, 0)).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn fetch_paged_propagates_errors() {
        let result = fetch_paged(1000, |o| async move {
            if o == 100 {
                anyhow::bail!("fallo en la página 3")
            } else {
                fake_page(o, 300).await
            }
        })
        .await;
        assert!(result.is_err());
    }

    /// Página de `items` pedida en `offset`, de una lista de `total` elementos.
    fn page_at(offset: u32, items: Vec<u32>, total: u32) -> (u32, Page<u32>) {
        (
            offset,
            Page {
                offset,
                ..page(items, total)
            },
        )
    }

    #[tokio::test]
    async fn fetch_pages_keep_the_requested_offsets() {
        let pages = fetch_pages(1000, |o| fake_page(o, 120)).await.unwrap();
        let offsets: Vec<u32> = pages.iter().map(|(o, _)| *o).collect();
        assert_eq!(offsets, vec![0, 50, 100]);
        let positions = with_positions(pages, 1000);
        assert_eq!(positions.len(), 120);
        assert!(positions.iter().all(|(p, item)| *p == Some(*item)));
    }

    /// Si Spotify omitió un elemento nulo de una página, sus posiciones dejan
    /// de ser fiables; las de las demás páginas se conservan.
    #[test]
    fn positions_of_a_page_with_missing_items_are_unknown() {
        let total = 60;
        let first: Vec<u32> = (0..50).filter(|n| *n != 7).collect();
        let pages = vec![
            page_at(0, first, total),
            page_at(50, (50..60).collect(), total),
        ];
        let positions = with_positions(pages, 1000);
        assert_eq!(positions.len(), 59);
        assert!(positions[..49].iter().all(|(p, _)| p.is_none()));
        assert_eq!(positions[49], (Some(50), 50));
        assert_eq!(positions[58], (Some(59), 59));
        // El límite se respeta.
        let capped = with_positions(vec![page_at(0, (0..50).collect(), 50)], 10);
        assert_eq!(capped.len(), 10);
    }

    #[test]
    fn context_start_becomes_the_right_offset() {
        let uri = "spotify:track:4u7EnebtmKWzUH433cf5Qv".to_string();
        assert_eq!(ContextStart::Uri(uri.clone()).offset(), Offset::Uri(uri));
        assert_eq!(
            ContextStart::Position(39).offset(),
            Offset::Position(chrono::Duration::milliseconds(39))
        );
    }

    #[test]
    fn uri_window_contains_start_and_fills_up() {
        assert_eq!(uri_window(10, 3, 100), (0, 10));
        assert_eq!(uri_window(500, 0, 100), (0, 100));
        assert_eq!(uri_window(500, 250, 100), (225, 325));
        // Cerca del final la ventana se desplaza para seguir llena.
        assert_eq!(uri_window(200, 190, 100), (100, 200));
        for (len, start) in [(1, 0), (100, 99), (101, 100), (1000, 999)] {
            let (from, to) = uri_window(len, start, 100);
            assert!(from <= start && start < to && to - from <= 100);
        }
    }

    #[test]
    fn pick_image_prefers_medium_size() {
        let img = |w: u32| Image {
            height: Some(w),
            width: Some(w),
            url: format!("u{w}"),
        };
        assert_eq!(
            pick_image(&[img(640), img(300), img(64)]).as_deref(),
            Some("u300")
        );
        assert_eq!(pick_image(&[img(64)]).as_deref(), Some("u64"));
        assert_eq!(pick_image(&[]), None);
    }

    #[test]
    fn search_kind_cycles() {
        let mut k = SearchKind::Tracks;
        for _ in 0..SearchKind::ALL.len() {
            k = k.next();
        }
        assert_eq!(k, SearchKind::Tracks);
        assert_eq!(SearchKind::Tracks.prev(), SearchKind::Playlists);
    }

    #[test]
    fn status_messages_depend_on_request_kind() {
        assert!(status_message(404, Kind::Player, None).contains("dispositivo"));
        assert!(!status_message(404, Kind::Query, None).contains("dispositivo"));
        assert!(status_message(429, Kind::Query, Some(Duration::from_secs(7))).contains("7 s"));
    }

    /// Un 403 al pedir las canciones de una playlist ajena no culpa a la
    /// configuración de la app; el resto de códigos se explican igual.
    #[test]
    fn playlist_items_explain_foreign_playlists() {
        let foreign = status_message(403, Kind::PlaylistItems, None);
        assert!(foreign.contains("tus playlists") && !foreign.contains("User Management"));
        assert_ne!(status_message(403, Kind::Query, None), foreign);
        assert_eq!(
            status_message(404, Kind::PlaylistItems, None),
            status_message(404, Kind::Query, None)
        );
    }

    #[test]
    fn durations_never_go_negative() {
        assert_eq!(millis(chrono::Duration::milliseconds(-5)), 0);
        assert_eq!(millis(chrono::Duration::milliseconds(1500)), 1500);
    }

    /// Solo un 400/401 de accounts.spotify.com significa que hay que volver a
    /// entrar; una caída (5xx) o un 429 se reintentan solos.
    #[test]
    fn refresh_failures_are_classified_by_status() {
        let transient = RefreshFailure::Transient(REFRESH_RETRY_TRANSIENT);
        assert_eq!(
            RefreshFailure::from_status(400, None),
            RefreshFailure::Rejected
        );
        assert_eq!(
            RefreshFailure::from_status(401, None),
            RefreshFailure::Rejected
        );
        assert_eq!(RefreshFailure::from_status(500, None), transient);
        assert_eq!(RefreshFailure::from_status(503, None), transient);
        assert_eq!(RefreshFailure::from_status(429, None), transient);
        assert_eq!(
            RefreshFailure::from_status(429, Some(Duration::from_secs(7))),
            RefreshFailure::Transient(Duration::from_secs(7))
        );
        assert_eq!(
            RefreshFailure::from_status(429, Some(Duration::from_secs(3600))),
            RefreshFailure::Transient(MAX_RETRY_AFTER)
        );
        // Un error que no es una respuesta de Spotify no demuestra nada.
        assert_eq!(RefreshFailure::of(&ClientError::InvalidToken), transient);

        assert!(RefreshFailure::Rejected.retry().1.contains("--logout"));
        assert!(!transient.retry().1.contains("--logout"));
        assert!(!RefreshFailure::Offline.retry().1.contains("--logout"));
    }

    // -- Renovación del token contra un servidor de tokens de prueba --------

    /// Peticiones recibidas por el servidor de tokens de prueba (sus cuerpos).
    type Requests = Arc<std::sync::Mutex<Vec<String>>>;

    fn http_response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    fn rejected() -> String {
        http_response("400 Bad Request", "", r#"{"error":"invalid_grant"}"#)
    }

    fn renewed() -> String {
        let body = r#"{"access_token":"renovado","token_type":"Bearer","expires_in":3600,"refresh_token":"r2","scope":""}"#;
        http_response("200 OK", "", body)
    }

    /// Lee una petición HTTP completa y devuelve su cuerpo.
    async fn read_body(stream: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return String::new();
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).into_owned();
            let Some(end) = text.find("\r\n\r\n") else {
                continue;
            };
            let len = text[..end]
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if data.len() >= end + 4 + len {
                return String::from_utf8_lossy(&data[end + 4..]).into_owned();
            }
        }
    }

    /// Imita accounts.spotify.com: responde a cada petición con la siguiente
    /// respuesta de la lista (la última se repite). Devuelve su URL.
    async fn token_server(responses: Vec<String>) -> (String, Requests) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Requests::default();
        let seen = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = read_body(&mut stream).await;
                let n = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(body);
                    seen.len()
                };
                let response = &responses[(n - 1).min(responses.len() - 1)];
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (url, requests)
    }

    fn session_token(access: &str, refresh: &str, valid: bool) -> rspotify::Token {
        rspotify::Token {
            access_token: access.into(),
            refresh_token: Some(refresh.into()),
            expires_at: valid.then(|| chrono::Utc::now() + chrono::Duration::hours(1)),
            ..Default::default()
        }
    }

    /// Cliente con un token caducado (refresh token `r0`) en memoria, que
    /// renueva contra `auth_url` y guarda la sesión en un directorio propio.
    async fn expired_session(name: &str, auth_url: &str) -> (Client, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("spotify_cmd_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = rspotify::Config {
            token_cached: false,
            token_refreshing: false,
            cache_path: dir.join("token.json"),
            auth_base_url: auth_url.into(),
            ..Default::default()
        };
        let sp = Client::with_config(Default::default(), Default::default(), config);
        set_token(&sp, session_token("viejo", "r0", false)).await;
        (sp, dir)
    }

    async fn access_token(sp: &Client) -> Option<String> {
        sp.token
            .lock()
            .await
            .unwrap()
            .as_ref()
            .map(|t| t.access_token.clone())
    }

    /// Una caída de accounts.spotify.com no da la sesión por perdida: avisa
    /// de que se reintentará y no insiste en cada petición.
    #[tokio::test]
    async fn server_errors_while_refreshing_are_transient() {
        let down = http_response("503 Service Unavailable", "", "");
        let (url, requests) = token_server(vec![down]).await;
        let (sp, dir) = expired_session("refresh_503", &url).await;
        let gate = RetryGate::const_new(None);

        let err = ensure_token_with(&sp, &gate).await.unwrap_err().to_string();
        assert_eq!(err, REFRESH_TROUBLE);
        let err = ensure_token_with(&sp, &gate).await.unwrap_err().to_string();
        assert_eq!(err, REFRESH_TROUBLE);
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "espera antes de reintentar"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ante un 429 se espera lo que indique `Retry-After`.
    #[tokio::test]
    async fn rate_limited_refresh_waits_retry_after() {
        let busy = http_response("429 Too Many Requests", "retry-after: 60\r\n", "");
        let (url, _requests) = token_server(vec![busy]).await;
        let (sp, dir) = expired_session("refresh_429", &url).await;
        let gate = RetryGate::const_new(None);

        let err = ensure_token_with(&sp, &gate).await.unwrap_err().to_string();
        assert!(!err.contains("--logout"));
        let (at, _) = gate.lock().await.unwrap();
        assert!(at > Instant::now() + Duration::from_secs(50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spotify rechaza el refresh token y no hay otro en disco: hay que
    /// volver a entrar.
    #[tokio::test]
    async fn rejected_refresh_expires_the_session() {
        let (url, requests) = token_server(vec![rejected()]).await;
        let (sp, dir) = expired_session("refresh_rejected", &url).await;
        // En disco está el mismo refresh token que en memoria.
        crate::config::save_token(&sp.config.cache_path, &session_token("viejo", "r0", false))
            .unwrap();
        let gate = RetryGate::const_new(None);

        let err = ensure_token_with(&sp, &gate).await.unwrap_err().to_string();
        assert_eq!(err, SESSION_EXPIRED);
        assert_eq!(requests.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Otra instancia ya renovó (el refresh token de memoria ya no vale) y
    /// guardó en disco un token vigente: se adopta sin pedir nada más.
    #[tokio::test]
    async fn adopts_valid_token_saved_by_another_instance() {
        let (url, requests) = token_server(vec![rejected()]).await;
        let (sp, dir) = expired_session("adopt_valid", &url).await;
        crate::config::save_token(
            &sp.config.cache_path,
            &session_token("de-disco", "r1", true),
        )
        .unwrap();
        let gate = RetryGate::const_new(None);

        ensure_token_with(&sp, &gate).await.unwrap();
        assert_eq!(access_token(&sp).await.as_deref(), Some("de-disco"));
        assert_eq!(requests.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El token del disco también caducó: se renueva una vez con su refresh
    /// token y el resultado se guarda.
    #[tokio::test]
    async fn renews_with_refresh_token_saved_by_another_instance() {
        let (url, requests) = token_server(vec![rejected(), renewed()]).await;
        let (sp, dir) = expired_session("adopt_expired", &url).await;
        crate::config::save_token(
            &sp.config.cache_path,
            &session_token("de-disco", "r1", false),
        )
        .unwrap();
        let gate = RetryGate::const_new(None);

        ensure_token_with(&sp, &gate).await.unwrap();
        assert_eq!(access_token(&sp).await.as_deref(), Some("renovado"));
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("refresh_token=r0"));
        assert!(requests[1].contains("refresh_token=r1"));
        let saved = crate::config::load_token(&sp.config.cache_path, &Default::default()).unwrap();
        assert_eq!(saved.access_token, "renovado");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El token renovado en memoria se guarda en la ruta de la configuración.
    #[tokio::test]
    async fn save_session_writes_the_token_in_memory() {
        let dir = std::env::temp_dir().join(format!(
            "spotify_cmd_test_{}_save_session",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        let config = rspotify::Config {
            token_cached: false,
            cache_path: path.clone(),
            ..Default::default()
        };
        let sp = Client::with_config(Default::default(), Default::default(), config);

        // Sin token en memoria no se escribe nada.
        save_session(&sp).await.unwrap();
        assert!(!path.exists());

        *sp.token.lock().await.unwrap() = Some(rspotify::Token {
            access_token: "renovado".into(),
            ..Default::default()
        });
        save_session(&sp).await.unwrap();
        let saved = crate::config::load_token(&path, &Default::default()).unwrap();
        assert_eq!(saved.access_token, "renovado");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod parsing {
    use rspotify::model::{CurrentUserQueue, Page, PlaylistItem};

    use super::playable_entry;

    #[test]
    fn parses_playlist_items() {
        let raw = include_str!("../tests/fixtures/playlist_items.json");
        let page: Page<PlaylistItem> = serde_json::from_str(raw).unwrap();
        let entries: Vec<_> = page
            .items
            .iter()
            .filter_map(|i| i.item.as_ref().and_then(playable_entry))
            .collect();
        println!("{:?}", page.items.first().map(|i| &i.item));
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn parses_queue() {
        let raw = include_str!("../tests/fixtures/queue.json");
        let q: CurrentUserQueue = serde_json::from_str(raw).unwrap();
        assert_eq!(q.queue.iter().filter_map(playable_entry).count(), 4);
    }
}

/// Respuestas REALES de la API de Spotify (tests/fixtures/real): la app debe
/// entenderlas, y no fallar nunca aunque lleguen alteradas.
#[cfg(test)]
mod real_api {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use rspotify::model::{
        CurrentPlaybackContext, CurrentUserQueue, FullEpisode, FullPlaylist, FullTrack, Page,
        PlayableItem, PlaylistItem, SimplifiedPlaylist,
    };
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    use super::*;

    fn load<T: DeserializeOwned>(raw: &str) -> T {
        serde_json::from_str(raw).expect("la respuesta real debe deserializarse")
    }

    const PLAYBACK: &str = include_str!("../tests/fixtures/real/current_playback.json");
    const QUEUE: &str = include_str!("../tests/fixtures/real/queue.json");
    const TRACK: &str = include_str!("../tests/fixtures/real/full_track.json");
    const PLAYLIST: &str = include_str!("../tests/fixtures/real/simplified_playlist.json");
    const EPISODE: &str = include_str!("../tests/fixtures/real/full_episode.json");
    const MALFORMED: &str =
        include_str!("../tests/fixtures/real/playlist_items_malformed_episodes.json");
    const NULL_PAGE: &str = include_str!("../tests/fixtures/real/page_with_null_item.json");
    const FULL_PLAYLIST: &str = include_str!("../tests/fixtures/real/full_playlist.json");

    #[test]
    fn understands_real_playback() {
        let pb = playback_from_context(load::<CurrentPlaybackContext>(PLAYBACK));
        assert!(!pb.title.is_empty() && pb.title != "(sin información)");
        assert!(pb.duration_ms > 0);
        assert!(!pb.device_name.is_empty());
        assert!(pb.current_progress_ms() <= pb.duration_ms);
    }

    #[test]
    fn understands_real_lists() {
        let queue: CurrentUserQueue = load(QUEUE);
        let entries: Vec<_> = queue.queue.iter().filter_map(playable_entry).collect();
        assert_eq!(entries.len(), queue.queue.len());
        assert!(entries.iter().all(|e| !e.name.is_empty() && e.id.is_some()));

        let track = track_entry(&load::<FullTrack>(TRACK));
        assert!(!track.name.is_empty() && !track.artists.is_empty() && track.id.is_some());

        let playlist = playlist_entry(load::<SimplifiedPlaylist>(PLAYLIST));
        assert!(!playlist.name.is_empty() && !playlist.owner.is_empty());

        let episode = playable_entry(&PlayableItem::Episode(load::<FullEpisode>(EPISODE))).unwrap();
        assert!(!episode.name.is_empty() && episode.id.is_some());

        let full: FullPlaylist = load(FULL_PLAYLIST);
        let songs: Vec<_> = full
            .items
            .items
            .iter()
            .filter_map(|i| i.item.as_ref().and_then(playable_entry))
            .collect();
        assert_eq!(
            songs.len(),
            full.items.items.iter().filter(|i| i.item.is_some()).count()
        );

        let nulls: Page<SimplifiedPlaylist> = load(NULL_PAGE);
        assert!(
            nulls.items.is_empty(),
            "los null de Spotify se ignoran sin fallar"
        );
    }

    /// Una canción se convierte igual venga sola o como elemento reproducible.
    #[test]
    fn track_conversions_agree() {
        let track: FullTrack = load(TRACK);
        let single = track_entry(&track);
        let item = playable_entry(&PlayableItem::Track(track)).unwrap();
        assert_eq!(single.uri(), item.uri());
        assert_eq!(
            (
                single.name,
                single.artists,
                single.album,
                single.duration_ms
            ),
            (item.name, item.artists, item.album, item.duration_ms)
        );
    }

    #[test]
    fn keeps_malformed_episodes_instead_of_dropping_them() {
        // Caso real (rspotify #525): episodios que no encajan en el modelo.
        let page: Page<PlaylistItem> = load(MALFORMED);
        let with_item = page.items.iter().filter(|i| i.item.is_some()).count();
        let shown = page
            .items
            .iter()
            .filter_map(|i| i.item.as_ref().and_then(playable_entry))
            .count();
        assert!(with_item > 0);
        assert_eq!(shown, with_item, "ningún elemento se pierde");
    }

    /// Generador pseudoaleatorio reproducible.
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

    /// Altera un JSON al azar: borra campos, cambia tipos, pone nulos.
    fn mutate(v: &mut Value, rng: &mut Lcg, depth: usize) {
        match v {
            Value::Object(map) if !map.is_empty() => {
                let keys: Vec<String> = map.keys().cloned().collect();
                let key = &keys[rng.next() % keys.len()];
                match rng.next() % 6 {
                    0 => {
                        map.remove(key);
                    }
                    1 => {
                        map.insert(key.clone(), Value::Null);
                    }
                    2 => {
                        map.insert(key.clone(), Value::from(-1));
                    }
                    3 => {
                        map.insert(key.clone(), Value::from("x"));
                    }
                    4 => {
                        map.insert(key.clone(), Value::Array(vec![]));
                    }
                    _ if depth < 6 => {
                        if let Some(child) = map.get_mut(key) {
                            mutate(child, rng, depth + 1);
                        }
                    }
                    _ => {}
                }
            }
            Value::Array(items) if !items.is_empty() => {
                let i = rng.next() % items.len();
                mutate(&mut items[i], rng, depth + 1);
            }
            _ => {}
        }
    }

    /// Deserializa como lo haría la app: si `rspotify` sufre un panic con datos
    /// imposibles, en la app lo captura `spawn_guarded`; aquí se cuenta igual.
    fn try_parse<T: DeserializeOwned>(v: &Value) -> Option<T> {
        catch_unwind(AssertUnwindSafe(|| {
            serde_json::from_value::<T>(v.clone()).ok()
        }))
        .ok()
        .flatten()
    }

    #[test]
    fn altered_real_responses_never_crash_the_app_code() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silencia los panics esperados de rspotify
        let mut rng = Lcg(2026);
        let fixtures = [
            PLAYBACK,
            QUEUE,
            TRACK,
            PLAYLIST,
            EPISODE,
            MALFORMED,
            FULL_PLAYLIST,
        ];
        let mut parsed = 0;
        for raw in fixtures {
            let original: Value = serde_json::from_str(raw).unwrap();
            for _ in 0..400 {
                let mut v = original.clone();
                for _ in 0..(1 + rng.next() % 4) {
                    mutate(&mut v, &mut rng, 0);
                }
                // Nuestro código debe aguantar cualquier JSON, sin protección.
                let _ = lenient_info(&v);
                if let Some(item) = v.get("item") {
                    let _ = lenient_info(item);
                }
                if let Some(ctx) = try_parse::<CurrentPlaybackContext>(&v) {
                    let _ = playback_from_context(ctx);
                    parsed += 1;
                }
                if let Some(q) = try_parse::<CurrentUserQueue>(&v) {
                    let _ = q.queue.iter().filter_map(playable_entry).count();
                    parsed += 1;
                }
                if let Some(page) = try_parse::<Page<PlaylistItem>>(&v) {
                    let _ = page
                        .items
                        .iter()
                        .filter_map(|i| i.item.as_ref().and_then(playable_entry))
                        .count();
                    parsed += 1;
                }
                if let Some(t) = try_parse::<FullTrack>(&v) {
                    let _ = track_entry(&t);
                    parsed += 1;
                }
                if let Some(p) = try_parse::<SimplifiedPlaylist>(&v) {
                    let _ = playlist_entry(p);
                    parsed += 1;
                }
                if let Some(item) = try_parse::<PlayableItem>(&v) {
                    let _ = playable_entry(&item);
                    parsed += 1;
                }
            }
        }
        std::panic::set_hook(hook);
        assert!(
            parsed > 500,
            "deben probarse muchas variantes válidas ({parsed})"
        );
    }
}

#[cfg(test)]
mod tls {
    /// El cliente HTTP confía también en los certificados del sistema, así que
    /// funciona detrás de proxies corporativos que reemplazan el certificado.
    /// Necesita red (se ejecuta a mano): `cargo test -- --ignored tls`.
    #[tokio::test]
    #[ignore]
    async fn https_works_behind_intercepting_proxy() {
        let client = reqwest::Client::new();
        let resp = client
            .get("https://index.crates.io/config.json")
            .send()
            .await
            .expect("la conexión TLS debe funcionar");
        assert!(resp.status().is_success());
    }
}
