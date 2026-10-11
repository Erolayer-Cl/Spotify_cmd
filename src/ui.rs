//! Dibujado de la interfaz con ratatui.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Cell, Clear, LineGauge, List, ListItem, Paragraph, Row, Table, Wrap,
    },
};
use ratatui_image::{FilterType, Resize, StatefulImage};

use crate::app::{App, FIXED_LIBRARY, Focus, LibraryItem, MainContent, Popup, repeat_label};
use crate::spotify::SearchKind;

const GREEN: Color = Color::Rgb(30, 215, 96);
const DIM: Color = Color::Rgb(140, 140, 140);
const SELECT_BG: Color = Color::Rgb(40, 40, 40);

pub fn draw(f: &mut Frame, app: &mut App) {
    let [body, player, footer] = Layout::vertical([
        Constraint::Min(8),
        Constraint::Length(11),
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

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { GREEN } else { DIM }))
        .title(title)
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ---------------------------------------------------------------------------
// Biblioteca (lateral)
// ---------------------------------------------------------------------------

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Library && matches!(app.popup, Popup::None);
    let mut items: Vec<ListItem> = FIXED_LIBRARY
        .iter()
        .map(|item| {
            let (icon, name) = match item {
                LibraryItem::Liked => ("♥", "Tus me gusta"),
                LibraryItem::Recent => ("◷", "Recientes"),
                LibraryItem::Queue => ("≡", "Cola"),
                LibraryItem::Playlist(_) => unreachable!(),
            };
            library_line(
                icon,
                name.to_string(),
                None,
                app.active_library == Some(*item),
            )
        })
        .collect();

    items.extend(app.playlists.iter().enumerate().map(|(i, p)| {
        library_line(
            "♫",
            p.name.clone(),
            Some(p.total),
            app.active_library == Some(LibraryItem::Playlist(i)),
        )
    }));

    let title = if app.playlists.is_empty() {
        " Biblioteca ".to_string()
    } else {
        format!(" Biblioteca · {} playlists ", app.playlists.len())
    };
    let list = List::new(items)
        .block(panel(title, focused))
        .highlight_style(Style::new().bg(SELECT_BG).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, area, &mut app.library_state);
}

fn library_line(
    icon: &'static str,
    name: String,
    total: Option<u32>,
    active: bool,
) -> ListItem<'static> {
    let color = if active { GREEN } else { Color::Reset };
    let mut spans = vec![
        Span::styled(format!("{icon} "), Style::new().fg(GREEN)),
        Span::styled(name, Style::new().fg(color)),
    ];
    if let Some(total) = total {
        spans.push(Span::styled(format!(" {total}"), Style::new().fg(DIM)));
    }
    ListItem::new(Line::from(spans))
}

// ---------------------------------------------------------------------------
// Lista principal
// ---------------------------------------------------------------------------

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Main && matches!(app.popup, Popup::None);
    let playing_uri = app.playback.as_ref().and_then(|p| p.item_uri.clone());

    match &app.main {
        MainContent::Loading(text) | MainContent::Message(text) => {
            let block = panel(" Lista ", focused);
            let inner = block.inner(area);
            f.render_widget(block, area);
            let [_, text_area] =
                Layout::vertical([Constraint::Percentage(35), Constraint::Fill(1)]).areas(inner);
            let p = Paragraph::new(text.clone())
                .style(Style::new().fg(DIM))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true });
            f.render_widget(p, text_area);
        }
        MainContent::Tracks { title, tracks, .. } => {
            let rows = tracks.iter().enumerate().map(|(i, t)| {
                let is_playing = playing_uri.is_some() && t.uri() == playing_uri;
                let num = if is_playing {
                    "♪".to_string()
                } else {
                    (i + 1).to_string()
                };
                let style = if is_playing {
                    Style::new().fg(GREEN).add_modifier(Modifier::BOLD)
                } else if t.id.is_none() {
                    Style::new().fg(DIM)
                } else {
                    Style::new()
                };
                Row::new(vec![
                    Cell::from(num).style(Style::new().fg(if is_playing { GREEN } else { DIM })),
                    Cell::from(t.name.clone()),
                    Cell::from(t.artists.clone()).style(Style::new().fg(if is_playing {
                        GREEN
                    } else {
                        Color::Gray
                    })),
                    Cell::from(t.album.clone()).style(Style::new().fg(DIM)),
                    Cell::from(fmt_ms(t.duration_ms)).style(Style::new().fg(DIM)),
                ])
                .style(style)
            });
            let header = Row::new(["#", "Título", "Artista", "Álbum", "Dur."])
                .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))
                .bottom_margin(1);
            let table = Table::new(
                rows,
                [
                    Constraint::Length(4),
                    Constraint::Percentage(38),
                    Constraint::Percentage(27),
                    Constraint::Percentage(25),
                    Constraint::Length(7),
                ],
            )
            .header(header)
            .block(panel(format!(" {title} · {} ", tracks.len()), focused))
            .row_highlight_style(Style::new().bg(SELECT_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌");
            f.render_stateful_widget(table, area, &mut app.main_state);
        }
        MainContent::Contexts { title, kind, items } => {
            let icon = match kind {
                SearchKind::Albums => "◉",
                SearchKind::Artists => "☺",
                SearchKind::Playlists => "♫",
                SearchKind::Tracks => "♪",
            };
            let rows = items.iter().map(|c| {
                Row::new(vec![
                    Cell::from(icon).style(Style::new().fg(GREEN)),
                    Cell::from(c.name.clone()),
                    Cell::from(c.detail.clone()).style(Style::new().fg(DIM)),
                ])
            });
            let header = Row::new(["", "Nombre", "Detalle"])
                .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD))
                .bottom_margin(1);
            let table = Table::new(
                rows,
                [
                    Constraint::Length(2),
                    Constraint::Percentage(50),
                    Constraint::Percentage(50),
                ],
            )
            .header(header)
            .block(panel(format!(" {title} "), focused))
            .row_highlight_style(Style::new().bg(SELECT_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌");
            f.render_stateful_widget(table, area, &mut app.main_state);
        }
    }
}

