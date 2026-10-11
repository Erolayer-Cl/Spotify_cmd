//! Acciones sobre la fila seleccionada de la vista principal: reproducirla
//! (Enter), añadirla a la cola ('a') o abrir el álbum o la playlist ('o').

use super::{App, Focus, MainContent, TrackSource};
use crate::spotify::{Action, ContextStart, TrackEntry};

impl App {
    /// La lista principal tiene el foco pero la tapa la carátula grande.
    pub(super) fn list_hidden(&self) -> bool {
        self.big_cover && self.focus == Focus::Main
    }

    /// Reproduce la fila seleccionada (o, si la lista no se pudo ver, la
    /// playlist o el álbum entero).
    pub(super) fn activate_main(&mut self) {
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

    /// Añade a la cola la canción seleccionada.
    pub(super) fn queue_selected(&mut self) {
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

    /// Abre el álbum o la playlist seleccionada de una búsqueda.
    pub(super) fn open_selected(&mut self) {
        let Some(i) = self.main_state.selected() else {
            return;
        };
        if let MainContent::Contexts { items, .. } = &self.main
            && let Some(entry) = items.get(i).cloned()
        {
            self.open_context(entry);
        }
    }
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

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::app::LibraryItem;
    use crate::app::tests::{ID_A, ID_B, deliver, key, playlist, song, status, test_app, track};

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
}
