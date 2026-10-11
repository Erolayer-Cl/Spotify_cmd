//! spotify_cmd — controla Spotify desde la terminal.

mod app;
mod auth;
mod config;
mod keymap;
mod logo;
mod spotify;
mod theme;
mod ui;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, EventStream};
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
                    println!("Sesión cerrada (token borrado: {}).", path.display());
                } else {
                    println!("No había ninguna sesión guardada.");
                }
                return Ok(None);
            }
            "-h" | "--help" => {
                print!("{}", logo::banner());
                println!("\n{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("spotify_cmd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => anyhow::bail!("opción desconocida: {other}\n\n{HELP}"),
        }
    }
    Ok(Some(args))
}

#[tokio::main]
async fn main() {
    if let Err(e) = try_main().await {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

async fn try_main() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    print!("{}", logo::banner());
    let cfg = config::Config::load_or_create()?;
    let spotify = auth::authenticate(&cfg, args.manual_auth).await?;

    // A partir de aquí la terminal entra en modo "pantalla completa".
    let terminal = ratatui::init();
    // La detección del protocolo gráfico (Kitty, Sixel, iTerm2) debe hacerse
    // tras entrar en la pantalla alternativa y antes de leer eventos.
    let picker = if args.halfblocks {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };

    // Permite pegar texto de una vez en el buscador. Si el programa falla, se
    // desactiva antes de restaurar la terminal (el hook de ratatui va después).
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        ratatui_hook(info);
    }));
    let result = run(terminal, spotify, picker, cfg.poll_secs).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn run(
    mut terminal: DefaultTerminal,
    spotify: spotify::Client,
    picker: Picker,
    poll_secs: u64,
) -> Result<()> {
    let (resize_tx, mut resize_rx) = unbounded_channel();
    let mut app = App::new(spotify, picker, resize_tx, poll_secs);
    let msg_tx = app.sender();
    app.start();

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !app.should_quit {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        tokio::select! {
            Some(event) = events.next() => app.on_event(event?),
            Some(msg) = app.rx.recv() => app.on_msg(msg),
            Some(request) = resize_rx.recv() => {
                // Redimensionar y codificar la imagen es costoso: se hace fuera
                // del hilo de la interfaz.
                let tx = msg_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send(Msg::Resized(request.resize_encode()));
                });
            }
            _ = tick.tick() => app.on_tick(),
        }
    }
    Ok(())
}
