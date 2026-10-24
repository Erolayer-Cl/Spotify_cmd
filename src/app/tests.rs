//! Utilidades para las pruebas de la aplicación (también las usan las
//! pruebas de `ui` y de `main`) y una prueba de conjunto: miles de teclas y
//! respuestas aleatorias no deben provocar ningún *panic*.
//!
//! Las pruebas de cada parte están junto a su código, en el módulo `tests`
//! de cada submódulo.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::{errors::Errors, picker::Picker, thread::ResizeRequest};
use rspotify::model::{AlbumId, PlaylistId, RepeatState};
use rspotify::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{App, CrashOrigin, Msg};
use crate::spotify::{
    Client, ContextEntry, DeviceEntry, Playback, PlaylistEntry, SearchKind, SearchResults,
    TrackEntry, TrackList,
};

/// Id de una canción de prueba.
pub(super) const ID_A: &str = "4u7EnebtmKWzUH433cf5Qv";
/// Id de otra canción de prueba.
pub(super) const ID_B: &str = "5ChkMS8OtdzJeqyybCc9R5";

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

/// Error del redimensionado pedido para la carátula `generation`.
pub fn resize_error(generation: u64) -> Msg {
    Msg::Resized {
        generation,
        result: Err(Errors::Sixel("fallo simulado".into())),
    }
}

/// Veces que se ha vuelto a preparar la carátula actual tras un fallo.
pub fn cover_rebuilds(app: &App) -> u8 {
    app.cover_rebuilds
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

/// Pulsación de una tecla sin modificadores.
pub(super) fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Álbum de unos resultados de búsqueda.
pub(super) fn album(name: &str) -> ContextEntry {
    ContextEntry {
        name: name.into(),
        detail: String::new(),
        id: PlayContextId::Album(AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy".to_string()).unwrap()),
    }
}

/// Playlist del usuario.
pub(super) fn playlist(name: &str) -> PlaylistEntry {
    PlaylistEntry {
        id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap(),
        name: name.into(),
        owner: "o".into(),
        total: 1,
    }
}

/// Canción sin id (como un archivo local).
pub(super) fn song(name: &str) -> TrackEntry {
    TrackEntry {
        name: name.into(),
        artists: "Artista".into(),
        album: "Álbum".into(),
        duration_ms: 200_000,
        id: None,
        position: None,
    }
}

/// Canción reproducible con su posición en la lista.
pub(super) fn track(name: &str, id: &str, position: Option<u32>) -> TrackEntry {
    TrackEntry {
        id: rspotify::model::TrackId::from_id(id.to_string())
            .ok()
            .map(rspotify::model::PlayableId::Track),
        position,
        ..song(name)
    }
}

/// Estado de reproducción con la canción `title`.
pub fn now_playing(title: &str, is_playing: bool) -> Playback {
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

/// Muestra `tracks` como la lista cargada de la petición en curso.
pub(super) fn deliver(app: &mut App, tracks: Vec<TrackEntry>) {
    app.on_msg(Msg::Tracks {
        request: app.request_id,
        result: Ok(tracks.into()),
    });
}

/// Texto del aviso visible en la barra de estado.
pub(super) fn status(app: &App) -> Option<&str> {
    app.visible_status().map(|s| s.text.as_str())
}

// ---------------------------------------------------------------------------
// Prueba de conjunto
// ---------------------------------------------------------------------------

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
    match rng.next() % 12 {
        0 => Msg::Playback {
            seq,
            result: Ok(None),
        },
        1 => Msg::Playback { seq, result: err() },
        // Con algo sonando, los controles cambian el estado de verdad.
        2 => Msg::Playback {
            seq,
            result: Ok(Some(now_playing("Sonando", rng.next().is_multiple_of(2)))),
        },
        3 => Msg::Tracks {
            request,
            result: Ok(TrackList::default()),
        },
        // Filas reales: moverse, reproducir, encolar y seguir la selección.
        4 => Msg::Tracks {
            request,
            result: Ok(TrackList {
                tracks: (0..rng.next() % 40)
                    .map(|i| match i % 3 {
                        0 => song("Local"),
                        1 => track("A", ID_A, Some(i as u32)),
                        _ => track("B", ID_B, None),
                    })
                    .collect(),
                // A veces es solo una parte de una lista más larga.
                total: rng.next() % 80,
            }),
        },
        5 => Msg::Tracks {
            request,
            result: err(),
        },
        6 => Msg::Devices {
            request: app.devices_request.wrapping_sub((rng.next() % 2) as u64),
            result: Ok(Vec::new()),
        },
        7 => Msg::Devices {
            request: app.devices_request,
            result: Ok(vec![DeviceEntry {
                id: Some("altavoz".into()),
                name: "Altavoz".into(),
                kind: "Speaker".into(),
                active: rng.next().is_multiple_of(2),
            }]),
        },
        8 => Msg::ActionDone {
            done: Some("ok"),
            refresh_queue: true,
            replay: None,
            result: err(),
        },
        // Una reproducción sin dispositivo activo.
        9 => Msg::ActionDone {
            done: None,
            refresh_queue: true,
            replay: Some(Box::new(crate::spotify::Action::PlayTracks {
                tracks: Vec::new(),
                start: 0,
                device_id: None,
            })),
            result: Err(anyhow::Error::new(crate::spotify::ApiError::player(404))),
        },
        10 => Msg::Search {
            request,
            title: "t".into(),
            kind: SearchKind::Albums,
            result: Ok(SearchResults::Contexts(vec![album("Disco")])),
        },
        _ if rng.next().is_multiple_of(2) => crash(CrashOrigin::List(request), "panic simulado"),
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

/// Lo mismo, dibujando cada estado en ventanas de tamaños variados (de
/// diminutas a enormes): el dibujado tampoco debe fallar nunca.
#[tokio::test]
async fn survives_random_keys_and_messages_while_drawing() {
    let (mut app, _rx) = test_app();
    let mut rng = Lcg(7);
    let sizes = [(1, 1), (20, 6), (45, 14), (80, 24), (200, 60)];
    let mut terminals: Vec<_> = sizes
        .iter()
        .map(|&(w, h)| ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap())
        .collect();
    for i in 0..2_000 {
        if rng.next().is_multiple_of(3) {
            let msg = random_msg(&mut rng, &app);
            app.on_msg(msg);
        } else {
            let code = KEYS[rng.next() % KEYS.len()];
            app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        }
        app.on_tick();
        app.should_quit = false;
        let tui = &mut terminals[i % sizes.len()];
        tui.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
    }
}
