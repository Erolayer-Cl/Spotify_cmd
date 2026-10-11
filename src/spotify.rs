//! Capa fina sobre `rspotify`: convierte los modelos de la API en estructuras
//! sencillas para la interfaz y traduce los errores a mensajes legibles.

use std::time::Instant;

use anyhow::{Result, anyhow};
use futures::TryStreamExt;
use rspotify::{
    AuthCodePkceSpotify, ClientError,
    http::HttpError,
    model::{
        AdditionalType, AlbumId, FullTrack, Image, Market, Offset, PlayableItem, PlaylistId,
        RepeatState, SearchResult, SearchType, SimplifiedPlaylist,
    },
    prelude::*,
};

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

/// Traduce un error de la API a un mensaje en español con una pista útil.
pub fn friendly(err: ClientError) -> anyhow::Error {
    if let ClientError::Http(http) = &err
        && let HttpError::StatusCode(resp) = http.as_ref()
    {
        let msg = match resp.status().as_u16() {
            401 => "sesión caducada o inválida (ejecuta `spotify_cmd --logout`)".to_string(),
            403 => "Spotify denegó la acción (los controles requieren Spotify Premium, \
                        o la acción no está permitida ahora)"
                .to_string(),
            404 => "no hay ningún dispositivo activo: abre Spotify en algún dispositivo \
                        o pulsa 'd' para elegir uno"
                .to_string(),
            429 => "demasiadas peticiones a Spotify, espera unos segundos".to_string(),
            code => format!("Spotify respondió con el código {code}"),
        };
        return anyhow!(msg);
    }
    anyhow!("{err}")
}

trait Friendly<T> {
    fn friendly(self) -> Result<T>;
}

impl<T> Friendly<T> for std::result::Result<T, ClientError> {
    fn friendly(self) -> Result<T> {
        self.map_err(friendly)
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

pub async fn playlists(sp: &Client) -> Result<Vec<PlaylistEntry>> {
    let items: Vec<SimplifiedPlaylist> =
        sp.current_user_playlists().try_collect().await.friendly()?;
    Ok(items.into_iter().map(playlist_entry).collect())
}

pub async fn playlist_tracks(sp: &Client, id: PlaylistId<'static>) -> Result<Vec<TrackEntry>> {
    let items: Vec<_> = sp
        .playlist_items(id, None, Some(Market::FromToken))
        .try_collect()
        .await
        .friendly()?;
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

/// Canciones guardadas ("Tus me gusta"). Se limitan a las 500 más recientes
/// para no tardar demasiado con bibliotecas enormes.
pub async fn saved_tracks(sp: &Client) -> Result<Vec<TrackEntry>> {
    let mut out = Vec::new();
    let mut offset = 0;
    loop {
        let page = sp
            .current_user_saved_tracks_manual(Some(Market::FromToken), Some(50), Some(offset))
            .await
            .friendly()?;
        out.extend(page.items.iter().map(|s| track_entry(&s.track)));
        offset += page.items.len() as u32;
        if page.next.is_none() || page.items.is_empty() || out.len() >= 500 {
            break;
        }
    }
    Ok(out)
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

pub async fn download_image(http: &reqwest::Client, url: &str) -> Result<image::DynamicImage> {
    let bytes = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
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
            let from = start.saturating_sub(MAX_URIS / 4);
            let to = (from + MAX_URIS).min(tracks.len());
            let window = tracks[from..to].to_vec();
            let offset = Offset::Position(chrono::Duration::milliseconds((start - from) as i64));
            sp.start_uris_playback(window, None, Some(offset), None)
                .await
        }
        Action::Queue(id) => sp.add_item_to_queue(id, None).await,
        Action::Transfer { device_id } => sp.transfer_playback(&device_id, Some(true)).await,
    }
    .friendly()
}

/// Renueva el token si ha caducado, de uno en uno. Con PKCE cada refresh token
/// solo sirve una vez, así que varias tareas renovándolo a la vez fallarían.
pub async fn ensure_token(sp: &Client) {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let expired = sp
        .token
        .lock()
        .await
        .unwrap()
        .as_ref()
        .is_none_or(|t| t.is_expired());
    if expired {
        let _ = sp.refresh_token().await;
    }
}
