//! Dibujado de la interfaz con ratatui.
//!
//! Cada zona de la pantalla tiene su propia función `draw_*`, y las piezas que
//! se repiten (paneles, tablas, logo, atajos) son funciones pequeñas
//! reutilizables. Los colores salen de [`crate::theme`].

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    text::{Line, Span},
    widgets::{Cell, Clear, LineGauge, List, ListItem, Paragraph, Row, Table, TableState, Wrap},
};
use ratatui_image::{FilterType, Resize, StatefulImage};
use rspotify::model::RepeatState;
use rspotify::prelude::Id;

use crate::app::{App, FIXED_LIBRARY, Focus, LibraryItem, MainContent, Popup, repeat_label};
use crate::logo::{LOGO_LARGE, LOGO_SMALL};
use crate::spotify::{Playback, SearchKind, TrackEntry};
use crate::theme;

/// Alto del panel "Reproduciendo" (borde incluido).
const PLAYER_HEIGHT: u16 = 11;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [body, player, footer] = Layout::vertical([
        Constraint::Min(8),
        Constraint::Length(PLAYER_HEIGHT),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [library, main] =
        Layout::horizontal([Constraint::Percentage(26), Constraint::Percentage(74)]).areas(body);

    draw_library(f, app, library);
    if app.big_cover {
        draw_big_cover(f, app, main);
    } else {
        draw_main(f, app, main);
    }
    draw_player(f, app, player);
    draw_footer(f, app, footer);

    match &app.popup {
        Popup::Search => draw_search(f, app, main),
        Popup::Devices { .. } => draw_devices(f, app),
        Popup::Help => draw_help(f),
        Popup::None => {}
    }
}

// ---------------------------------------------------------------------------
// Piezas reutilizables
// ---------------------------------------------------------------------------

/// "3:45" o "1:02:03".
fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Un panel tiene el foco si es el activo y no hay ninguna ventana encima.
fn has_focus(app: &App, panel: Focus) -> bool {
    app.focus == panel && matches!(app.popup, Popup::None)
}

/// Rectángulo de `width × height` centrado dentro de `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(v);
    h
}

/// Logo de Spotify en verde, como líneas listas para un `Paragraph`.
fn logo_lines(rows: &[&'static str]) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| Line::styled(*row, theme::text()))
        .collect()
}

/// Logo centrado con un mensaje debajo (pantallas vacías o de carga).
fn draw_logo_message(f: &mut Frame, area: Rect, message: &str) {
    let logo: &[&str] = if area.height >= LOGO_LARGE.len() as u16 + 4 {
        &LOGO_LARGE
    } else {
        &LOGO_SMALL
    };
    let mut lines = logo_lines(logo);
    lines.push(Line::raw(""));
    lines.extend(
        message
            .lines()
            .map(|l| Line::styled(l.to_string(), theme::soft())),
    );
    let height = lines.len() as u16;
    let area = centered(area, area.width, height);
    let p = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Tabla con cabecera, selección y estilo comunes a todas las listas.
fn styled_table<'a>(
    rows: Vec<Row<'a>>,
    widths: &[Constraint],
    header: &[&'a str],
    title: String,
    focused: bool,
) -> Table<'a> {
    let header = Row::new(header.to_vec())
        .style(theme::dim().add_modifier(ratatui::style::Modifier::BOLD))
        .bottom_margin(1);
    Table::new(rows, widths.to_vec())
        .header(header)
        .block(theme::panel(title, focused))
        .style(theme::text())
        .row_highlight_style(theme::selected())
        .highlight_symbol("▌")
}

fn render_table(f: &mut Frame, table: Table, area: Rect, state: &mut TableState) {
    f.render_stateful_widget(table, area, state);
}

// ---------------------------------------------------------------------------
// Biblioteca (lateral)
// ---------------------------------------------------------------------------

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    let fixed = FIXED_LIBRARY.iter().map(|item| {
        let (icon, name) = match item {
            LibraryItem::Liked => ("♥", "Tus me gusta"),
            LibraryItem::Recent => ("◷", "Recientes"),
            LibraryItem::Queue => ("≡", "Cola"),
            LibraryItem::Playlist(_) => ("♫", "Playlist"),
        };
        library_line(
            icon,
            name.to_string(),
            None,
            app.active_library == Some(*item),
        )
    });
    let playlists = app.playlists.iter().enumerate().map(|(i, p)| {
        let active = app.active_library == Some(LibraryItem::Playlist(i));
        library_line("♫", p.name.clone(), Some(p.total), active)
    });
    let items: Vec<ListItem> = fixed.chain(playlists).collect();

    let title = match app.playlists.len() {
        0 => " Biblioteca ".to_string(),
        n => format!(" Biblioteca · {n} playlists "),
    };
    let list = List::new(items)
        .block(theme::panel(title, has_focus(app, Focus::Library)))
        .highlight_style(theme::selected())
        .highlight_symbol("▌");
    f.render_stateful_widget(list, area, &mut app.library_state);
}

