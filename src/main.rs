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
use ratatui::{DefaultTerminal, Terminal, backend::Backend};
use ratatui_image::{picker::Picker, thread::ResizeRequest};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::app::{App, CrashOrigin, Msg};

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
                logout()?;
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

/// Borra el token guardado y sus temporales (`--logout`).
fn logout() -> Result<()> {
    let path = config::token_path()?;
    if config::delete_session(&path)? {
        say!("Sesión cerrada (token borrado: {}).", path.display());
    } else {
        say!("No había ninguna sesión guardada.");
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let result = try_main().await;
    // Pase lo que pase, la terminal vuelve a su estado normal.
    terminal::restore();
    // Se termina aquí, sin volver de `main`: al destruir el runtime, tokio
    // esperaría sin límite a los hilos de `spawn_blocking` (un guardado
    // colgado en un disco de red) y el tope de `RENEWAL_GRACE` no serviría.
    std::process::exit(exit_code(result));
}

/// Código de salida del programa. Si hubo un error, lo muestra.
fn exit_code(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            say_err!("Error: {e:#}");
            1
        }
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
    // La terminal se devuelve antes de esperar a la renovación: si tarda (sin
    // red), el usuario ve su consola y un aviso, no una app congelada.
    close_terminal(tui);
    terminal::restore();
    finish_renewal().await;
    result
}

/// Tope de cada petición HTTP de rspotify: su cliente `reqwest` usa
/// `timeout(10 s)`.
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Margen para guardar en disco el token renovado.
const SAVE_GRACE: Duration = Duration::from_secs(5);
/// Tiempo máximo que se espera, al salir, a una renovación del token en curso
/// y a su guardado. Una renovación puede hacer dos peticiones de token (si
/// Spotify rechaza el refresh token se adopta el de `token.json` y se renueva
/// con él) y cortarla antes de la respuesta dejaría en disco un refresh token
/// ya gastado: el tope cubre las dos y el guardado.
const RENEWAL_GRACE: Duration = TOKEN_REQUEST_TIMEOUT
    .saturating_mul(2)
    .saturating_add(SAVE_GRACE);
/// Si al salir la espera a la renovación pasa de este tiempo, se avisa.
const RENEWAL_NOTICE: Duration = Duration::from_millis(300);

/// Una renovación del token en curso (y su guardado) termina antes de salir:
/// cortada a medias, dejaría en disco un refresh token ya gastado y habría
/// que volver a entrar en el navegador. La espera tiene tope y, si se nota,
/// se avisa de qué se está esperando.
async fn finish_renewal() {
    wait_with_notice(
        spotify::wait_for_renewal,
        RENEWAL_GRACE,
        RENEWAL_NOTICE,
        || say!("Guardando la sesión…"),
    )
    .await;
}

/// Espera con `wait` (que recibe el tope y devuelve si terminó a tiempo)
/// como mucho `max` en total. Si a los `notice_after` aún no ha terminado,
/// llama a `notice` una vez y sigue esperando el resto. Devuelve si terminó.
async fn wait_with_notice<Fut>(
    wait: impl Fn(Duration) -> Fut,
    max: Duration,
    notice_after: Duration,
    notice: impl FnOnce(),
) -> bool
where
    Fut: Future<Output = bool>,
{
    let first = notice_after.min(max);
    if wait(first).await {
        return true;
    }
    notice();
    wait(max.saturating_sub(first)).await
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
        // 0. Seguir el tamaño de la ventana, con tope de memoria, y repintar
        // la pantalla entera si la app lo pidió.
        let _ = terminal::fit_to_window(tui, &mut area);
        redraw_if_requested(tui, &mut app);

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

        // Los redimensionados de la carátula solo se piden al dibujar.
        dispatch_resizes(&mut resize_rx, &app, &msg_tx);

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
            _ = tick.tick() => {
                protected(&mut app, App::on_tick);
            }
            () = &mut shutdown => break,
        }
    }
    Ok(())
}

/// Borra la pantalla y la vuelve a dibujar entera en el siguiente frame si la
/// app lo pidió (al cerrar una ventana sobre una carátula Sixel o iTerm2, ver
/// [`App::take_full_redraw`]). Así la carátula se reenvía entera y no quedan
/// restos de la ventana encima.
fn redraw_if_requested<B: Backend>(tui: &mut Terminal<B>, app: &mut App) {
    if app.take_full_redraw() {
        let _ = terminal::full_redraw(tui);
    }
}

