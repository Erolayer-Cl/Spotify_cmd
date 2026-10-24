//! Estructuras sencillas que muestra la interfaz (reproducción, canciones,
//! playlists, resultados de búsqueda y dispositivos) y su conversión desde los
//! modelos de `rspotify`.
//!
//! Lo que `rspotify` no entiende (por ejemplo, porque Spotify cambió el
//! formato) se lee del JSON crudo de forma tolerante en vez de descartarlo.

use std::time::Instant;

use rspotify::{
    model::{
        CurrentPlaybackContext, EpisodeId, FullTrack, Image, PlayableItem, PlaylistId, RepeatState,
        SimplifiedArtist, SimplifiedPlaylist, TrackId,
    },
    prelude::*,
};

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

/// Canciones cargadas de una lista y cuántas tiene en Spotify: las listas
/// enormes se cargan solo en parte (para que abran rápido), y así se avisa.
#[derive(Debug, Clone, Default)]
pub struct TrackList {
    pub tracks: Vec<TrackEntry>,
    pub total: usize,
}

/// Una lista cargada entera.
impl From<Vec<TrackEntry>> for TrackList {
    fn from(tracks: Vec<TrackEntry>) -> Self {
        let total = tracks.len();
        TrackList { tracks, total }
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
pub(super) fn artist_names(artists: &[SimplifiedArtist]) -> String {
    join_artists(artists.iter().map(|a| a.name.as_str()))
}

/// Duración en milisegundos; una negativa (no debería llegar) cuenta como 0.
pub(super) fn millis(d: chrono::Duration) -> u64 {
    d.num_milliseconds().max(0) as u64
}

pub(super) fn track_entry(t: &FullTrack) -> TrackEntry {
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

pub(super) fn playable_entry(item: &PlayableItem) -> Option<TrackEntry> {
    item_info(item).map(Into::into)
}

pub(super) fn playlist_entry(p: SimplifiedPlaylist) -> PlaylistEntry {
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

/// Convierte la respuesta de Spotify en el estado que muestra la interfaz.
/// Función pura: se prueba con respuestas reales de la API.
pub(super) fn playback_from_context(ctx: CurrentPlaybackContext) -> Playback {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn durations_never_go_negative() {
        assert_eq!(millis(chrono::Duration::milliseconds(-5)), 0);
        assert_eq!(millis(chrono::Duration::milliseconds(1500)), 1500);
    }
}

#[cfg(test)]
mod parsing {
    use rspotify::model::{CurrentUserQueue, Page, PlaylistItem};

    use super::playable_entry;

    #[test]
    fn parses_playlist_items() {
        let raw = include_str!("../../tests/fixtures/playlist_items.json");
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
        let raw = include_str!("../../tests/fixtures/queue.json");
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

    const PLAYBACK: &str = include_str!("../../tests/fixtures/real/current_playback.json");
    const QUEUE: &str = include_str!("../../tests/fixtures/real/queue.json");
    const TRACK: &str = include_str!("../../tests/fixtures/real/full_track.json");
    const PLAYLIST: &str = include_str!("../../tests/fixtures/real/simplified_playlist.json");
    const EPISODE: &str = include_str!("../../tests/fixtures/real/full_episode.json");
    const MALFORMED: &str =
        include_str!("../../tests/fixtures/real/playlist_items_malformed_episodes.json");
    const NULL_PAGE: &str = include_str!("../../tests/fixtures/real/page_with_null_item.json");
    const FULL_PLAYLIST: &str = include_str!("../../tests/fixtures/real/full_playlist.json");

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