fn library_line(
    icon: &'static str,
    name: String,
    total: Option<u32>,
    active: bool,
) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(format!("{icon} "), theme::text()),
        Span::styled(
            name,
            if active {
                theme::strong()
            } else {
                theme::soft()
            },
        ),
    ];
    if let Some(total) = total {
        spans.push(Span::styled(format!(" {total}"), theme::dim()));
    }
    ListItem::new(Line::from(spans))
}

// ---------------------------------------------------------------------------
// Lista principal
// ---------------------------------------------------------------------------

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = has_focus(app, Focus::Main);
    // Id (sin "spotify:track:") de lo que suena, para marcarlo en la lista.
    let playing_id = app
        .playback
        .as_ref()
        .and_then(|p| p.item_uri.as_deref())
        .and_then(|uri| uri.rsplit(':').next());

    match &app.main {
        MainContent::Loading(text) | MainContent::Message(text) => {
            let block = theme::panel(" Lista ", focused);
            let inner = block.inner(area);
            f.render_widget(block, area);
            draw_logo_message(f, inner, text);
        }
        MainContent::Tracks { title, tracks, .. } => {
            let rows = tracks
                .iter()
                .enumerate()
                .map(|(i, t)| track_row(i, t, is_playing(t, playing_id)))
                .collect();
            let widths = [
                Constraint::Length(4),
                Constraint::Percentage(38),
                Constraint::Percentage(27),
                Constraint::Percentage(25),
                Constraint::Length(7),
            ];
            let header = ["#", "Título", "Artista", "Álbum", "Dur."];
            let title = format!(" {title} · {} ", tracks.len());
            let table = styled_table(rows, &widths, &header, title, focused);
            render_table(f, table, area, &mut app.main_state);
        }
        MainContent::Contexts { title, kind, items } => {
            let icon = search_icon(*kind);
            let rows = items
                .iter()
                .map(|c| {
                    Row::new(vec![
                        Cell::from(icon).style(theme::text()),
                        Cell::from(c.name.as_str()),
                        Cell::from(c.detail.as_str()).style(theme::dim()),
                    ])
                })
                .collect();
            let widths = [
                Constraint::Length(2),
                Constraint::Percentage(50),
                Constraint::Percentage(50),
            ];
            let table = styled_table(
                rows,
                &widths,
                &["", "Nombre", "Detalle"],
                format!(" {title} "),
                focused,
            );
            render_table(f, table, area, &mut app.main_state);
        }
    }
}

/// Compara ids sin reservar memoria: se llama para cada fila en cada frame.
fn is_playing(track: &TrackEntry, playing_id: Option<&str>) -> bool {
    match (&track.id, playing_id) {
        (Some(id), Some(playing)) => id.id() == playing,
        _ => false,
    }
}

