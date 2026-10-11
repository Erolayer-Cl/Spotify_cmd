//! Paleta y estilos reutilizables. Toda la interfaz se pinta en verde Spotify:
//! cambiar un color aquí lo cambia en todas las pantallas.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType},
};

/// Verde oficial de Spotify (#1ED760): texto principal y acentos.
pub const GREEN: Color = Color::Rgb(30, 215, 96);
/// Verde suave para texto secundario (artista, detalles).
pub const GREEN_SOFT: Color = Color::Rgb(125, 205, 150);
/// Verde apagado para bordes inactivos y datos poco importantes.
pub const GREEN_DIM: Color = Color::Rgb(60, 120, 80);
/// Fondo de la fila seleccionada.
pub const SELECT_BG: Color = Color::Rgb(18, 52, 32);
/// Parte vacía de la barra de progreso.
pub const TRACK_BG: Color = Color::Rgb(35, 60, 45);
pub const ERROR: Color = Color::Rgb(255, 95, 95);
pub const WARNING: Color = Color::Rgb(240, 200, 80);

/// Texto normal.
pub fn text() -> Style {
    Style::new().fg(GREEN)
}

/// Texto destacado (títulos, canción actual).
pub fn strong() -> Style {
    text().add_modifier(Modifier::BOLD)
}

/// Texto secundario.
pub fn soft() -> Style {
    Style::new().fg(GREEN_SOFT)
}

/// Texto poco importante (números, duraciones, ayudas).
pub fn dim() -> Style {
    Style::new().fg(GREEN_DIM)
}

/// Fila o elemento seleccionado en una lista.
pub fn selected() -> Style {
    Style::new().bg(SELECT_BG).add_modifier(Modifier::BOLD)
}

/// Etiqueta en negativo (tecla de un atajo, pestaña activa).
pub fn badge() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(GREEN)
        .add_modifier(Modifier::BOLD)
}

pub fn error() -> Style {
    Style::new().fg(ERROR).add_modifier(Modifier::BOLD)
}

pub fn warning() -> Style {
    Style::new().fg(WARNING).add_modifier(Modifier::BOLD)
}

/// Estilo de "encendido/apagado" para indicadores como aleatorio o repetir.
pub fn toggle(on: bool) -> Style {
    if on { strong() } else { dim() }
}

/// Recuadro redondeado con título. El borde brilla si el panel tiene el foco.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { text() } else { dim() })
        .title(title)
        .title_style(strong())
}

/// Recuadro de las ventanas emergentes, con una ayuda en el borde inferior.
pub fn popup<'a>(title: &'a str, hint: &'a str) -> Block<'a> {
    panel(format!(" {title} "), true).title_bottom(Line::from(format!(" {hint} ")).right_aligned())
}

/// Atajo de teclado: `[tecla] descripción`.
pub fn key_hint(key: &str, action: &str) -> [Span<'static>; 2] {
    [
        Span::styled(format!(" {key} "), badge()),
        Span::styled(format!(" {action} "), dim()),
    ]
}
