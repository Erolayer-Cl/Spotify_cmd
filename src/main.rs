//! spotify_cmd — controla Spotify desde la terminal.

/// `println!` que nunca hace panic: si la terminal ya no existe (se cerró la
/// ventana), el texto se descarta en silencio.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// `print!` sin salto de línea (para preguntas), también a prueba de fallos.
macro_rules! say_inline {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut out = std::io::stdout();
        let _ = write!(out, $($arg)*);
        let _ = out.flush();
    }};
}

/// `eprintln!` a prueba de fallos.
macro_rules! say_err {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod app;
mod auth;
mod config;
mod keymap;
mod logo;
mod spotify;
mod terminal;
mod theme;
mod ui;

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::unbounded_channel;

use crate::app::{App, Msg};

const HELP: &str = "\
spotify_cmd — cliente de Spotify para la terminal

USO:
    spotify_cmd [OPCIONES]

OPCIONES:
    --manual-auth   Inicia sesión pegando la URL a mano (útil por SSH)
    --halfblocks    Dibuja la carátula con caracteres (sin protocolo gráfico)
    --logout        Borra el token guardado y sale
    -h, --help      Muestra esta ayuda
    -V, --version   Muestra la versión

CONFIGURACIÓN:
    ~/.config/spotify_cmd/config.toml  (client_id, redirect_uri, poll_secs)
    Variables: SPOTIFY_CLIENT_ID, SPOTIFY_REDIRECT_URI

Dentro de la aplicación pulsa '?' para ver los atajos de teclado.";

struct Args {
    manual_auth: bool,
    halfblocks: bool,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args {
        manual_auth: false,
        halfblocks: false,
    };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--manual-auth" => args.manual_auth = true,
            "--halfblocks" => args.halfblocks = true,
            "--logout" => {
                let path = config::token_path()?;
                if path.exists() {
                    std::fs::remove_file(&path)?;
                    say!("Sesión cerrada (token borrado: {}).", path.display());
                } else {
                    say!("No había ninguna sesión guardada.");
                }
                return Ok(None);
            }
            "-h" | "--help" => {
                say_inline!("{}", logo::banner());
                say!("\n{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                say!("spotify_cmd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => anyhow::bail!("opción desconocida: {other}\n\n{HELP}"),
        }
    }
    Ok(Some(args))
}

#[tokio::main]
async fn main() {
    let result = try_main().await;
    // Pase lo que pase, la terminal vuelve a su estado normal.
    terminal::restore();
    if let Err(e) = result {
        say_err!("Error: {e:#}");
        std::process::exit(1);
    }
}

async fn try_main() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    terminal::ensure_interactive()?;

    say_inline!("{}", logo::banner());
    let cfg = config::Config::load_or_create()?;
    let spotify = auth::authenticate(&cfg, args.manual_auth).await?;

    // A partir de aquí la terminal entra en modo "pantalla completa".
    let tui = terminal::init().context("no se pudo preparar la terminal")?;
    // La detección del protocolo gráfico (Kitty, Sixel, iTerm2) debe hacerse
    // tras entrar en la pantalla alternativa y antes de leer eventos.
    let picker = if args.halfblocks {
        Picker::halfblocks()
    } else {
        terminal::guard(Picker::from_query_stdio)
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(Picker::halfblocks)
    };

    let mut tui = tui;
    let result = run(&mut tui, spotify, picker, cfg.poll_secs).await;
    close_terminal(tui);
    result
}

/// Devuelve el cursor y suelta la terminal. Si la terminal ya no existe (se
/// cerró la ventana), se evita el `Drop` de ratatui, que intentaría escribir
/// en ella y fallaría.
fn close_terminal(mut tui: DefaultTerminal) {
    if tui.show_cursor().is_err() {
        std::mem::forget(tui);
    }
}

