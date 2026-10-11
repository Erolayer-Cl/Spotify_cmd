//! Dibujado de la interfaz con ratatui.
//!
//! Cada zona de la pantalla tiene su propia función `draw_*`, y las piezas que
//! se repiten (paneles, tablas, logo, atajos) son funciones pequeñas
//! reutilizables. Los colores salen de [`crate::theme`].

use ratatui::{
    Frame,
    buffer::CellWidth,
    layout::{Alignment, Constraint, Flex, Layout, Position, Rect},
    style::Style,
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
/// Tamaño máximo de la carátula grande (en filas; el ancho es el doble).
const MAX_COVER_ROWS: u16 = 60;

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

/// Logos que se prueban para acompañar a un mensaje, del más vistoso al más
/// compacto, con las filas en blanco que los separan de él.
const LOGO_CHOICES: [(&[&str], u16); 3] = [(&LOGO_LARGE, 1), (&LOGO_SMALL, 1), (&LOGO_SMALL, 0)];

/// Convierte un número de filas o columnas a `u16` sin desbordarse.
fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// El logo más grande que cabe en `area` junto a un mensaje de `text_rows`
/// filas, con las filas que ocupa (separación incluida). `None` si no cabe
/// ninguno: el mensaje tiene prioridad.
fn logo_that_fits(area: Rect, text_rows: u16) -> Option<(&'static [&'static str], u16)> {
    LOGO_CHOICES.into_iter().find_map(|(logo, gap)| {
        let rows = to_u16(logo.len()).saturating_add(gap);
        let width = logo.first().map_or(0, |row| row.cell_width());
        let fits = rows.saturating_add(text_rows) <= area.height && width <= area.width;
        fits.then_some((logo, rows))
    })
}

/// Logo centrado con un mensaje debajo (pantallas vacías o de carga). El
/// alto del mensaje se mide ya ajustado al ancho, y el logo solo se dibuja si
/// cabe junto al mensaje entero: nunca lo tapa ni lo empuja fuera.
fn draw_logo_message(f: &mut Frame, area: Rect, message: &str) {
    let lines: Vec<Line> = message
        .lines()
        .map(|l| Line::styled(l.to_string(), theme::soft()))
        .collect();
    let text = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    let text_rows = to_u16(text.line_count(area.width));
    let logo = logo_that_fits(area, text_rows);
    let logo_rows = logo.map_or(0, |(_, rows)| rows);

    let block = centered(area, area.width, logo_rows.saturating_add(text_rows));
    let [logo_area, text_area] =
        Layout::vertical([Constraint::Length(logo_rows), Constraint::Fill(1)]).areas(block);
    if let Some((logo, _)) = logo {
        let logo = Paragraph::new(logo_lines(logo)).alignment(Alignment::Center);
        f.render_widget(logo, logo_area);
    }
    f.render_widget(text, text_area);
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
    // Id (sin "spotify:track:") de la canción actual (suene o esté en pausa),
    // para marcarla en la lista.
    let current_id = app
        .playback
        .as_ref()
        .and_then(|p| p.item_uri.as_deref())
        .and_then(|uri| uri.rsplit(':').next());

    match &app.main {
        MainContent::Loading(text)
        | MainContent::Message(text)
        | MainContent::Unlisted { message: text, .. } => {
            let block = theme::panel(" Lista ", focused);
            let inner = block.inner(area);
            f.render_widget(block, area);
            draw_logo_message(f, inner, text);
        }
        MainContent::Tracks { title, tracks, .. } => {
            let rows = tracks
                .iter()
                .enumerate()
                .map(|(i, t)| track_row(i, t, is_current_track(t, current_id)))
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

/// `true` si `track` es la canción actual (suene o esté en pausa). Compara
/// ids sin reservar memoria: se llama para cada fila en cada frame.
fn is_current_track(track: &TrackEntry, current_id: Option<&str>) -> bool {
    match (&track.id, current_id) {
        (Some(id), Some(current)) => id.id() == current,
        _ => false,
    }
}

/// Fila de una canción; la canción actual se marca con ♪ y en negrita.
/// Toma el texto prestado de `track`, sin copiarlo.
fn track_row(index: usize, track: &TrackEntry, current: bool) -> Row<'_> {
    let (number, number_style, artist_style, row_style) = if current {
        let strong = theme::strong();
        ("♪".to_string(), strong, strong, strong)
    } else {
        let row = if track.id.is_none() {
            theme::dim()
        } else {
            theme::text()
        };
        ((index + 1).to_string(), theme::dim(), theme::soft(), row)
    };
    Row::new(vec![
        Cell::from(number).style(number_style),
        Cell::from(track.name.as_str()),
        Cell::from(track.artists.as_str()).style(artist_style),
        Cell::from(track.album.as_str()).style(theme::dim()),
        Cell::from(fmt_ms(track.duration_ms)).style(theme::dim()),
    ])
    .style(row_style)
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
    if !app.has_cover() {
        // Sin carátula (cargando o nada sonando): el logo de Spotify.
        let logo = Paragraph::new(logo_lines(&LOGO_SMALL)).alignment(Alignment::Center);
        let height = (LOGO_SMALL.len() as u16).min(area.height);
        f.render_widget(logo, centered(area, area.width, height));
        return;
    }
    // `Scale` amplía también las imágenes pequeñas para ocupar todo el hueco.
    // Mientras se redimensiona no se dibuja nada (ni el logo, que parpadearía).
    let widget = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle)));
    f.render_stateful_widget(widget, area, &mut app.cover);
}