/// Fila de una canción; la que está sonando se marca con ♪ y en negrita.
/// Toma el texto prestado de `track`, sin copiarlo.
fn track_row(index: usize, track: &TrackEntry, is_playing: bool) -> Row<'_> {
    let number = if is_playing {
        "♪".to_string()
    } else {
        (index + 1).to_string()
    };
    let base = if is_playing {
        theme::strong()
    } else if track.id.is_none() {
        theme::dim()
    } else {
        theme::text()
    };
    let secondary = if is_playing {
        theme::strong()
    } else {
        theme::soft()
    };
    Row::new(vec![
        Cell::from(number).style(if is_playing {
            theme::strong()
        } else {
            theme::dim()
        }),
        Cell::from(track.name.as_str()),
        Cell::from(track.artists.as_str()).style(secondary),
        Cell::from(track.album.as_str()).style(theme::dim()),
        Cell::from(fmt_ms(track.duration_ms)).style(theme::dim()),
    ])
    .style(base)
}

fn search_icon(kind: SearchKind) -> &'static str {
    match kind {
        SearchKind::Albums => "◉",
        SearchKind::Artists => "☺",
        SearchKind::Playlists => "♫",
        SearchKind::Tracks => "♪",
    }
}

// ---------------------------------------------------------------------------
// Reproductor
// ---------------------------------------------------------------------------

fn draw_player(f: &mut Frame, app: &mut App, area: Rect) {
    let block = theme::panel(" Reproduciendo ", false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let info_area = if app.big_cover {
        inner
    } else {
        // Las celdas de la terminal son ~el doble de altas que anchas: un
        // cuadrado de N filas ocupa ~2N columnas.
        let cover_w = (inner.height * 2).min(inner.width / 3);
        let [cover, _gap, info] = Layout::horizontal([
            Constraint::Length(cover_w),
            Constraint::Length(2),
            Constraint::Min(10),
        ])
        .areas(inner);
        draw_cover(f, app, cover);
        info
    };

    match &app.playback {
        Some(pb) => draw_now_playing(f, pb, info_area),
        None => draw_nothing_playing(f, info_area),
    }
}

fn draw_nothing_playing(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::raw(""),
        Line::styled("No hay nada reproduciéndose.", theme::strong()),
        Line::raw(""),
        Line::styled(
            "Abre Spotify en cualquier dispositivo, o elige una canción y pulsa Enter.",
            theme::soft(),
        ),
        Line::styled(
            "Pulsa 'd' para elegir el dispositivo donde reproducir.",
            theme::dim(),
        ),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn draw_now_playing(f: &mut Frame, pb: &Playback, area: Rect) {
    let [text_area, _, gauge_area] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let lines = vec![
        Line::styled(pb.title.clone(), theme::strong()),
        Line::styled(pb.artists.clone(), theme::text()),
        Line::styled(pb.album.clone(), theme::soft().italic()),
        Line::raw(""),
        status_line(pb),
        Line::styled(
            format!("Dispositivo: {} ({})", pb.device_name, pb.device_type),
            theme::dim(),
        ),
    ];
    f.render_widget(Paragraph::new(lines), text_area);
    f.render_widget(progress_bar(pb), gauge_area);
}

/// "▶ Reproduciendo   ⇄ Aleatorio: sí   ↻ Repetir: lista   Vol 65%".
fn status_line(pb: &Playback) -> Line<'static> {
    let state = if pb.is_playing {
        Span::styled("▶ Reproduciendo", theme::strong())
    } else {
        Span::styled("⏸ En pausa", theme::warning())
    };
    let gap = || Span::raw("   ");
    let mut spans = vec![
        state,
        gap(),
        Span::styled(
            format!("⇄ Aleatorio: {}", if pb.shuffle { "sí" } else { "no" }),
            theme::toggle(pb.shuffle),
        ),
        gap(),
        Span::styled(
            format!("↻ Repetir: {}", repeat_label(pb.repeat)),
            theme::toggle(pb.repeat != RepeatState::Off),
        ),
    ];
    if let Some(volume) = pb.volume {
        spans.extend([gap(), Span::styled(format!("Vol {volume}%"), theme::soft())]);
    }
    Line::from(spans)
}

fn progress_bar(pb: &Playback) -> LineGauge<'static> {
    let progress = pb.current_progress_ms();
    let ratio = if pb.duration_ms > 0 {
        (progress as f64 / pb.duration_ms as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    LineGauge::default()
        .ratio(ratio)
        .label(Span::styled(
            format!("{} / {} ", fmt_ms(progress), fmt_ms(pb.duration_ms)),
            theme::text(),
        ))
        .filled_symbol("━")
        .unfilled_symbol("─")
        .filled_style(theme::text())
        .unfilled_style(ratatui::style::Style::new().fg(theme::TRACK_BG))
}

fn draw_cover(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    if app.cover.protocol_type().is_none() {
        // Sin carátula (cargando o nada sonando): el logo de Spotify.
        let logo = Paragraph::new(logo_lines(&LOGO_SMALL)).alignment(Alignment::Center);
        let height = (LOGO_SMALL.len() as u16).min(area.height);
        f.render_widget(logo, centered(area, area.width, height));
        return;
    }
    // `Scale` amplía también las imágenes pequeñas para ocupar todo el hueco.
    let widget = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle)));
    f.render_stateful_widget(widget, area, &mut app.cover);
}