// ---------------------------------------------------------------------------
// Reproductor
// ---------------------------------------------------------------------------

fn draw_player(f: &mut Frame, app: &mut App, area: Rect) {
    let block = panel(" Reproduciendo ", false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let info_area = if app.big_cover {
        inner
    } else {
        // Las celdas de la terminal son ~el doble de altas que anchas: un
        // cuadrado de N filas ocupa ~2N columnas.
        let cover_w = (inner.height * 2 + 1).min(inner.width / 3);
        let [cover, _gap, info] = Layout::horizontal([
            Constraint::Length(cover_w),
            Constraint::Length(2),
            Constraint::Min(10),
        ])
        .areas(inner);
        draw_cover(f, app, cover);
        info
    };

    let Some(pb) = &app.playback else {
        let lines = vec![
            Line::from(""),
            Line::from("No hay nada reproduciéndose.".bold()),
            Line::from(""),
            Line::from(Span::styled(
                "Abre Spotify en cualquier dispositivo, o elige una canción y pulsa Enter.",
                Style::new().fg(DIM),
            )),
            Line::from(Span::styled(
                "Pulsa 'd' para elegir el dispositivo donde reproducir.",
                Style::new().fg(DIM),
            )),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), info_area);
        return;
    };

    let [text_area, _, gauge_area] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(info_area);

    let state = if pb.is_playing {
        Span::styled("▶ Reproduciendo", Style::new().fg(GREEN).bold())
    } else {
        Span::styled("⏸ En pausa", Style::new().fg(Color::Yellow).bold())
    };
    let on_off = |on: bool| {
        if on {
            Style::new().fg(GREEN)
        } else {
            Style::new().fg(DIM)
        }
    };
    let sep = || Span::styled("   ", Style::new());
    let mut status = vec![
        state,
        sep(),
        Span::styled(
            format!("⇄ Aleatorio: {}", if pb.shuffle { "sí" } else { "no" }),
            on_off(pb.shuffle),
        ),
        sep(),
        Span::styled(
            format!("↻ Repetir: {}", repeat_label(pb.repeat)),
            on_off(pb.repeat != rspotify::model::RepeatState::Off),
        ),
    ];
    if let Some(v) = pb.volume {
        status.push(sep());
        status.push(Span::styled(
            format!("Vol {v}%"),
            Style::new().fg(Color::Gray),
        ));
    }

    let lines = vec![
        Line::from(Span::styled(
            pb.title.clone(),
            Style::new().fg(Color::White).bold(),
        )),
        Line::from(Span::styled(pb.artists.clone(), Style::new().fg(GREEN))),
        Line::from(Span::styled(
            pb.album.clone(),
            Style::new().fg(DIM).italic(),
        )),
        Line::from(""),
        Line::from(status),
        Line::from(Span::styled(
            format!("Dispositivo: {} ({})", pb.device_name, pb.device_type),
            Style::new().fg(DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), text_area);

    let progress = pb.current_progress_ms();
    let ratio = if pb.duration_ms > 0 {
        (progress as f64 / pb.duration_ms as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let gauge = LineGauge::default()
        .ratio(ratio)
        .label(format!(
            "{} / {} ",
            fmt_ms(progress),
            fmt_ms(pb.duration_ms)
        ))
        .filled_symbol("━")
        .unfilled_symbol("─")
        .filled_style(Style::new().fg(GREEN))
        .unfilled_style(Style::new().fg(Color::Rgb(70, 70, 70)));
    f.render_widget(gauge, gauge_area);
}

fn draw_cover(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    if app.cover.protocol_type().is_none() {
        // Marcador mientras se descarga la carátula (o si no hay ninguna).
        let placeholder = Paragraph::new(vec![Line::from(""), Line::from("♪").fg(DIM)])
            .alignment(Alignment::Center)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(DIM)),
            );
        f.render_widget(placeholder, area);
        return;
    }
    // `Scale` amplía también las imágenes pequeñas para ocupar todo el hueco.
    let widget = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle)));
    f.render_stateful_widget(widget, area, &mut app.cover);
}

