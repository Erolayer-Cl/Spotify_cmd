//! Biblioteca (barra lateral): las entradas fijas («Tus me gusta»,
//! recientes y cola) seguidas de las playlists del usuario. Carga de las
//! playlists y apertura o recarga de la entrada que se está viendo.

use std::time::Instant;

use anyhow::Result;
use rspotify::model::PlaylistId;

use super::lists::{LoadMode, TrackSource};
use super::{App, Msg, SETTLE};
use crate::spotify::{self, PlaylistEntry, TrackList};

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

impl App {
    /// Número de entradas de la barra lateral.
    pub fn library_len(&self) -> usize {
        FIXED_LIBRARY.len() + self.playlists.len()
    }

    /// Entrada de la barra lateral en la fila `index`.
    pub fn library_item(&self, index: usize) -> Option<LibraryItem> {
        if index < FIXED_LIBRARY.len() {
            Some(FIXED_LIBRARY[index])
        } else if index - FIXED_LIBRARY.len() < self.playlists.len() {
            Some(LibraryItem::Playlist(index - FIXED_LIBRARY.len()))
        } else {
            None
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

    // -----------------------------------------------------------------------
    // Playlists del usuario
    // -----------------------------------------------------------------------

    pub(super) fn load_playlists(&mut self) {
        // Mantener pulsada R no lanza una carga por cada repetición de la tecla.
        if self.playlists_inflight {
            return;
        }
        self.playlists_inflight = true;
        self.spawn(
            |sp| async move { spotify::playlists(&sp).await },
            Msg::Playlists,
        );
    }

    /// Respuesta de la carga de las playlists.
    pub(super) fn on_playlists(&mut self, result: Result<Vec<PlaylistEntry>>) {
        self.playlists_inflight = false;
        match result {
            Ok(list) => self.replace_playlists(list),
            Err(e) => self.show_request_error("No se pudieron cargar las playlists", &e),
        }
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

    // -----------------------------------------------------------------------
    // Entrada que se está viendo
    // -----------------------------------------------------------------------

    /// Abre una entrada de la biblioteca en la vista principal.
    pub(super) fn open_library(&mut self, item: LibraryItem) {
        self.active_library = Some(item);
        self.show_list();
        self.load_library(item, LoadMode::Open);
    }

    /// Recarga en segundo plano la lista de la biblioteca que se está viendo,
    /// sin mover el foco, la carátula ni la selección.
    pub(super) fn reload_active(&mut self) {
        if let Some(item) = self.active_library {
            self.load_library(item, LoadMode::Reload);
        }
    }

    /// Programa la recarga de la lista visible cuando Spotify ya refleje el
    /// cambio. Una petición posterior la vuelve a retrasar.
    pub(super) fn schedule_list_reload(&mut self) {
        self.list_reload_at = Some(Instant::now() + SETTLE);
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
                spotify::recently_played(&sp).await.map(TrackList::from)
            }),
            LibraryItem::Queue => self.load_tracks(title, loose, mode, |sp| async move {
                spotify::queue(&sp).await.map(TrackList::from)
            }),
            LibraryItem::Playlist(i) => {
                if let Some(id) = self.playlists.get(i).map(|p| p.id.clone()) {
                    self.load_playlist(title, id, mode);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::app::tests::{key, playlist, responses, test_app};

    fn playlist_with_id(name: &str, id: &str) -> PlaylistEntry {
        PlaylistEntry {
            id: PlaylistId::from_id(id.to_string()).unwrap(),
            ..playlist(name)
        }
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
}