/// Ejecuta `f` sobre la app; un panic se muestra en pantalla en vez de
/// cerrarla. Todo brazo nuevo del bucle principal que procese algo con la app
/// debe pasar por aquí (el dibujado tiene su propia protección).
fn protected(app: &mut App, f: impl FnOnce(&mut App)) {
    if let Err(reason) = terminal::guard(|| f(app)) {
        app.report_crash(&reason);
    }
}

/// Tras este número de errores seguidos leyendo el teclado, se sale.
const MAX_INPUT_ERRORS: u32 = 20;
/// Tras este número de fallos seguidos al dibujar, se muestra una pantalla mínima.
const MAX_DRAW_FAILURES: u32 = 3;

async fn run(
    tui: &mut DefaultTerminal,
    spotify: spotify::Client,
    picker: Picker,
    poll_secs: u64,
) -> Result<()> {
    // Área de dibujo actual; `fit_to_window` la ajusta (y limpia la pantalla,
    // también en terminales sin pantalla alternativa) en cada vuelta.
    let mut area = ratatui::layout::Rect::default();
    let (resize_tx, mut resize_rx) = unbounded_channel();
    let mut app = App::new(spotify, picker, resize_tx, poll_secs);
    let msg_tx = app.sender();
    protected(&mut app, App::start);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let mut input_errors = 0;
    let mut draw_failures = 0;

    while !app.should_quit {
        // 0. Seguir el tamaño de la ventana, con tope de memoria.
        let _ = terminal::fit_to_window(tui, &mut area);

        // 1. Dibujar. Un fallo al dibujar no tumba la aplicación.
        match terminal::guard(|| tui.draw(|f| ui::draw(f, &mut app)).map(drop)) {
            Ok(result) => {
                result.context("no se pudo escribir en la terminal")?;
                draw_failures = 0;
            }
            Err(reason) => {
                draw_failures += 1;
                app.report_crash(&format!("al dibujar: {reason}"));
                let _ = terminal::full_redraw(tui);
                if draw_failures >= MAX_DRAW_FAILURES {
                    let _ = terminal::guard(|| tui.draw(|f| ui::draw_fallback(f, &reason)));
                }
            }
        }

        // 2. Esperar lo primero que ocurra.
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => {
                    input_errors = 0;
                    protected(&mut app, |a| a.on_event(event));
                }
                Some(Err(e)) => {
                    input_errors += 1;
                    if input_errors >= MAX_INPUT_ERRORS {
                        anyhow::bail!("no se puede leer el teclado: {e}");
                    }
                    app.show_error(format!("Error leyendo el teclado: {e}"));
                }
                // La entrada se cerró (por ejemplo, se cerró la ventana).
                None => break,
            },
            Some(msg) = app.rx.recv() => {
                protected(&mut app, |a| a.on_msg(msg));
            }
            Some(request) = resize_rx.recv() => {
                // Redimensionar y codificar la imagen es costoso: se hace fuera
                // del hilo de la interfaz.
                app::spawn_blocking_guarded(msg_tx.clone(), move || {
                    Msg::Resized(request.resize_encode())
                });
            }
            _ = tick.tick() => {
                protected(&mut app, App::on_tick);
            }
            () = &mut shutdown => break,
        }
    }
    Ok(())
}

/// Termina limpiamente si el sistema pide cerrar (SIGTERM, SIGHUP, SIGINT).
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    async fn wait(kind: SignalKind) {
        match signal(kind) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
    tokio::select! {
        () = wait(SignalKind::terminate()) => {}
        () = wait(SignalKind::hangup()) => {}
        () = wait(SignalKind::interrupt()) => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_shows_panics_instead_of_closing() {
        let (mut app, _rx) = app::tests::test_app();
        protected(&mut app, |_| panic!("fallo simulado"));
        let status = app.status.as_ref().map(|s| (s.text.as_str(), s.error));
        assert!(matches!(status, Some((text, true)) if text.contains("fallo simulado")));
        assert!(!app.should_quit);

        // Sin panic, la acción se aplica y no se informa de ningún error.
        app.status = None;
        protected(&mut app, |a| a.should_quit = true);
        assert!(app.should_quit);
        assert!(app.status.is_none());
    }
}
