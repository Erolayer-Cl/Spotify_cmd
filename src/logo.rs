//! Logo de Spotify en ASCII (círculo verde con las tres ondas).
//!
//! Generado con medios bloques (`▀ ▄ █`): cada carácter dibuja dos "píxeles"
//! en vertical, así el círculo sale redondo aunque las celdas sean altas.

use crate::theme;

/// Versión compacta: 9 filas × 18 columnas.
pub const LOGO_SMALL: [&str; 9] = [
    "    ▄▄██████▄▄    ",
    "  ▄████████████▄  ",
    " █████▀▀▀▀▀▀█████ ",
    "███  ▄▄▄▄▄▄▄▄  ███",
    "████▀▀▀    ▀▀▀████",
    "████▄█▀▀▀▀▀▀█▄████",
    " ████▄▄▄██▄▄▄████ ",
    "  ▀████████████▀  ",
    "    ▀▀██████▀▀    ",
];

/// Versión grande: 15 filas × 30 columnas.
pub const LOGO_LARGE: [&str; 15] = [
    "        ▄▄▄████████▄▄▄        ",
    "     ▄▄████████████████▄▄     ",
    "   ▄██████████████████████▄   ",
    "  ▄████████████████████████▄  ",
    " ████████▀▀▀▀▀▀▀▀▀▀▀▀████████ ",
    "▄████                    ████▄",
    "█████ ▄▄▄████████████▄▄▄ █████",
    "███████▀▀▀          ▀▀▀███████",
    "███████  ▄▄▄▄████▄▄▄▄  ███████",
    "▀█████████▀▀▀▀▀▀▀▀▀▀█████████▀",
    " ███████  ▄▄▄▄▄▄▄▄▄▄  ███████ ",
    "  ▀████████████████████████▀  ",
    "   ▀██████████████████████▀   ",
    "     ▀▀████████████████▀▀     ",
    "        ▀▀▀████████▀▀▀        ",
];

const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_RESET: &str = "\x1b[0m";

/// Secuencia del verde de la app: de 24 bits o, si la terminal no los
/// muestra, el más parecido de la paleta de 256 (ver [`theme::truecolor`]).
fn ansi_green() -> &'static str {
    if theme::truecolor() {
        "\x1b[38;2;30;215;96m"
    } else {
        "\x1b[38;5;41m"
    }
}

/// Cabecera verde que se imprime en la terminal al arrancar.
pub fn banner() -> String {
    let green = ansi_green();
    let mut out = String::new();
    for row in LOGO_LARGE {
        out.push_str(&format!("  {green}{row}{ANSI_RESET}\n"));
    }
    out.push_str(&format!(
        "\n  {green}{ANSI_BOLD}spotify_cmd{ANSI_RESET} {green}· Spotify en tu terminal{ANSI_RESET}\n"
    ));
    out
}

/// Escribe un texto en verde (para los mensajes fuera de la interfaz).
pub fn green(text: &str) -> String {
    format!("{}{text}{ANSI_RESET}", ansi_green())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_rows_have_equal_width() {
        // Filas del mismo ancho = el logo se centra sin deformarse.
        for logo in [&LOGO_SMALL[..], &LOGO_LARGE[..]] {
            let width = logo[0].chars().count();
            assert!(logo.iter().all(|row| row.chars().count() == width));
        }
    }
}