/// Lanza los redimensionados de la carátula pedidos al dibujar. Redimensionar
/// y codificar la imagen es costoso: se hace fuera del hilo de la interfaz.
///
/// Se llama justo después de dibujar, así que la generación de la carátula es
/// la de cada petición: si la carátula cambia mientras tanto, la respuesta (o
/// el *panic*) de una petición obsoleta se reconoce y se ignora.
fn dispatch_resizes(
    requests: &mut UnboundedReceiver<ResizeRequest>,
    app: &App,
    tx: &UnboundedSender<Msg>,
) {
    let generation = app.cover_generation();
    while let Ok(request) = requests.try_recv() {
        let origin = CrashOrigin::Resize(generation);
        app::spawn_blocking_guarded(tx.clone(), origin, move || Msg::Resized {
            generation,
            result: request.resize_encode(),
        });
    }
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

    /// Dibuja la carátula grande y lanza los redimensionados que pida.
    async fn draw_and_resize(
        app: &mut App,
        requests: &mut UnboundedReceiver<ResizeRequest>,
    ) -> Msg {
        let backend = ratatui::backend::TestBackend::new(60, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let (tx, mut rx) = unbounded_channel();
        dispatch_resizes(requests, app, &tx);
        rx.recv().await.expect("se pidió un redimensionado")
    }

    fn cover() -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::new(16, 16))
    }

    /// Los redimensionados pedidos al dibujar llevan la generación de la
    /// carátula: la respuesta de la actual se aplica.
    #[tokio::test]
    async fn resizes_carry_the_cover_generation() {
        let (mut app, mut requests) = app::tests::test_app();
        app.big_cover = true;
        app.set_cover_image(cover());
        let msg = draw_and_resize(&mut app, &mut requests).await;
        let current = app.cover_generation();
        assert!(matches!(msg, Msg::Resized { generation, .. } if generation == current));
        app.on_msg(msg);
        assert!(app.cover.protocol_type().is_some());
    }

    /// Generación que lleva la respuesta de un redimensionado.
    fn generation_of(msg: &Msg) -> u64 {
        match msg {
            Msg::Resized { generation, .. } => *generation,
            _ => panic!("se esperaba Msg::Resized"),
        }
    }

    /// Si la carátula cambia mientras se redimensiona la anterior, lo que
    /// llegue de la vieja (su respuesta, su error o su *panic*) se ignora: la
    /// nueva sigue redimensionándose, sin gastar su reintento, y su propia
    /// respuesta se aplica.
    #[tokio::test]
    async fn resize_of_a_replaced_cover_is_ignored() {
        let (mut app, mut requests) = app::tests::test_app();
        app.big_cover = true;
        app.set_cover_image(cover());
        let old = draw_and_resize(&mut app, &mut requests).await;
        let stale = generation_of(&old);

        app.set_cover_image(cover());
        let fresh = draw_and_resize(&mut app, &mut requests).await;
        assert_ne!(generation_of(&fresh), stale);
        app.on_msg(app::tests::resize_error(stale));
        app.on_msg(old);
        assert!(app.status.is_none(), "el error viejo no se muestra");
        // El *panic* se informa (como todos), pero no toca la carátula.
        app.on_msg(app::tests::crash(CrashOrigin::Resize(stale), "panic viejo"));
        assert!(
            app.cover.protocol_type().is_none(),
            "la nueva sigue redimensionándose"
        );
        assert_eq!(app::tests::cover_rebuilds(&app), 0);

        app.on_msg(fresh);
        assert!(app.cover.protocol_type().is_some(), "la nueva queda lista");
    }

    /// Una vuelta del bucle principal sobre `tui`: el repintado pedido, el
    /// dibujado y los redimensionados de la carátula, aplicados al momento.
    fn frame(
        tui: &mut Terminal<ratatui::backend::TestBackend>,
        app: &mut App,
        requests: &mut UnboundedReceiver<ResizeRequest>,
    ) -> String {
        redraw_if_requested(tui, app);
        tui.draw(|f| ui::draw(f, app)).unwrap();
        while let Ok(request) = requests.try_recv() {
            app.cover
                .update_resized_protocol(request.resize_encode().unwrap());
        }
        let buf = tui.backend().buffer();
        buf.content().iter().map(|cell| cell.symbol()).collect()
    }

    /// Pulsación de una tecla sin modificadores.
    fn press(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Ventana que se abre y se cierra sobre la carátula en una prueba: su
    /// título, la tecla que la abre y la que la cierra.
    struct PopupCase {
        title: &'static str,
        open: crossterm::event::KeyCode,
        close: crossterm::event::KeyCode,
    }

    /// Abre y cierra la ventana `case` sobre la carátula (grande si `big`)
    /// dibujada con `protocol` en una terminal de `size`. Devuelve la pantalla
    /// antes de abrirla y después de cerrarla.
    fn open_and_close_over_cover(
        protocol: ratatui_image::picker::ProtocolType,
        big: bool,
        size: (u16, u16),
        case: &PopupCase,
    ) -> (String, String) {
        let (mut app, mut requests) = app::tests::test_app();
        app.picker.set_protocol_type(protocol);
        app.big_cover = big;
        app.set_cover_image(cover());
        let backend = ratatui::backend::TestBackend::new(size.0, size.1);
        let mut tui = Terminal::new(backend).unwrap();
        frame(&mut tui, &mut app, &mut requests);
        let before = frame(&mut tui, &mut app, &mut requests);
        assert!(
            before.contains('\u{1b}'),
            "{protocol:?}: se dibuja la imagen"
        );

        app.on_event(press(case.open));
        let open = frame(&mut tui, &mut app, &mut requests);
        assert!(open.contains(case.title), "{}: se abre", case.title);
        app.on_event(press(case.close));
        (before, frame(&mut tui, &mut app, &mut requests))
    }

    /// Con Sixel o iTerm2 la carátula va entera en su primera celda. La
    /// ayuda y los dispositivos tapan parte de la carátula (pequeña o grande)
    /// pero no esa celda: al cerrarlos, la pantalla debe quedar como antes de
    /// abrirlos, sin restos de la ventana encima de la imagen. (El buscador
    /// no se prueba aquí: tapa esa celda, y al reenviarse la imagen borra su
    /// hueco con secuencias que `TestBackend` no interpreta.)
    #[tokio::test]
    async fn closing_popups_over_a_sixel_or_iterm2_cover_leaves_no_trace() {
        use crossterm::event::KeyCode;
        use ratatui_image::picker::ProtocolType;
        let help = PopupCase {
            title: "Ayuda",
            open: KeyCode::Char('?'),
            close: KeyCode::Char('x'),
        };
        let devices = PopupCase {
            title: "Dispositivos",
            open: KeyCode::Char('d'),
            close: KeyCode::Esc,
        };
        for protocol in [ProtocolType::Sixel, ProtocolType::Iterm2] {
            for (case, big, size) in [
                (&help, false, (80, 24)),
                (&help, true, (120, 40)),
                (&devices, false, (80, 24)),
                (&devices, true, (120, 40)),
            ] {
                let (before, after) = open_and_close_over_cover(protocol, big, size, case);
                assert_eq!(
                    after, before,
                    "{protocol:?}, {} (grande: {big}, {size:?}): quedan restos",
                    case.title
                );
            }
        }
    }

    #[test]
    fn exit_code_reports_errors() {
        assert_eq!(exit_code(Ok(())), 0);
        assert_eq!(exit_code(Err(anyhow::anyhow!("fallo simulado"))), 1);
    }

    /// Espera falsa para [`wait_with_notice`]: el trabajo termina a los
    /// `work` desde ahora.
    fn fake_wait(
        work: Duration,
    ) -> impl Fn(Duration) -> std::pin::Pin<Box<dyn Future<Output = bool>>> {
        let done_at = tokio::time::Instant::now() + work;
        move |max| {
            Box::pin(async move {
                tokio::time::timeout(max, tokio::time::sleep_until(done_at))
                    .await
                    .is_ok()
            })
        }
    }

    /// Espera simulada, sin dormir: el trabajo termina a los `work` de un
    /// reloj virtual que solo avanza al esperar.
    fn virtual_wait(work: Duration) -> impl Fn(Duration) -> std::future::Ready<bool> {
        let clock = std::cell::Cell::new(Duration::ZERO);
        move |max| {
            let now = clock.get().saturating_add(max).min(work);
            clock.set(now);
            std::future::ready(now >= work)
        }
    }

    /// Al salir se espera a la renovación más larga posible (dos peticiones
    /// de token que agotan el tope de rspotify) y a su guardado: cortarla
    /// antes dejaría en disco un refresh token ya gastado.
    #[tokio::test]
    async fn exit_waits_for_the_longest_renewal() {
        let longest = TOKEN_REQUEST_TIMEOUT * 2 + Duration::from_secs(1);
        let wait = virtual_wait(longest);
        let done = wait_with_notice(wait, RENEWAL_GRACE, RENEWAL_NOTICE, || {}).await;
        assert!(done);
    }

    /// Una espera corta no avisa de nada.
    #[tokio::test]
    async fn quick_exit_wait_has_no_notice() {
        let wait = fake_wait(Duration::from_millis(10));
        let mut noticed = false;
        let max = Duration::from_secs(5);
        let done = wait_with_notice(wait, max, Duration::from_millis(500), || noticed = true).await;
        assert!(done && !noticed);
    }

    /// Si la espera se nota, se avisa una sola vez y se sigue esperando.
    #[tokio::test]
    async fn slow_exit_wait_gives_notice_and_finishes() {
        let wait = fake_wait(Duration::from_millis(300));
        let mut notices = 0;
        let max = Duration::from_secs(5);
        let done = wait_with_notice(wait, max, Duration::from_millis(50), || notices += 1).await;
        assert!(done);
        assert_eq!(notices, 1);
    }

    /// Con el trabajo colgado, la espera total no pasa del tope.
    #[tokio::test]
    async fn hung_exit_wait_is_bounded_overall() {
        let wait = fake_wait(Duration::from_secs(60));
        let mut noticed = false;
        let start = std::time::Instant::now();
        let max = Duration::from_millis(300);
        let done = wait_with_notice(wait, max, Duration::from_millis(100), || noticed = true).await;
        let waited = start.elapsed();
        assert!(!done && noticed);
        assert!(
            waited >= max && waited < Duration::from_secs(3),
            "{waited:?}"
        );
    }
}