fn draw_big_cover(f: &mut Frame, app: &mut App, area: Rect) {
    let block = theme::panel(" Carátula (c para volver) ", has_focus(app, Focus::Main));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Cuadrado lo más grande posible, centrado.
    let side = inner.height.min(inner.width / 2);
    draw_cover(f, app, centered(inner, side * 2, side));
}

// ---------------------------------------------------------------------------
// Pie, buscador y ventanas emergentes
// ---------------------------------------------------------------------------

const FOOTER_KEYS: [(&str, &str); 11] = [
    ("espacio", "play/pausa"),
    ("n/p", "sig/ant"),
    ("/", "buscar"),
    ("?", "ayuda"),
    ("q", "salir"),
    ("←/→", "±10s"),
    ("s", "aleatorio"),
    ("r", "repetir"),
    ("+/-", "vol"),
    ("c", "carátula"),
    ("d", "dispositivos"),
];

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    // Prioridad: aviso reciente → problema de sesión (persistente) → atajos.
    let line = if let Some(status) = app.visible_status() {
        let style = if status.error {
            theme::error()
        } else {
            theme::strong()
        };
        Line::styled(format!(" {}", status.text), style)
    } else if let Some(problem) = app.session_problem() {
        Line::styled(format!(" {problem}"), theme::error())
    } else {
        Line::from_iter(FOOTER_KEYS.iter().flat_map(|(k, v)| theme::key_hint(k, v)))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_search(f: &mut Frame, app: &App, main: Rect) {
    let area = Rect {
        height: 4.min(main.height),
        ..main
    };
    f.render_widget(Clear, area);

    let tabs = SearchKind::ALL.iter().flat_map(|kind| {
        let style = if *kind == app.search_kind {
            theme::badge()
        } else {
            theme::dim()
        };
        [
            Span::styled(format!(" {} ", kind.label()), style),
            Span::raw(" "),
        ]
    });
    let lines = vec![
        Line::from_iter(std::iter::once(Span::raw(" ")).chain(tabs)),
        Line::from(vec![
            Span::styled(" › ", theme::strong()),
            Span::styled(app.search_input.clone(), theme::text()),
            Span::styled("█", theme::text()),
        ]),
    ];
    let block = theme::popup("Buscar", "Tab: tipo · Enter: buscar · Esc: cerrar");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_devices(f: &mut Frame, app: &mut App) {
    let Popup::Devices { devices, state } = &mut app.popup else {
        return;
    };
    let area = centered(f.area(), 56, 12);
    f.render_widget(Clear, area);
    let block = theme::popup("Dispositivos", "Enter: reproducir aquí · Esc: cerrar");

    match devices {
        None => f.render_widget(
            Paragraph::new(" Buscando dispositivos…")
                .style(theme::soft())
                .block(block),
            area,
        ),
        Some(list) if list.is_empty() => f.render_widget(
            Paragraph::new(
                " No se encontró ningún dispositivo.\n\n Abre la app de Spotify en tu móvil, \
                 ordenador o altavoz y vuelve a pulsar 'd'.",
            )
            .style(theme::soft())
            .wrap(Wrap { trim: false })
            .block(block),
            area,
        ),
        Some(list) => {
            let items: Vec<ListItem> = list
                .iter()
                .map(|d| {
                    let mut spans = vec![
                        Span::styled(d.name.clone(), theme::text()),
                        Span::styled(format!("  ({})", d.kind), theme::dim()),
                    ];
                    if d.active {
                        spans.push(Span::styled("  ● activo", theme::strong()));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items)
                .block(block)
                .highlight_style(theme::selected())
                .highlight_symbol("▌");
            f.render_stateful_widget(list, area, state);
        }
    }
}

/// Filas de la ayuda: un título de sección se marca con descripción vacía.
const HELP_ROWS: [(&str, &str); 19] = [
    ("Reproducción", ""),
    ("espacio", "Reproducir / pausar"),
    ("n · p", "Canción siguiente · anterior"),
    ("← · →", "Retroceder · adelantar 10 segundos"),
    ("+ · -", "Subir · bajar volumen"),
    ("s", "Activar / desactivar aleatorio"),
    ("r", "Repetir: no → lista → canción"),
    ("d", "Elegir dispositivo de reproducción"),
    ("", ""),
    ("Navegación", ""),
    ("Tab", "Cambiar entre biblioteca y lista"),
    ("↑ ↓ · j k", "Moverse · PgUp/PgDn · g/G inicio/fin"),
    ("Enter", "Abrir lista / reproducir selección"),
    ("a", "Añadir canción a la cola"),
    ("o", "Abrir álbum o playlist de la búsqueda"),
    ("/", "Buscar (Tab cambia: canciones, álbumes…)"),
    ("c", "Ver la carátula en grande"),
    ("R · F5", "Actualizar"),
    ("q · Ctrl+C", "Salir"),
];

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 62, HELP_ROWS.len() as u16 + 2);
    f.render_widget(Clear, area);
    let lines: Vec<Line> = HELP_ROWS
        .iter()
        .map(|(key, action)| {
            if action.is_empty() {
                Line::styled(format!(" {key}"), theme::strong())
            } else {
                Line::from(vec![
                    Span::styled(format!("   {key:<12}"), theme::text()),
                    Span::styled(action.to_string(), theme::soft()),
                ])
            }
        })
        .collect();
    let block = theme::popup("Ayuda", "Pulsa cualquier tecla para cerrar");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Pantalla mínima si la interfaz normal falla varias veces seguidas.
pub fn draw_fallback(f: &mut Frame, reason: &str) {
    let area = f.area();
    f.render_widget(Clear, area);
    let message = format!(
        "La interfaz tuvo un problema al dibujarse:\n{reason}\n\n\
         Prueba a cambiar el tamaño de la ventana, o pulsa q para salir."
    );
    draw_logo_message(f, area, &message);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use image::{DynamicImage, Rgb, RgbImage};
    use ratatui::{Terminal, backend::TestBackend};
    use rspotify::model::{PlayableId, PlaylistId, RepeatState, TrackId};

    use super::*;
    use crate::{
        app::TrackSource,
        spotify::{Playback, PlaylistEntry, TrackEntry},
    };

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn demo_app() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<ratatui_image::thread::ResizeRequest>,
    ) {
        let (mut app, rx) = crate::app::tests::test_app();
        let track = |name: &str, artist: &str, album: &str, id: &str| TrackEntry {
            name: name.into(),
            artists: artist.into(),
            album: album.into(),
            duration_ms: 215_000,
            id: Some(PlayableId::Track(TrackId::from_id(id.to_string()).unwrap())),
        };
        app.playlists = vec![PlaylistEntry {
            id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap(),
            name: "Éxitos de hoy".into(),
            owner: "Spotify".into(),
            total: 50,
        }];
        app.main = MainContent::Tracks {
            title: "Tus me gusta".into(),
            source: TrackSource::Uris,
            tracks: vec![
                track(
                    "Bohemian Rhapsody",
                    "Queen",
                    "A Night at the Opera",
                    "4u7EnebtmKWzUH433cf5Qv",
                ),
                track(
                    "Billie Jean",
                    "Michael Jackson",
                    "Thriller",
                    "5ChkMS8OtdzJeqyybCc9R5",
                ),
            ],
        };
        app.main_state.select(Some(1));
        app.playback = Some(Playback {
            is_playing: true,
            progress_ms: 83_000,
            duration_ms: 294_000,
            title: "Bohemian Rhapsody".into(),
            artists: "Queen".into(),
            album: "A Night at the Opera".into(),
            cover_url: None,
            item_uri: Some("spotify:track:4u7EnebtmKWzUH433cf5Qv".into()),
            shuffle: true,
            repeat: RepeatState::Context,
            device_name: "Mi portátil".into(),
            device_type: "Computer".into(),
            volume: Some(65),
            fetched_at: Instant::now(),
        });
        (app, rx)
    }

    #[test]
    fn renders_main_screen_with_cover() {
        let (mut app, mut rx) = demo_app();
        // Carátula de prueba: degradado.
        let img = RgbImage::from_fn(64, 64, |x, y| Rgb([(x * 4) as u8, (y * 4) as u8, 120]));
        let proto = app.picker.new_resize_protocol(DynamicImage::ImageRgb8(img));
        app.cover.replace_protocol(proto);

        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        // El primer dibujado pide redimensionar la imagen en segundo plano.
        let request = rx.try_recv().expect("debe pedirse un redimensionado");
        app.cover
            .update_resized_protocol(request.resize_encode().unwrap());
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let text = buffer_text(&terminal);
        println!("{text}");
        assert!(text.contains("Bohemian Rhapsody"));
        assert!(text.contains("Éxitos de hoy"));
        assert!(text.contains("Reproduciendo"));
        assert!(text.contains("1:23 / 4:54"));
        // Los halfblocks dibujan la imagen con el carácter ▀/▄.
        assert!(text.contains('▀') || text.contains('▄'));
    }

    #[test]
    fn renders_search_and_help_popups() {
        let (mut app, _rx) = demo_app();
        app.popup = Popup::Search;
        app.search_input = "queen".into();
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        println!("{text}");
        assert!(text.contains("Buscar"));
        assert!(text.contains("› queen"));

        app.popup = Popup::Help;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("Ayuda"));
    }
}

/// Pruebas de robustez: la interfaz no debe fallar con ningún tamaño de
/// terminal, estado ni texto, por raro que sea.
#[cfg(test)]
mod robustness {
    use std::time::{Duration, Instant};

    use ratatui::{Terminal, backend::TestBackend};
    use rspotify::model::{PlayableId, RepeatState, TrackId};

    use super::*;
    use crate::app::{TrackSource, tests::test_app};
    use crate::spotify::{ContextEntry, DeviceEntry, Playback, PlaylistEntry, TrackEntry};

    const WEIRD: [&str; 6] = [
        "",
        "🎵🔥 Canción con emojis 👨‍👩‍👧‍👦",
        "東京事変 — 群青日和 (2004) 「テスト」",
        "مرحبا بالعالم موسيقى",
        "tab\tnueva\nlínea\u{1b}[31mescape\u{0}nulo",
        "Nombre larguísimo que no cabe en ninguna columna de ninguna terminal razonable jamás",
    ];

    fn track(name: &str) -> TrackEntry {
        TrackEntry {
            name: name.into(),
            artists: name.into(),
            album: name.into(),
            duration_ms: u64::MAX / 2,
            id: TrackId::from_id("4u7EnebtmKWzUH433cf5Qv".to_string())
                .ok()
                .map(PlayableId::Track),
        }
    }

    fn playback(name: &str, progress_ms: u64, duration_ms: u64) -> Playback {
        Playback {
            is_playing: progress_ms.is_multiple_of(2),
            progress_ms,
            duration_ms,
            title: name.into(),
            artists: name.into(),
            album: name.into(),
            cover_url: None,
            item_uri: Some("spotify:track:4u7EnebtmKWzUH433cf5Qv".into()),
            shuffle: true,
            repeat: RepeatState::Track,
            device_name: name.into(),
            device_type: name.into(),
            volume: Some(100),
            fetched_at: Instant::now() - Duration::from_millis(1),
        }
    }

    fn all_states(app: &mut App, step: usize) {
        let name = WEIRD[step % WEIRD.len()];
        app.playback = match step % 4 {
            0 => None,
            1 => Some(playback(name, 0, 0)),
            2 => Some(playback(name, 10_000_000, 1_000)),
            _ => Some(playback(name, 61_000, 3_600_000)),
        };
        app.playlists = WEIRD
            .iter()
            .map(|n| PlaylistEntry {
                id: rspotify::model::PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string())
                    .unwrap(),
                name: n.to_string(),
                owner: n.to_string(),
                total: u32::MAX,
            })
            .collect();
        app.main = match step % 4 {
            0 => MainContent::Loading(name.into()),
            1 => MainContent::Message(name.into()),
            2 => MainContent::Tracks {
                title: name.into(),
                source: TrackSource::Uris,
                tracks: WEIRD.iter().map(|n| track(n)).collect(),
            },
            _ => MainContent::Contexts {
                title: name.into(),
                kind: SearchKind::ALL[step % 4],
                items: WEIRD
                    .iter()
                    .map(|n| ContextEntry {
                        name: n.to_string(),
                        detail: n.to_string(),
                        id: rspotify::model::PlayContextId::Artist(
                            rspotify::model::ArtistId::from_id(
                                "0OdUWJ0sBjDrqHygGUXeCF".to_string(),
                            )
                            .unwrap(),
                        ),
                    })
                    .collect(),
            },
        };
        // Selecciones fuera de rango también deben aguantar.
        app.main_state.select(Some(step * 7));
        app.library_state.select(Some(step * 3));
        app.big_cover = step.is_multiple_of(3);
        app.search_input = name.into();
        app.popup = match step % 5 {
            0 => Popup::Search,
            1 => Popup::Help,
            2 => Popup::Devices {
                devices: None,
                state: Default::default(),
            },
            3 => Popup::Devices {
                devices: Some(
                    WEIRD
                        .iter()
                        .map(|n| DeviceEntry {
                            id: None,
                            name: n.to_string(),
                            kind: n.to_string(),
                            active: true,
                        })
                        .collect(),
                ),
                state: Default::default(),
            },
            _ => Popup::None,
        };
    }

    #[tokio::test]
    async fn draws_every_state_at_every_size() {
        let (mut app, _rx) = test_app();
        let mut step = 0;
        let widths = std::iter::once(0).chain((1..=130).step_by(7));
        for width in widths {
            for height in std::iter::once(0).chain((1..=45).step_by(4)) {
                all_states(&mut app, step);
                step += 1;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| draw(f, &mut app)).unwrap();
                terminal
                    .draw(|f| draw_fallback(f, WEIRD[step % WEIRD.len()]))
                    .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod cover_robustness {
    use image::{DynamicImage, Rgb, RgbImage};
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::ProtocolType;

    use super::*;
    use crate::app::tests::test_app;

    /// La carátula se codifica con cada protocolo gráfico y a cualquier
    /// tamaño (incluso diminuto) sin fallar.
    #[tokio::test]
    async fn cover_renders_with_every_protocol_and_size() {
        for protocol in [
            ProtocolType::Halfblocks,
            ProtocolType::Sixel,
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
        ] {
            let (mut app, mut rx) = test_app();
            app.picker.set_protocol_type(protocol);
            for (w, h) in [(1, 1), (3, 2), (300, 1), (1, 300), (64, 64)] {
                let img = RgbImage::from_fn(w, h, |x, y| Rgb([x as u8, y as u8, 200]));
                let proto = app.picker.new_resize_protocol(DynamicImage::ImageRgb8(img));
                app.cover.replace_protocol(proto);
                for (width, height) in [(20, 14), (60, 20), (120, 40), (8, 12)] {
                    for big in [false, true] {
                        app.big_cover = big;
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal.draw(|f| draw(f, &mut app)).unwrap();
                        // Procesa los redimensionados pendientes, como el bucle real.
                        while let Ok(request) = rx.try_recv() {
                            if let Ok(resp) = request.resize_encode() {
                                app.cover.update_resized_protocol(resp);
                            }
                        }
                        terminal.draw(|f| draw(f, &mut app)).unwrap();
                    }
                }
            }
        }
    }
}
