//! Paleta y estilos reutilizables. Toda la interfaz se pinta en verde Spotify:
//! cambiar un color aquí lo cambia en todas las pantallas.
//!
//! [`finish_frame`] da los últimos retoques a cada fotograma: el fondo
//! oscuro propio y, en terminales sin color de 24 bits, los colores más
//! parecidos de la paleta de 256.

use std::sync::OnceLock;

use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType},
};

/// Fondo de toda la interfaz (el de Spotify). Con el fondo de la terminal,
/// en un tema claro el verde apenas se leería.
pub const BACKGROUND: Color = Color::Rgb(18, 18, 18);

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

// ---------------------------------------------------------------------------
// Retoques finales del fotograma
// ---------------------------------------------------------------------------

/// Último paso de cada fotograma: lo que no tiene color propio recibe el
/// fondo y el texto de la app, y sin color de 24 bits cada color RGB pasa
/// al más parecido de la paleta de 256 (si no, la terminal lo mostraría
/// mal o no lo mostraría).
pub fn finish_frame(buf: &mut Buffer) {
    let truecolor = truecolor();
    for cell in &mut buf.content {
        if cell.bg == Color::Reset {
            cell.bg = BACKGROUND;
        }
        if cell.fg == Color::Reset {
            cell.fg = GREEN;
        }
        if !truecolor {
            cell.fg = to_256(cell.fg);
            cell.bg = to_256(cell.bg);
        }
    }
}

/// Si la terminal muestra colores de 24 bits. Se mira una vez.
pub fn truecolor() -> bool {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| supports_truecolor(|name| std::env::var(name).ok()))
}

/// Decide, a partir de las variables de entorno que da `var`, si la
/// terminal muestra colores de 24 bits. `SPOTIFY_CMD_COLORS` (`256` o
/// `truecolor`) manda sobre lo detectado.
fn supports_truecolor(var: impl Fn(&str) -> Option<String>) -> bool {
    let lower = |name| var(name).map(|v| v.to_ascii_lowercase());
    match lower("SPOTIFY_CMD_COLORS").as_deref() {
        Some("256") => return false,
        Some("truecolor" | "24bit") => return true,
        _ => {}
    }
    if matches!(lower("COLORTERM").as_deref(), Some("truecolor" | "24bit")) {
        return true;
    }
    // Terminal.app de macOS no los entiende (muestra otros colores).
    if lower("TERM_PROGRAM").as_deref() == Some("apple_terminal") {
        return false;
    }
    // Terminales que los muestran aunque no lo anuncien en COLORTERM.
    let known_program = matches!(
        lower("TERM_PROGRAM").as_deref(),
        Some("iterm.app" | "wezterm" | "vscode" | "ghostty" | "hyper" | "tabby")
    );
    let known_term = lower("TERM").is_some_and(|term| {
        [
            "kitty",
            "ghostty",
            "alacritty",
            "foot",
            "wezterm",
            "direct",
            "truecolor",
        ]
        .iter()
        .any(|name| term.contains(name))
    });
    let windows_terminal = var("WT_SESSION").is_some();
    // La consola de Windows 10+ los admite siempre.
    known_program || known_term || windows_terminal || cfg!(windows)
}

/// Color de la paleta de 256 más parecido a uno RGB; los demás no cambian.
fn to_256(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_256(r, g, b)),
        other => other,
    }
}

/// Índice de la paleta de 256 (cubo de 6×6×6 o escala de grises) más
/// parecido a `(r, g, b)`. Se calcula directamente, sin recorrer la tabla:
/// la carátula por bloques tiene miles de colores distintos.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| match v {
        0..48 => 0,
        48..115 => 1,
        _ => (v - 35) / 40,
    };
    let distance = |(r2, g2, b2): (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(r, r2) + d(g, g2) + d(b, b2)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (
        LEVELS[usize::from(ri)],
        LEVELS[usize::from(gi)],
        LEVELS[usize::from(bi)],
    );
    // Gris más cercano a la media: niveles 8, 18, ..., 238 (índices 232-255).
    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_step = u8::try_from(mean.saturating_sub(3) / 10).map_or(23, |s| s.min(23));
    let gray = 8 + 10 * gray_step;
    if distance((gray, gray, gray)) < distance(cube) {
        232 + gray_step
    } else {
        16 + 36 * ri + 6 * gi + bi
    }
}

/// Atajo de teclado: `[tecla] descripción`.
pub fn key_hint(key: &str, action: &str) -> [Span<'static>; 2] {
    [
        Span::styled(format!(" {key} "), badge()),
        Span::styled(format!(" {action} "), dim()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn truecolor_is_detected_from_the_environment() {
        assert!(supports_truecolor(env(&[("COLORTERM", "truecolor")])));
        assert!(supports_truecolor(env(&[("COLORTERM", "24bit")])));
        assert!(supports_truecolor(env(&[("TERM", "xterm-kitty")])));
        assert!(supports_truecolor(env(&[("TERM_PROGRAM", "iTerm.app")])));
        assert!(supports_truecolor(env(&[("WT_SESSION", "x")])));
        let apple = [
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("TERM", "xterm-256color"),
        ];
        assert!(!supports_truecolor(env(&apple)));
        // La variable propia manda sobre lo detectado.
        let forced = [("COLORTERM", "truecolor"), ("SPOTIFY_CMD_COLORS", "256")];
        assert!(!supports_truecolor(env(&forced)));
        let forced = [
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("SPOTIFY_CMD_COLORS", "truecolor"),
        ];
        assert!(supports_truecolor(env(&forced)));
        if !cfg!(windows) {
            // Sin pistas (p. ej. por SSH) se usa la paleta de 256, que todas entienden.
            assert!(!supports_truecolor(env(&[("TERM", "xterm-256color")])));
        }
    }

    /// Los colores puros y los de la paleta caen en su índice exacto, y
    /// los grises en la escala de grises.
    #[test]
    fn rgb_maps_to_the_nearest_of_256() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(0, 255, 0), 46);
        assert_eq!(nearest_256(0, 215, 95), 41);
        assert_eq!(nearest_256(18, 18, 18), 233);
        assert_eq!(nearest_256(128, 128, 128), 244);
        // El verde de Spotify sigue siendo verde.
        assert_eq!(nearest_256(30, 215, 96), 41);
        // Todos los valores dan un índice de color (16-255), sin desbordes.
        for v in 0..=255u8 {
            assert!(nearest_256(v, v, v) >= 16);
            assert!(nearest_256(v, 255 - v, v / 2) >= 16);
        }
    }

    /// Lo que no tiene color propio recibe el de la app; el resto se
    /// conserva (con color de 24 bits) o pasa a la paleta de 256.
    #[test]
    fn finished_frame_has_no_default_colors() {
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buf.content[1].fg = Color::Indexed(7);
        buf.content[2].bg = SELECT_BG;
        finish_frame(&mut buf);
        let expected = |c: Color| if truecolor() { c } else { to_256(c) };
        assert_eq!(buf.content[0].bg, expected(BACKGROUND));
        assert_eq!(buf.content[0].fg, expected(GREEN));
        assert_eq!(buf.content[1].fg, Color::Indexed(7));
        assert_eq!(buf.content[2].bg, expected(SELECT_BG));
    }
}
