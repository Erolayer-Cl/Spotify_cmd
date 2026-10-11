//! Capa fina sobre `rspotify`: convierte los modelos de la API en estructuras
//! sencillas para la interfaz y traduce los errores a mensajes legibles.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::{StreamExt, TryStreamExt};
use rspotify::{
    AuthCodePkceSpotify, ClientError,
    http::HttpError,
    model::{
        AdditionalType, AlbumId, FullTrack, Image, Market, Offset, Page, PlayableItem, PlaylistId,
        RepeatState, SearchResult, SearchType, SimplifiedPlaylist,
    },
    prelude::*,
};
use std::future::Future;

pub type Client = AuthCodePkceSpotify;

/// Límite de resultados por búsqueda. Las apps en "Development mode" de
/// Spotify aceptan como máximo 10 desde febrero de 2026.
pub const SEARCH_LIMIT: u32 = 10;
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

fn track_entry(t: &FullTrack) -> TrackEntry {
    TrackEntry {
        name: t.name.clone(),
        artists: join_artists(t.artists.iter().map(|a| a.name.as_str())),
        album: t.album.name.clone(),
        duration_ms: t.duration.num_milliseconds().max(0) as u64,
        id: t.id.clone().map(PlayableId::Track),
    }
}

fn playable_entry(item: &PlayableItem) -> Option<TrackEntry> {
    match item {
        PlayableItem::Track(t) => Some(track_entry(t)),
        PlayableItem::Episode(e) => Some(TrackEntry {
            name: e.name.clone(),
            artists: e.show.name.clone(),
            album: "Podcast".into(),
            duration_ms: e.duration.num_milliseconds().max(0) as u64,
            id: Some(PlayableId::Episode(e.id.clone())),
        }),
        _ => None,
    }
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

/// Qué tipo de petición falló: cambia el significado de algunos códigos.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Query,
    Player,
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
                "sin conexión con Spotify: revisa tu red (se reintentará solo)"
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
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let message = status_message(status, kind, retry_after);
    anyhow::Error::new(ApiError {
        status: Some(status),
        retry_after,
        message,
    })
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
        (403, Kind::Query) => "Spotify no permite este acceso a la app \
                               (¿tu usuario está en «User Management» de la app?)"
            .into(),
        (404, Kind::Player) => {
            "no hay ningún dispositivo activo: abre Spotify en algún dispositivo \
                                o pulsa 'd' para elegir uno"
                .into()
        }
        (404, Kind::Query) => "Spotify no encontró ese contenido".into(),
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
    let Some(ctx) = sp.current_playback(None, Some(&types)).await.friendly()? else {
        return Ok(None);
    };

    let (title, artists, album, cover_url, duration_ms, item_uri) = match &ctx.item {
        Some(PlayableItem::Track(t)) => (
            t.name.clone(),
            join_artists(t.artists.iter().map(|a| a.name.as_str())),
            t.album.name.clone(),
            pick_image(&t.album.images),
            t.duration.num_milliseconds().max(0) as u64,
            t.id.as_ref().map(|id| id.uri()),
        ),
        Some(PlayableItem::Episode(e)) => (
            e.name.clone(),
            e.show.name.clone(),
            "Podcast".into(),
            pick_image(&e.images).or_else(|| pick_image(&e.show.images)),
            e.duration.num_milliseconds().max(0) as u64,
            Some(e.id.uri()),
        ),
        _ => (
            "(sin información)".into(),
            String::new(),
            String::new(),
            None,
            0,
            None,
        ),
    };

    Ok(Some(Playback {
        is_playing: ctx.is_playing,
        progress_ms: ctx
            .progress
            .map(|p| p.num_milliseconds().max(0) as u64)
            .unwrap_or(0),
        duration_ms,
        title,
        artists,
        album,
        cover_url,
        item_uri,
        shuffle: ctx.shuffle_state,
        repeat: ctx.repeat_state,
        device_name: ctx.device.name.clone(),
        device_type: format!("{:?}", ctx.device._type),
        volume: ctx.device.volume_percent,
        fetched_at: Instant::now(),
    }))
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
/// pedirlas una detrás de otra.
async fn fetch_paged<T, F, Fut>(max_items: u32, fetch_page: F) -> Result<Vec<T>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Page<T>>>,
    T: serde::de::DeserializeOwned,
{
    let first = fetch_page(0).await?;
    let wanted = first.total.min(max_items);
    let mut items = first.items;

    let offsets = (PAGE_SIZE..wanted).step_by(PAGE_SIZE as usize);
    let rest: Vec<Page<T>> = futures::stream::iter(offsets)
        .map(fetch_page)
        .buffered(PARALLEL_PAGES)
        .try_collect()
        .await?;
    items.extend(rest.into_iter().flat_map(|page| page.items));
    items.truncate(max_items as usize);
    Ok(items)
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
    let items = fetch_paged(MAX_PLAYLIST_TRACKS, |offset| async move {
        sp.playlist_items_manual(
            id.as_ref(),
            None,
            Some(Market::FromToken),
            Some(PAGE_SIZE),
            Some(offset),
        )
        .await
        .friendly()
    })
    .await?;
    Ok(items
        .iter()
        .filter_map(|i| i.item.as_ref().and_then(playable_entry))
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
        .map(|t| TrackEntry {
            name: t.name,
            artists: join_artists(t.artists.iter().map(|a| a.name.as_str())),
            album: album_name.to_string(),
            duration_ms: t.duration.num_milliseconds().max(0) as u64,
            id: t.id.map(PlayableId::Track),
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
                    let artists = join_artists(a.artists.iter().map(|x| x.name.as_str()));
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
        start_uri: Option<String>,
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
        Action::PlayContext { context, start_uri } => {
            sp.start_context_playback(context, None, start_uri.map(Offset::Uri), None)
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

/// Renueva el token si ha caducado, de uno en uno. Con PKCE cada refresh token
/// solo sirve una vez, así que varias tareas renovándolo a la vez fallarían.
pub async fn ensure_token(sp: &Client) {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let expired = match sp.token.lock().await {
        Ok(token) => token.as_ref().is_none_or(|t| t.is_expired()),
        Err(_) => false,
    };
    if expired {
        let _ = sp.refresh_token().await;
    }
}

#[cfg(test)]
mod tests {
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
}
