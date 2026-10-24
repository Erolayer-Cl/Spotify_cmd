//! Vista principal: qué muestra ([`MainContent`]) y cómo se cargan en
//! segundo plano sus listas de canciones, álbumes, playlists y búsquedas.
//!
//! Cada carga lleva un identificador de petición: si se pide otra antes de
//! que termine, la anterior se cancela y su respuesta se descarta.

use std::future::Future;

use anyhow::Result;
use ratatui::widgets::TableState;
use rspotify::model::PlaylistId;
use rspotify::prelude::*;

use super::{App, Focus, Msg};
use crate::spotify::{self, ApiError, Client, ContextEntry, SearchKind, SearchResults, TrackEntry};

/// Cómo reproducir las canciones de la lista mostrada.
#[derive(Debug, Clone)]
pub enum TrackSource {
    /// Playlist o álbum: se reproduce el contexto (siguiente/anterior respetan la lista).
    Context(PlayContextId<'static>),
    /// Lista "suelta" (me gusta, recientes, búsqueda...): se envían las URIs.
    Uris,
}

/// Contenido de la vista principal.
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
pub(super) enum LoadMode {
    /// Lista nueva: «Cargando…» y selección en la primera fila.
    Open,
    /// Recarga en segundo plano de la lista visible: no la borra mientras
    /// carga y conserva la fila seleccionada.
    Reload,
}

/// Lista de canciones que se está cargando.
pub(super) struct PendingList {
    title: String,
    source: TrackSource,
    mode: LoadMode,
}

impl App {
    /// Muestra la vista principal con el foco: lo que se abra a continuación
    /// tiene que verse (la carátula grande la taparía).
    pub(super) fn show_list(&mut self) {
        self.focus = Focus::Main;
        self.big_cover = false;
    }

    /// Número de filas de la vista principal.
    pub(super) fn main_len(&self) -> usize {
        match &self.main {
            MainContent::Tracks { tracks, .. } => tracks.len(),
            MainContent::Contexts { items, .. } => items.len(),
            _ => 0,
        }
    }

    // -----------------------------------------------------------------------
    // Peticiones de lista
    // -----------------------------------------------------------------------

