//! Preparar y restaurar la terminal, y la red de seguridad contra *panics*.
//!
//! Objetivo: que la aplicación no se caiga nunca. Si algo falla dentro de una
//! tarea o al dibujar, el error se captura, se muestra en la barra de estado y
//! la aplicación sigue funcionando. Solo un fallo fuera de esas zonas
//! protegidas restaura la terminal y termina el programa limpiamente.

use std::{
    any::Any,
    cell::Cell,
    io::{self, IsTerminal, stdout},
    panic::{self, AssertUnwindSafe},
    sync::atomic::{AtomicBool, Ordering},
};

use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    DefaultTerminal, Terminal, TerminalOptions, Viewport,
    backend::{Backend, CrosstermBackend},
    layout::{Rect, Size},
};

/// Tope del área de dibujo. ratatui reserva memoria por celda: sin tope, una
/// ventana absurda (p. ej. 6000×6000, que un pty permite) consumiría GB y el
/// sistema mataría la app. 1000×500 supera de sobra cualquier monitor real.
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;

/// Área de dibujo para un tamaño de ventana dado, con el tope aplicado.
pub fn capped_area(size: Size) -> Rect {
    Rect::new(0, 0, size.width.min(MAX_COLS), size.height.min(MAX_ROWS))
}

/// `true` mientras la terminal está en modo pantalla completa.
static ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// `true` mientras el hilo ejecuta código protegido por [`guard`].
    static GUARDED: Cell<bool> = const { Cell::new(false) };
}

/// Comprueba que hay una terminal interactiva antes de empezar.
pub fn ensure_interactive() -> anyhow::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "spotify_cmd necesita una terminal interactiva (no redirijas la entrada ni la salida)"
        );
    }
    Ok(())
}

/// Entra en pantalla completa y modo "raw" (cada tecla llega al instante).
pub fn init() -> io::Result<DefaultTerminal> {
    install_panic_hook();
    restore_at_exit();
    enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    if let Err(e) = execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste) {
        restore();
        return Err(e);
    }
    let backend = CrosstermBackend::new(stdout());
    let area = capped_area(backend.size().inspect_err(|_| restore())?);
    // Ventana fija: el tamaño lo ajusta `fit_to_window`, siempre con tope.
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(area),
        },
    )
    .inspect_err(|_| restore())
}

/// Ajusta el área de dibujo al tamaño actual de la ventana (con tope). Hace el
/// papel de `autoresize`, que ratatui no aplica a ventanas fijas.
pub fn fit_to_window(tui: &mut DefaultTerminal, current: &mut Rect) -> io::Result<()> {
    let area = capped_area(tui.size()?);
    if area != *current {
        tui.resize(area)?;
        *current = area;
    }
    Ok(())
}

/// Borra la pantalla y obliga a redibujarla entera en el siguiente frame.
///
/// A diferencia de `Terminal::clear`, no pregunta a la terminal la posición
/// del cursor: algunas terminales (o un SSH lento) no responden a tiempo y eso
/// haría fallar la aplicación.
pub fn full_redraw<B: Backend>(tui: &mut Terminal<B>) -> Result<(), B::Error> {
    let area = capped_area(tui.size()?);
    tui.resize(area)
}

/// Deja la terminal como estaba. Se puede llamar varias veces sin problema, y
/// no hace nada si la terminal nunca se preparó.
pub fn restore() {
    restore_stderr();
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}

/// Si algo termina el proceso con `exit()` desde otro hilo (librespot lo
/// hace ante algunos fallos internos), la terminal se restaura igual: si no,
/// quedaría en modo "raw" y sin eco.
fn restore_at_exit() {
    #[cfg(unix)]
    {
        extern "C" fn at_exit() {
            let _ = panic::catch_unwind(restore);
        }
        static REGISTERED: AtomicBool = AtomicBool::new(false);
        if !REGISTERED.swap(true, Ordering::SeqCst) {
            // SAFETY: `at_exit` es una función `extern "C"` sin argumentos
            // que nunca deja escapar un *panic*.
            unsafe {
                libc::atexit(at_exit);
            }
        }
    }
}