fn draw_big_cover(f: &mut Frame, app: &mut App, area: Rect) {
    let block = theme::panel(" Carátula (c para volver) ", has_focus(app, Focus::Main));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Cuadrado lo más grande posible, centrado, con tope: en sixel/kitty la
    // imagen se genera a resolución real (filas × alto de la fuente).
    let side = inner.height.min(inner.width / 2).min(MAX_COVER_ROWS);
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
    let block = theme::popup("Buscar", "Tab: tipo · Enter: buscar · Esc: cerrar");
    let inner = block.inner(area);
    // Columnas para el texto: todo menos el indicador y el cursor.
    let room = inner
        .width
        .saturating_sub(SEARCH_PROMPT.cell_width().saturating_add(1));
    let (shown, shown_width) = tail_that_fits(&app.search_input, room);
    let lines = vec![
        Line::from_iter(std::iter::once(Span::raw(" ")).chain(tabs)),
        Line::from(vec![
            Span::styled(SEARCH_PROMPT, theme::strong()),
            Span::styled(shown, theme::text()),
            Span::styled("█", theme::text()),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
    // El cursor real va detrás del texto: así se ve dónde se escribe y los
    // métodos de entrada (japonés, chino…) componen en el buscador.
    if let Some(cursor) = search_cursor(inner, shown_width) {
        f.set_cursor_position(cursor);
    }
}

/// Indicador delante del texto del buscador.
const SEARCH_PROMPT: &str = " › ";

/// Final de `text` que cabe en `max` columnas, medido por grafemas igual que
/// lo dibuja ratatui. Si no cabe entero empieza por «…»: al escribir, lo que
/// importa es el final. Devuelve el texto y su ancho en columnas.
fn tail_that_fits(text: &str, max: u16) -> (String, u16) {
    let span = Span::raw(text);
    let graphemes: Vec<&str> = span
        .styled_graphemes(Style::default())
        .map(|g| g.symbol)
        .collect();
    let total = graphemes
        .iter()
        .fold(0u16, |acc, g| acc.saturating_add(g.cell_width()));
    if total <= max {
        return (graphemes.concat(), total);
    }
    let Some(room) = max.checked_sub(1) else {
        return (String::new(), 0);
    };
    let mut width = 0u16;
    let kept = graphemes
        .iter()
        .rev()
        .take_while(|g| {
            let next = width.saturating_add(g.cell_width());
            let fits = next <= room;
            if fits {
                width = next;
            }
            fits
        })
        .count();
    let start = graphemes.len().saturating_sub(kept);
    let tail: String = graphemes.iter().skip(start).copied().collect();
    (format!("…{tail}"), width.saturating_add(1))
}

/// Posición del cursor real en el buscador: detrás de las `text_width`
/// columnas de texto, en la segunda fila del recuadro. `None` si esa fila o
/// esa columna no se ven.
fn search_cursor(inner: Rect, text_width: u16) -> Option<Position> {
    let x = inner
        .x
        .saturating_add(SEARCH_PROMPT.cell_width())
        .saturating_add(text_width);
    let y = inner.y.saturating_add(1);
    (inner.height >= 2 && x < inner.right()).then_some(Position { x, y })
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
            position: None,
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
        app.set_cover_image(DynamicImage::ImageRgb8(img));

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

    /// Mientras la carátula se redimensiona no aparece el logo (parpadeo):
    /// el logo es solo para cuando no hay carátula.
    #[test]
    fn resizing_cover_does_not_flash_the_logo() {
        let (mut app, mut rx) = demo_app();
        let logo_row = LOGO_SMALL[0].trim();
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(
            buffer_text(&terminal).contains(logo_row),
            "sin carátula, el logo"
        );

        let img = RgbImage::from_fn(64, 64, |x, y| Rgb([(x * 4) as u8, (y * 4) as u8, 120]));
        app.set_cover_image(DynamicImage::ImageRgb8(img));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let request = rx.try_recv().expect("debe pedirse un redimensionado");
        // El redimensionado aún no ha vuelto.
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(!buffer_text(&terminal).contains(logo_row));

        app.cover
            .update_resized_protocol(request.resize_encode().unwrap());
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(!text.contains(logo_row));
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

    /// Dibuja la app en una terminal de `width × height` y devuelve el texto.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        buffer_text(&terminal)
    }

    /// A 80×24 (el tamaño por defecto de muchas terminales) el mensaje del
    /// panel principal se ve, y el logo lo acompaña si cabe.
    #[test]
    fn main_message_is_visible_at_80x24() {
        let (mut app, _rx) = demo_app();
        app.playback = None;
        for text in [
            "Elige una lista en la biblioteca (Enter).",
            "Cargando «Tus me gusta»…",
            "Sin resultados.",
        ] {
            app.main = MainContent::Message(text.into());
            let screen = screen(&mut app, 80, 24);
            assert!(screen.contains(text), "no se ve «{text}»:\n{screen}");
            // Un logo en el panel principal y otro en el reproductor.
            assert_eq!(screen.matches(LOGO_SMALL[0].trim()).count(), 2, "{screen}");
        }
    }

    /// Si la primera línea del mensaje ocupa varias filas al ajustarse, la
    /// última línea se sigue viendo.
    #[test]
    fn wrapped_message_keeps_its_last_line() {
        let (mut app, _rx) = demo_app();
        app.main = MainContent::Unlisted {
            title: "Éxitos de hoy".into(),
            message: "No se pudo cargar «Éxitos de hoy»: Spotify solo deja ver las \
                      canciones de tus playlists o de las colaborativas\n\n\
                      Pulsa Enter para reproducir todas sus canciones."
                .into(),
            context: rspotify::model::PlayContextId::Playlist(
                PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M".to_string()).unwrap(),
            ),
        };
        for (width, height) in [(80, 24), (100, 30), (120, 40), (160, 50)] {
            let screen = screen(&mut app, width, height);
            assert!(
                screen.contains("Pulsa Enter para reproducir todas sus canciones."),
                "{width}×{height}:\n{screen}"
            );
            assert!(screen.contains("No se pudo cargar"), "{width}×{height}");
        }
    }

    /// La pantalla de emergencia muestra el mensaje entero aunque el motivo
    /// sea largo y se ajuste en varias filas.
    #[test]
    fn fallback_shows_the_whole_message() {
        let reason = "index out of bounds: the len is 3 but the index is 7 en \
                      src/ui.rs al dibujar la lista principal";
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| draw_fallback(f, reason)).unwrap();
        let screen = buffer_text(&terminal);
        // La última línea, ajustada, termina en una fila propia.
        assert!(screen.contains("salir."), "{screen}");
        assert!(screen.contains("principal"), "{screen}");
    }

    /// El logo solo se elige si cabe junto al mensaje entero: primero el
    /// grande, luego el pequeño (con separación o sin ella) y, si no, ninguno.
    #[test]
    fn logo_that_fits_leaves_room_for_the_message() {
        let area = |width, height| Rect::new(0, 0, width, height);
        let chosen = |width, height, text_rows| {
            logo_that_fits(area(width, height), text_rows).map(|(logo, rows)| (logo.len(), rows))
        };
        assert_eq!(chosen(57, 17, 1), Some((LOGO_LARGE.len(), 16)));
        assert_eq!(chosen(57, 16, 1), Some((LOGO_SMALL.len(), 10)));
        assert_eq!(chosen(57, 11, 1), Some((LOGO_SMALL.len(), 10)));
        assert_eq!(chosen(57, 10, 1), Some((LOGO_SMALL.len(), 9)));
        assert_eq!(chosen(57, 10, 2), None);
        assert_eq!(chosen(57, 3, 5), None);
        // Más estrecho que el logo grande: el pequeño; más que el pequeño, nada.
        assert_eq!(chosen(20, 40, 1), Some((LOGO_SMALL.len(), 10)));
        assert_eq!(chosen(17, 40, 1), None);
    }

    /// Con una búsqueda más larga que el campo se ve el final (con «…»
    /// delante) y el cursor real queda detrás del texto, donde se escribe.
    #[test]
    fn long_search_shows_its_end_and_the_real_cursor() {
        let (mut app, _rx) = demo_app();
        app.popup = Popup::Search;
        app.search_input = "Bohemian Rhapsody Remastered 2011 Queen A Night at the Opera".into();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Fila del texto: borde superior, pestañas y texto.
        let row: String = (0..buf.area.width).map(|x| buf[(x, 2)].symbol()).collect();
        assert!(row.contains("› …"), "{row}");
        assert!(row.contains("at the Opera█"), "{row}");
        let cursor_x = (0..buf.area.width)
            .find(|&x| buf[(x, 2)].symbol() == "█")
            .unwrap();
        terminal.backend_mut().assert_cursor_position((cursor_x, 2));
    }

    /// Una búsqueda corta se ve entera y el cursor va justo detrás.
    #[test]
    fn short_search_cursor_follows_the_text() {
        let (mut app, _rx) = demo_app();
        app.popup = Popup::Search;
        app.search_input = "東京".into();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        // Biblioteca (21 columnas) + borde + " › " + 4 columnas de texto.
        let x = 21 + 1 + 3 + 4;
        assert_eq!(terminal.backend().buffer()[(x, 2)].symbol(), "█");
        terminal.backend_mut().assert_cursor_position((x, 2));
    }

    #[test]
    fn tail_that_fits_keeps_the_end() {
        let fit = tail_that_fits;
        assert_eq!(fit("queen", 10), ("queen".into(), 5));
        assert_eq!(fit("queen", 5), ("queen".into(), 5));
        assert_eq!(fit("queen", 4), ("…een".into(), 4));
        assert_eq!(fit("queen", 1), ("…".into(), 1));
        assert_eq!(fit("queen", 0), (String::new(), 0));
        assert_eq!(fit("", 0), (String::new(), 0));
        // Caracteres de dos columnas: nunca se pasa del ancho.
        assert_eq!(fit("東京事変", 8), ("東京事変".into(), 8));
        assert_eq!(fit("東京事変", 5), ("…事変".into(), 5));
        assert_eq!(fit("東京事変", 4), ("…変".into(), 3));
        // Un grafema compuesto (familia) no se parte.
        let family = "👨‍👩‍👧‍👦";
        assert_eq!(fit(&format!("a{family}"), 3), (format!("a{family}"), 3));
        assert_eq!(fit(&format!("ab{family}"), 3), (format!("…{family}"), 3));
    }

    #[test]
    fn search_cursor_stays_inside_the_field() {
        let inner = Rect::new(5, 1, 10, 2);
        assert_eq!(search_cursor(inner, 0), Some(Position { x: 8, y: 2 }));
        assert_eq!(search_cursor(inner, 6), Some(Position { x: 14, y: 2 }));
        assert_eq!(search_cursor(inner, 7), None);
        // Sin la fila del texto (panel muy bajo), no hay cursor.
        assert_eq!(search_cursor(Rect::new(5, 1, 10, 1), 0), None);
        assert_eq!(search_cursor(Rect::new(5, 1, 2, 2), 0), None);
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
            position: None,
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

    #[test]
    fn is_current_track_compares_ids() {
        let current = track("actual");
        assert!(is_current_track(&current, Some("4u7EnebtmKWzUH433cf5Qv")));
        assert!(!is_current_track(&current, Some("5ChkMS8OtdzJeqyybCc9R5")));
        assert!(!is_current_track(&current, None));
        // Una canción local (sin id) nunca es la actual.
        let local = TrackEntry {
            id: None,
            ..track("local")
        };
        assert!(!is_current_track(&local, Some("4u7EnebtmKWzUH433cf5Qv")));
    }

    #[test]
    fn track_row_styles_current_normal_and_local_tracks() {
        let (current, normal) = (track("a"), track("b"));
        let local = TrackEntry {
            id: None,
            ..track("c")
        };
        let rows = vec![
            track_row(0, &current, true),
            track_row(1, &normal, false),
            track_row(2, &local, false),
        ];
        // Columnas: número (0..4), título (5..9), artista (10..14), ...
        let widths = [Constraint::Length(4); 5];
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).unwrap();
        terminal
            .draw(|f| f.render_widget(Table::new(rows, widths), f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let fg = |x: u16, y: u16| Some(buf[(x, y)].fg);
        let bold = |x: u16, y: u16| {
            buf[(x, y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        };

        // La canción actual: ♪ y todo en negrita.
        assert_eq!(buf[(0, 0)].symbol(), "♪");
        assert!(bold(0, 0) && bold(5, 0) && bold(10, 0));
        assert_eq!(fg(10, 0), theme::strong().fg);
        // Una canción normal: número tenue, título normal, artista suave.
        assert_eq!(buf[(0, 1)].symbol(), "2");
        assert!(!bold(0, 1) && !bold(5, 1));
        assert_eq!(fg(0, 1), theme::dim().fg);
        assert_eq!(fg(5, 1), theme::text().fg);
        assert_eq!(fg(10, 1), theme::soft().fg);
        // Una canción local (sin id): el título se atenúa.
        assert_eq!(buf[(0, 2)].symbol(), "3");
        assert_eq!(fg(5, 2), theme::dim().fg);
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
                app.set_cover_image(DynamicImage::ImageRgb8(img));
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