fn draw_big_cover(f: &mut Frame, app: &mut App, area: Rect) {
    let block = panel(" Carátula (c para volver) ", app.focus == Focus::Main);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Cuadrado lo más grande posible, centrado.
    let side_h = inner.height.min(inner.width / 2);
    let side_w = side_h * 2;
    let [_, col, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(side_w),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let [_, cell, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(side_h),
        Constraint::Fill(1),
    ])
    .areas(col);
    draw_cover(f, app, cell);
}

// ---------------------------------------------------------------------------
// Pie, buscador y ventanas emergentes
// ---------------------------------------------------------------------------

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some(status) = app.visible_status() {
        let style = if status.error {
            Style::new().fg(Color::Red).bold()
        } else {
            Style::new().fg(GREEN)
        };
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {}", status.text), style)),
            area,
        );
        return;
    }
    let keys: &[(&str, &str)] = &[
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
    let mut spans = Vec::new();
    for (k, v) in keys {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::new().fg(Color::Black).bg(GREEN),
        ));
        spans.push(Span::styled(format!(" {v} "), Style::new().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_search(f: &mut Frame, app: &App, main: Rect) {
    let area = Rect {
        height: 4.min(main.height),
        ..main
    };
    f.render_widget(Clear, area);

    let mut tabs = vec![Span::raw(" ")];
    for kind in SearchKind::ALL {
        let style = if kind == app.search_kind {
            Style::new().fg(Color::Black).bg(GREEN).bold()
        } else {
            Style::new().fg(DIM)
        };
        tabs.push(Span::styled(format!(" {} ", kind.label()), style));
        tabs.push(Span::raw(" "));
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(GREEN))
        .title(" Buscar ")
        .title_bottom(Line::from(" Tab: tipo · Enter: buscar · Esc: cerrar ").right_aligned());
    let lines = vec![
        Line::from(tabs),
        Line::from(vec![
            Span::styled(" › ", Style::new().fg(GREEN).bold()),
            Span::raw(app.search_input.clone()),
            Span::styled("█", Style::new().fg(GREEN)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(v);
    h
}

fn draw_devices(f: &mut Frame, app: &mut App) {
    let Popup::Devices { devices, state } = &mut app.popup else {
        return;
    };
    let area = centered(f.area(), 56, 12);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(GREEN))
        .title(" Dispositivos ")
        .title_bottom(Line::from(" Enter: reproducir aquí · Esc: cerrar ").right_aligned());

    match devices {
        None => f.render_widget(Paragraph::new(" Buscando dispositivos…").block(block), area),
        Some(list) if list.is_empty() => f.render_widget(
            Paragraph::new(
                " No se encontró ningún dispositivo.\n\n Abre la app de Spotify en tu móvil, \
                 ordenador o altavoz y vuelve a pulsar 'd'.",
            )
            .wrap(Wrap { trim: false })
            .block(block),
            area,
        ),
        Some(list) => {
            let items: Vec<ListItem> = list
                .iter()
                .map(|d| {
                    let mut spans = vec![
                        Span::raw(d.name.clone()),
                        Span::styled(format!("  ({})", d.kind), Style::new().fg(DIM)),
                    ];
                    if d.active {
                        spans.push(Span::styled("  ● activo", Style::new().fg(GREEN)));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::new().bg(SELECT_BG).bold())
                .highlight_symbol("▌");
            f.render_stateful_widget(list, area, state);
        }
    }
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 62, 26);
    f.render_widget(Clear, area);
    let rows: &[(&str, &str)] = &[
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
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::from(Span::styled(format!(" {k}"), Style::new().fg(GREEN).bold()))
            } else {
                Line::from(vec![
                    Span::styled(format!("   {k:<12}"), Style::new().fg(Color::White).bold()),
                    Span::styled(v.to_string(), Style::new().fg(Color::Gray)),
                ])
            }
        })
        .collect();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(GREEN))
        .title(" Ayuda ")
        .title_bottom(Line::from(" Pulsa cualquier tecla para cerrar ").right_aligned());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use image::{DynamicImage, Rgb, RgbImage};
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;
    use rspotify::model::{PlayableId, PlaylistId, RepeatState, TrackId};
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::{
        app::TrackSource,
        spotify::{Client, Playback, PlaylistEntry, TrackEntry},
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
        let (tx, rx) = unbounded_channel();
        let mut app = App::new(Client::default(), Picker::halfblocks(), tx, 3);
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
