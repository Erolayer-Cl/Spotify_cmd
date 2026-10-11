//! Logo de Spotify en ASCII (círculo verde con las tres ondas).
//!
//! Generado con medios bloques (`▀ ▄ █`): cada carácter dibuja dos "píxeles"
//! en vertical, así el círculo sale redondo aunque las celdas sean altas.

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

const ANSI_GREEN: &str = "\x1b[38;2;30;215;96m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_RESET: &str = "\x1b[0m";

/// Cabecera verde que se imprime en la terminal al arrancar.
pub fn banner() -> String {
    let mut out = String::new();
    for row in LOGO_LARGE {
        out.push_str(&format!("  {ANSI_GREEN}{row}{ANSI_RESET}\n"));
    }
    out.push_str(&format!(
        "\n  {ANSI_GREEN}{ANSI_BOLD}spotify_cmd{ANSI_RESET} {ANSI_GREEN}· Spotify en tu terminal{ANSI_RESET}\n"
    ));
    out
}

/// Escribe un texto en verde (para los mensajes fuera de la interfaz).
pub fn green(text: &str) -> String {
    format!("{ANSI_GREEN}{text}{ANSI_RESET}")
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