    /// Cancela la carga de lista en curso e invalida su identificador: si su
    /// respuesta llega igualmente, se descarta.
    pub(super) fn abandon_request(&mut self) {
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
    pub(super) fn list_loading(&self) -> bool {
        self.list_task.as_ref().is_some_and(|t| !t.is_finished())
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

    // -----------------------------------------------------------------------
    // Cargas
    // -----------------------------------------------------------------------

    /// Pide en segundo plano las canciones de la lista (con «Cargando…» si
    /// es una lista nueva).
    pub(super) fn load_tracks<F, Fut>(
        &mut self,
        title: String,
        source: TrackSource,
        mode: LoadMode,
        fetch: F,
    ) where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Vec<TrackEntry>>> + Send + 'static,
    {
        let request = self.next_request(title, source, mode);
        self.spawn_list(fetch, move |result| Msg::Tracks { request, result });
    }

    /// Carga una playlist: al reproducirla se usa su contexto.
    pub(super) fn load_playlist(&mut self, title: String, id: PlaylistId<'static>, mode: LoadMode) {
        let source = TrackSource::Context(PlayContextId::Playlist(id.clone()));
        self.load_tracks(title, source, mode, |sp| async move {
            spotify::playlist_tracks(&sp, id).await
        });
    }

    /// Abre un álbum o una playlist de los resultados de una búsqueda.
    pub(super) fn open_context(&mut self, entry: ContextEntry) {
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

    /// Lanza la búsqueda escrita en el buscador.
    pub(super) fn run_search(&mut self) {
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
        self.spawn_list(
            move |sp| async move { spotify::search(&sp, &query, kind).await },
            move |result| Msg::Search {
                request,
                title,
                kind,
                result,
            },
        );
    }

    // -----------------------------------------------------------------------
    // Respuestas
    // -----------------------------------------------------------------------

    /// Respuesta de la carga de una lista de canciones.
    pub(super) fn on_tracks(&mut self, request: u64, result: Result<Vec<TrackEntry>>) {
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

    /// Respuesta de una búsqueda.
    pub(super) fn on_search(
        &mut self,
        request: u64,
        title: String,
        kind: SearchKind,
        result: Result<SearchResults>,
    ) {
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

    /// Muestra las canciones recibidas para la lista `pending`.
    /// La canción seleccionada en la lista que se ve, con su fila.
    fn selected_track(&self) -> Option<(usize, &TrackEntry)> {
        let row = self.main_state.selected()?;
        match &self.main {
            MainContent::Tracks { tracks, .. } => tracks.get(row).map(|t| (row, t)),
            _ => None,
        }
    }

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
                self.show_request_error(&format!("No se pudo actualizar «{title}»"), &e);
                return;
            }
            Err(e) => load_error(title, source, &e),
            Ok(tracks) if tracks.is_empty() => {
                MainContent::Message(format!("«{title}» está vacía."))
            }
            Ok(tracks) => {
                // Al recargar (la cola tras un salto, las recientes...) la
                // selección sigue a la canción, no al número de fila.
                let row = if reload {
                    followed_row(self.selected_track(), &tracks)
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

/// Fila que queda seleccionada al recargar una lista que ahora tiene `len`
/// filas (`len` > 0): la misma de antes, o la última si ya no existe.
fn kept_row(selected: Option<usize>, len: usize) -> usize {
    selected.unwrap_or(0).min(len.saturating_sub(1))
}

/// Fila de la lista nueva donde quedó la canción que estaba seleccionada
/// (`previous`: su fila y la canción). Se compara por id, o por nombre y
/// artista en los archivos locales sin id; si aparece varias veces gana la
/// más cercana a la fila anterior. Si ya no está, se mantiene la fila.
fn followed_row(previous: Option<(usize, &TrackEntry)>, tracks: &[TrackEntry]) -> usize {
    let Some((row, old)) = previous else {
        return 0;
    };
    let same_song = |t: &TrackEntry| match (&old.id, &t.id) {
        (Some(a), Some(b)) => a == b,
        (None, None) => t.name == old.name && t.artists == old.artists,
        _ => false,
    };
    tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| same_song(t))
        .min_by_key(|(j, _)| j.abs_diff(row))
        .map_or_else(|| kept_row(Some(row), tracks.len()), |(j, _)| j)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::KeyCode;

    use super::*;
    use crate::app::tests::{
        album, deliver, key, now_playing, playlist, responses, song, status, test_app,
    };
    use crate::app::{CrashOrigin, FIXED_LIBRARY, LibraryItem};
    use crate::spotify::{Playback, PlaylistEntry};

    /// Error de la API al pedir las canciones de una playlist.
    fn api_error(status: Option<u16>) -> anyhow::Error {
        anyhow::Error::new(ApiError::playlist_items(status))
    }

    fn named(names: &[&str]) -> Vec<TrackEntry> {
        names.iter().map(|n| song(n)).collect()
    }

    /// La selección sigue a la canción cuando la lista cambia al recargarse.
    #[test]
    fn reload_selection_follows_the_song() {
        let old = named(&["A", "B", "C", "D"]);
        let c = Some((2, &old[2]));
        // Las recientes anteponen la canción anterior: C baja una fila.
        assert_eq!(followed_row(c, &named(&["X", "A", "B", "C", "D"])), 3);
        // La cola tras 'n': todo sube una fila.
        assert_eq!(followed_row(c, &named(&["B", "C", "D"])), 1);
        // Si aparece varias veces, gana la más cercana a la fila anterior.
        assert_eq!(followed_row(c, &named(&["C", "X", "Y", "C"])), 3);
        // Si ya no está, se mantiene la fila (dentro de la lista).
        assert_eq!(followed_row(c, &named(&["X", "Y", "Z", "W"])), 2);
        assert_eq!(followed_row(c, &named(&["X"])), 0);
        assert_eq!(followed_row(None, &named(&["X", "Y"])), 0);
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

    /// El *panic* de la tarea de una lista llega etiquetado con su petición.
    #[tokio::test]
    async fn list_task_panics_are_attributed_to_their_request() {
        let (mut app, _rx) = test_app();
        app.begin_request("Cargando…".into());
        app.spawn_list(
            |_| async { panic!("página imposible") },
            |result| Msg::Tracks { request: 0, result },
        );
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

    #[test]
    fn reload_keeps_the_row_within_the_new_list() {
        assert_eq!(kept_row(Some(2), 3), 2);
        assert_eq!(kept_row(Some(7), 3), 2);
        assert_eq!(kept_row(None, 3), 0);
        assert_eq!(kept_row(Some(4), 0), 0);
    }
}
