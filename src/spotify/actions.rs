//! Acciones de reproducción ([`Action`]): lo que la interfaz puede pedir
//! (reproducir, pausar, saltar, buscar, cambiar volumen, encolar,
//! transferir...) y cómo se envía cada una a Spotify ([`perform`]).

use anyhow::Result;
use rspotify::{
    model::{Offset, RepeatState},
    prelude::*,
};

use super::Client;
use super::errors::Friendly;

/// Máximo de URIs que se envían al reproducir una lista sin contexto.
const MAX_URIS: usize = 100;

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
                // Spotify reordena lo próximo al cambiar el aleatorio o la repetición.
                | Action::Shuffle(_)
                | Action::Repeat(_)
        )
    }

    /// Si la acción cambia la canción que suena: un salto de posición aún
    /// sin enviar ya no le corresponde.
    pub fn changes_track(&self) -> bool {
        matches!(
            self,
            Action::Next
                | Action::Previous
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_start_becomes_the_right_offset() {
        let uri = "spotify:track:4u7EnebtmKWzUH433cf5Qv".to_string();
        assert_eq!(ContextStart::Uri(uri.clone()).offset(), Offset::Uri(uri));
        assert_eq!(
            ContextStart::Position(39).offset(),
            Offset::Position(chrono::Duration::milliseconds(39))
        );
    }

    /// Solo las acciones que cambian de canción descartan un salto de
    /// posición pendiente.
    #[test]
    fn only_track_changes_drop_a_pending_seek() {
        let album = rspotify::model::AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy").unwrap();
        let play_context = Action::PlayContext {
            context: PlayContextId::Album(album),
            start: None,
        };
        let play_tracks = Action::PlayTracks {
            tracks: Vec::new(),
            start: 0,
        };
        for action in [Action::Next, Action::Previous, play_context, play_tracks] {
            assert!(action.changes_track(), "{action:?}");
        }
        for action in [
            Action::TogglePlay { playing: true },
            Action::Seek { position_ms: 0 },
            Action::Volume(50),
            Action::Shuffle(true),
            Action::Repeat(RepeatState::Track),
            Action::Transfer {
                device_id: "altavoz".into(),
            },
        ] {
            assert!(!action.changes_track(), "{action:?}");
        }
    }

    /// El aleatorio y la repetición reordenan lo próximo (la cola), pero no
    /// cambian la canción que suena.
    #[test]
    fn shuffle_and_repeat_change_the_queue_not_the_track() {
        for action in [Action::Shuffle(true), Action::Repeat(RepeatState::Context)] {
            assert!(action.changes_queue(), "{action:?}");
            assert!(!action.changes_track(), "{action:?}");
        }
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
}