/// Copia de la salida de errores original mientras está desviada (-1: no
/// lo está).
#[cfg(unix)]
static SAVED_STDERR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Mientras la interfaz está abierta, lo que se escriba en la salida de
/// errores va a `path`: bibliotecas de C como ALSA escriben ahí sus avisos y
/// en pantalla romperían el dibujo. [`restore`] la devuelve a su sitio.
#[cfg_attr(not(feature = "local-player"), allow(dead_code))]
pub fn redirect_stderr(path: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::create(path)?;
        // SAFETY: llamadas al sistema sobre descriptores válidos; los
        // errores se comprueban y no se pierde ninguno abierto.
        unsafe {
            let saved = libc::dup(libc::STDERR_FILENO);
            if saved < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) < 0 {
                let err = io::Error::last_os_error();
                libc::close(saved);
                return Err(err);
            }
            let previous = SAVED_STDERR.swap(saved, Ordering::SeqCst);
            if previous >= 0 {
                libc::close(previous);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Devuelve la salida de errores a su sitio si estaba desviada.
pub fn restore_stderr() {
    #[cfg(unix)]
    {
        let saved = SAVED_STDERR.swap(-1, Ordering::SeqCst);
        if saved >= 0 {
            // SAFETY: `saved` es la copia válida que hizo `redirect_stderr`.
            unsafe {
                libc::dup2(saved, libc::STDERR_FILENO);
                libc::close(saved);
            }
        }
    }
}

/// Los *panics* de código protegido o de tareas en segundo plano no tocan la
/// terminal (la aplicación sigue viva y los muestra en pantalla). El resto
/// restaura la terminal antes de imprimir el error.
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let in_main_thread = std::thread::current().name() == Some("main");
        let guarded = GUARDED.with(Cell::get);
        if in_main_thread && !guarded {
            restore();
            default_hook(info);
        }
    }));
}

/// Ejecuta `f` capturando cualquier *panic* y devolviéndolo como texto.
pub fn guard<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    let previous = GUARDED.with(|g| g.replace(true));
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    GUARDED.with(|g| g.set(previous));
    result.map_err(panic_message)
}

/// Texto legible de la carga de un *panic*.
pub fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "error interno desconocido".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_turns_panics_into_errors() {
        assert_eq!(guard(|| 2 + 2), Ok(4));
        let err = guard(|| -> u8 { panic!("boom") }).unwrap_err();
        assert_eq!(err, "boom");
        let err = guard(|| -> u8 { panic!("{}", String::from("dinámico")) }).unwrap_err();
        assert_eq!(err, "dinámico");
        // Tras capturar, el hilo vuelve a estar desprotegido.
        assert!(!GUARDED.with(Cell::get));
    }

    /// Lo que se escribe en la salida de errores mientras está desviada va
    /// al archivo, y `restore` la devuelve a su sitio (también sin haber
    /// preparado la terminal).
    #[cfg(unix)]
    #[test]
    fn stderr_goes_to_the_log_while_redirected() {
        let path = std::env::temp_dir().join(format!(
            "spotify_cmd_test_{}_stderr.log",
            std::process::id()
        ));
        let write = |text: &str| {
            // SAFETY: escribe un búfer válido en el descriptor 2.
            unsafe { libc::write(libc::STDERR_FILENO, text.as_ptr().cast(), text.len()) };
        };
        redirect_stderr(&path).unwrap();
        write("aviso de ALSA\n");
        restore();
        write("");
        assert_eq!(SAVED_STDERR.load(Ordering::SeqCst), -1);
        let logged = std::fs::read_to_string(&path).unwrap();
        assert!(logged.contains("aviso de ALSA"), "{logged:?}");
        let _ = std::fs::remove_file(&path);
    }
}
