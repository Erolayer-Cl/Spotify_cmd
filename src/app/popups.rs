//! Ventanas emergentes: el buscador, donde las teclas escriben texto en vez
//! de ejecutar órdenes, y la lista de dispositivos donde reproducir.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use ratatui_image::picker::ProtocolType;

use super::{App, Msg, Popup};
use crate::keymap::{Command, step_index};
use crate::spotify::{self, Action, DeviceEntry};

impl App {
    // -----------------------------------------------------------------------
    // Cierre
    // -----------------------------------------------------------------------

    /// Cierra la ventana emergente abierta. Todas se cierran por aquí: si la
    /// ventana pudo dejar restos sobre la carátula, se pide repintar la
    /// pantalla entera (ver [`leaves_trace_on_cover`]).
    pub(super) fn close_popup(&mut self) {
        let closed = std::mem::replace(&mut self.popup, Popup::None);
        if leaves_trace_on_cover(&closed, self.picker.protocol_type()) {
            self.full_redraw = true;
        }
        // La reproducción pendiente solo vale mientras se elige dispositivo.
        if matches!(closed, Popup::Devices { .. }) {
            self.pending_play = None;
        }
    }

    /// `true` (una sola vez) si hay que repintar la pantalla entera antes del
    /// siguiente dibujado.
    pub fn take_full_redraw(&mut self) -> bool {
        std::mem::take(&mut self.full_redraw)
    }

    // -----------------------------------------------------------------------
    // Buscador
    // -----------------------------------------------------------------------

    /// Abre el buscador. La búsqueda anterior queda a la vista (Tab + Enter
    /// la repite con otro tipo), pero lo primero que se escriba la sustituye.
    pub(super) fn open_search(&mut self) {
        self.popup = Popup::Search;
        self.search_fresh = !self.search_input.is_empty();
    }

    /// Escribe en el buscador: si aún se muestra la búsqueda anterior, la
    /// sustituye en lugar de añadirse detrás. Lo que pase de
    /// [`MAX_SEARCH_CHARS`] se descarta.
    fn type_in_search(&mut self, text: &str) {
        if std::mem::take(&mut self.search_fresh) {
            self.search_input.clear();
        }
        let room = MAX_SEARCH_CHARS.saturating_sub(self.search_input.chars().count());
        self.search_input.extend(text.chars().take(room));
    }

    /// En el buscador las teclas escriben texto en vez de ejecutar órdenes.
    pub(super) fn on_search_key(&mut self, key: KeyEvent) {
        // Cualquier tecla de edición (borrar, cambiar de tipo) acepta la
        // búsqueda anterior como punto de partida.
        if !matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Esc)
            || key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.search_fresh = false;
        }
        match key.code {
            KeyCode::Esc => self.close_popup(),
            KeyCode::Enter => {
                self.close_popup();
                self.run_search();
            }
            KeyCode::Tab => self.search_kind = self.search_kind.next(),
            KeyCode::BackTab => self.search_kind = self.search_kind.prev(),
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            // Algunas terminales envían Ctrl+H como retroceso.
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.clear();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                delete_last_word(&mut self.search_input);
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                self.type_in_search(c.encode_utf8(&mut [0; 4]));
            }
            _ => {}
        }
    }

    /// Texto pegado en el buscador. De un pegado enorme solo se mira el
    /// principio: el resto no cabría.
    pub(super) fn on_search_paste(&mut self, text: &str) {
        let start = text
            .char_indices()
            .nth(MAX_SEARCH_CHARS * 4)
            .map_or(text, |(end, _)| &text[..end]);
        self.type_in_search(&pasted_text(start));
    }

    // -----------------------------------------------------------------------
    // Dispositivos
    // -----------------------------------------------------------------------

    /// Abre la ventana de dispositivos y los pide a Spotify. La respuesta de
    /// una petición anterior que siga en curso ya no se aplicará.
    pub(super) fn open_devices(&mut self) {
        self.popup = Popup::Devices {
            devices: None,
            state: ListState::default(),
        };
        self.devices_request = self.devices_request.wrapping_add(1);
        let request = self.devices_request;
        self.spawn(
            |sp| async move { spotify::devices(&sp).await },
            move |result| Msg::Devices { request, result },
        );
    }

    /// Orden recibida con la ventana de dispositivos abierta.
    pub(super) fn on_devices_command(&mut self, command: Command) {
        let Popup::Devices { devices, state } = &mut self.popup else {
            return;
        };
        match command {
            Command::Back | Command::Quit => self.close_popup(),
            // 'd' vuelve a buscar (como indica la ventana sin dispositivos),
            // salvo que ya se esté buscando.
            Command::OpenDevices => {
                if devices.is_some() {
                    self.open_devices();
                }
            }
            Command::Move(_) | Command::Jump(_) => {
                let len = devices.as_ref().map_or(0, Vec::len);
                state.select(step_index(state.selected(), len, command));
            }
            Command::Activate => {
                // Mientras se busca, o sin dispositivos, la ventana sigue
                // abierta con su explicación.
                let Some(list) = devices.as_ref().filter(|l| !l.is_empty()) else {
                    return;
                };
                let device = state
                    .selected()
                    .and_then(|i| list.get(i))
                    .and_then(|d| d.id.clone());
                let pending = self.pending_play.take();
                self.close_popup();
                match device {
                    Some(device_id) => self.do_action(device_choice(pending, device_id)),
                    None => self.set_status("Ese dispositivo no se puede controlar", true),
                }
            }
            _ => {}
        }
    }

    /// Respuesta de la petición `request` de la lista de dispositivos. Solo
    /// se aplica la de la última petición, y si la ventana sigue abierta: una
    /// anterior cerraría la ventana con un error falso o cambiaría la lista
    /// mientras se elige.
    pub(super) fn on_devices(&mut self, request: u64, result: Result<Vec<DeviceEntry>>) {
        if request != self.devices_request {
            return;
        }
        if let Popup::Devices { devices, state } = &mut self.popup {
            match result {
                Ok(list) => {
                    let active = list.iter().position(|d| d.active).unwrap_or(0);
                    state.select((!list.is_empty()).then_some(active));
                    *devices = Some(list);
                }
                Err(e) => {
                    self.close_popup();
                    self.show_request_error("No se pudieron listar los dispositivos", &e);
                }
            }
        }
    }
}

/// Si cerrar `popup` puede dejar restos de la ventana sobre la carátula.
///
/// Con Sixel o iTerm2 la imagen entera va en su primera celda y ratatui no
/// vuelve a enviar las demás. La ayuda y los dispositivos, centrados en la
/// pantalla, tapan parte de la carátula pero no esa celda: al cerrarlos, la
/// imagen no se reenvía y lo que pintaron encima se queda. El buscador no
/// hace falta: arriba del todo del panel, o tapa esa celda (y la imagen se
/// reenvía, borrando antes su hueco) o no toca la carátula.
fn leaves_trace_on_cover(popup: &Popup, protocol: ProtocolType) -> bool {
    matches!(popup, Popup::Help | Popup::Devices { .. })
        && matches!(protocol, ProtocolType::Sixel | ProtocolType::Iterm2)
}

/// Acción al elegir el dispositivo `device_id`: si la ventana se abrió para
/// la reproducción `pending` (que falló sin dispositivo), se repite en el
/// elegido; si no, se le pasa lo que esté sonando.
fn device_choice(pending: Option<Action>, device_id: String) -> Action {
    pending
        .and_then(|play| play.on_device(device_id.clone()))
        .unwrap_or(Action::Transfer { device_id })
}

/// Largo máximo de una búsqueda, en caracteres. Ninguna búsqueda real se
/// acerca; un pegado enorme (un archivo entero) haría lento cada dibujado y
/// Spotify rechazaría la consulta.
const MAX_SEARCH_CHARS: usize = 200;

/// Texto pegado en el buscador: los saltos de línea y tabuladores separan
/// palabras (se cambian por un espacio) y el resto de caracteres de control
/// se quitan.
fn pasted_text(text: &str) -> String {
    text.split(char::is_control)
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Si una tecla con estos modificadores escribe texto: sin Ctrl ni Alt, o
/// con los dos a la vez (AltGr en Windows, para @, # o €).
fn is_text_input(modifiers: KeyModifiers) -> bool {
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    !modifiers.intersects(ctrl_alt) || modifiers.contains(ctrl_alt)
}

/// Borra la última palabra y los espacios que la siguen (Ctrl+W).
fn delete_last_word(text: &mut String) {
    let end = text.trim_end().len();
    text.truncate(end);
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    text.truncate(start);
}

#[cfg(test)]
mod tests {
    use crossterm::event::Event;

    use super::*;
    use crate::app::tests::{album, crash, key, status, test_app};
    use crate::app::{CrashOrigin, MainContent};
    use crate::spotify::{ApiError, SearchKind, SearchResults};

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    /// Al reabrir el buscador, lo que se escribe sustituye a la búsqueda
    /// anterior (antes quedaba «queenabba»); Tab + Enter la repite.
    #[tokio::test]
    async fn reopened_search_replaces_the_old_query() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('/')));
        for c in "queen".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Char('/')));
        assert!(app.search_fresh, "la búsqueda anterior se ve atenuada");
        for c in "abba".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        assert_eq!(app.search_input, "abba");

        // Tab (cambiar de tipo) conserva la búsqueda para repetirla.
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Char('/')));
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Char('s')));
        assert_eq!(app.search_input, "abbas");

        // Pegar también sustituye.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('/')));
        app.on_event(Event::Paste("muse".into()));
        assert_eq!(app.search_input, "muse");
    }

    #[tokio::test]
    async fn search_typing_paste_and_stale_results() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('/')));
        assert!(matches!(app.popup, Popup::Search));
        for c in "que".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        // 'q' dentro del buscador escribe, no sale de la aplicación.
        assert!(!app.should_quit);
        app.on_event(Event::Paste("en\n".into()));
        assert_eq!(app.search_input, "queen");
        app.on_event(key(KeyCode::Tab));
        assert_eq!(app.search_kind, SearchKind::Albums);

        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.popup, Popup::None));
        assert!(matches!(app.main, MainContent::Loading(_)));
        let current = app.request_id;

        // Una respuesta de una petición anterior se ignora.
        app.on_msg(Msg::Search {
            request: current - 1,
            title: "vieja".into(),
            kind: SearchKind::Albums,
            result: Ok(SearchResults::Contexts(vec![album("Viejo")])),
        });
        assert!(matches!(app.main, MainContent::Loading(_)));

        app.on_msg(Msg::Search {
            request: current,
            title: "Búsqueda: «queen» · Álbumes".into(),
            kind: SearchKind::Albums,
            result: Ok(SearchResults::Contexts(vec![album("A Night at the Opera")])),
        });
        match &app.main {
            MainContent::Contexts { title, items, .. } => {
                assert!(title.contains("queen"));
                assert_eq!(items[0].name, "A Night at the Opera");
            }
            other => panic!("se esperaban resultados, no {other:?}"),
        }
        assert_eq!(app.main_state.selected(), Some(0));
    }

    /// En el buscador, los atajos con Ctrl o Alt no escriben su letra; AltGr
    /// (Ctrl+Alt) y Mayúsculas sí escriben, y Ctrl+W borra una palabra.
    #[tokio::test]
    async fn search_ignores_shortcuts_but_accepts_altgr() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('/')));
        for c in "queen live".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_event(key_with(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.on_event(key_with(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.on_event(key_with(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(app.search_input, "queen live");
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        app.on_event(key_with(KeyCode::Char('@'), altgr));
        app.on_event(key_with(KeyCode::Char('Q'), KeyModifiers::SHIFT));
        assert_eq!(app.search_input, "queen live@Q");
        app.on_event(key_with(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.search_input, "queen ");
        app.on_event(key_with(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(app.search_input, "queen");
        assert!(matches!(app.popup, Popup::Search));
    }

    #[test]
    fn ctrl_w_deletes_the_last_word() {
        for (before, after) in [
            ("queen live", "queen "),
            ("queen live   ", "queen "),
            ("queen", ""),
            ("", ""),
            ("   ", ""),
            ("東京 事変", "東京 "),
            ("a\u{3000}b", "a\u{3000}"),
        ] {
            let mut text = before.to_string();
            delete_last_word(&mut text);
            assert_eq!(text, after, "desde «{before}»");
        }
    }

    /// Al pegar varias líneas o una fila con tabuladores, las palabras no se
    /// juntan.
    #[test]
    fn pasted_lines_and_tabs_become_spaces() {
        assert_eq!(
            pasted_text("Bohemian Rhapsody\nQueen"),
            "Bohemian Rhapsody Queen"
        );
        assert_eq!(pasted_text("a\tb\r\nc\n"), "a b c");
        assert_eq!(pasted_text("en\n"), "en");
        assert_eq!(pasted_text("\n\t\u{0}"), "");
    }

    fn device(name: &str, active: bool) -> DeviceEntry {
        DeviceEntry {
            id: Some(name.into()),
            name: name.into(),
            kind: "Smartphone".into(),
            active,
        }
    }

    /// Si la ventana se cerró y se volvió a abrir, la respuesta de la
    /// petición anterior no la cierra con un error falso ni cambia la lista
    /// (y la selección) mientras se elige.
    #[tokio::test]
    async fn stale_device_answers_are_ignored() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('d')));
        let old = app.devices_request;
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('d')));

        app.on_msg(Msg::Devices {
            request: old,
            result: Err(anyhow::anyhow!("tiempo agotado")),
        });
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        assert_eq!(status(&app), None);

        app.on_msg(Msg::Devices {
            request: app.devices_request,
            result: Ok(vec![device("Móvil", false), device("Altavoz", true)]),
        });
        app.on_event(key(KeyCode::Char('k')));
        app.on_msg(Msg::Devices {
            request: old,
            result: Ok(vec![device("Altavoz", true)]),
        });
        match &app.popup {
            Popup::Devices {
                devices: Some(list),
                state,
            } => {
                assert_eq!(list.len(), 2);
                assert_eq!(state.selected(), Some(0), "se mantiene el móvil elegido");
            }
            _ => panic!("la ventana debe seguir abierta con su lista"),
        }
    }

    /// Un pegado enorme no hace crecer la búsqueda sin límite; lo que cabe
    /// se conserva entero, también con caracteres de varios bytes.
    #[tokio::test]
    async fn search_text_has_a_cap() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('/')));
        app.on_event(Event::Paste("ñ".repeat(1_000_000)));
        assert_eq!(app.search_input.chars().count(), MAX_SEARCH_CHARS);
        app.on_event(key(KeyCode::Char('x')));
        assert_eq!(app.search_input.chars().count(), MAX_SEARCH_CHARS);
        app.on_event(key(KeyCode::Backspace));
        app.on_event(key(KeyCode::Char('x')));
        assert!(app.search_input.ends_with('x'));
    }

    /// Reproducción de prueba que espera dispositivo.
    fn waiting_play() -> Action {
        Action::PlayTracks {
            tracks: Vec::new(),
            start: 2,
            device_id: None,
        }
    }

    /// Si Enter falla por no haber dispositivo activo, se abre la ventana de
    /// dispositivos y la reproducción se repite en el que se elija.
    #[tokio::test]
    async fn play_without_device_asks_for_one_and_replays_there() {
        let (mut app, _rx) = test_app();
        app.on_msg(Msg::ActionDone {
            done: None,
            refresh_queue: true,
            replay: Some(Box::new(waiting_play())),
            result: Err(anyhow::Error::new(ApiError::player(404))),
        });
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        assert!(app.pending_play.is_some());
        assert!(status(&app).is_some_and(|s| s.contains("elige dónde")));

        // Al elegir, la reproducción se consume (se repite una sola vez).
        app.on_msg(Msg::Devices {
            request: app.devices_request,
            result: Ok(vec![device("altavoz", false)]),
        });
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.popup, Popup::None));
        assert!(app.pending_play.is_none(), "se repite una sola vez");

        // El dispositivo elegido recibe la reproducción, no una transferencia.
        assert!(matches!(
            device_choice(Some(waiting_play()), "altavoz".into()),
            Action::PlayTracks { start: 2, device_id: Some(id), .. } if id == "altavoz"
        ));
        assert!(matches!(
            device_choice(None, "altavoz".into()),
            Action::Transfer { device_id } if device_id == "altavoz"
        ));
    }

    /// Cerrar la ventana descarta la reproducción pendiente: abrirla después
    /// con 'd' solo transfiere. Con otra ventana abierta no se interrumpe:
    /// solo se avisa.
    #[tokio::test]
    async fn pending_play_is_dropped_or_not_intrusive() {
        let (mut app, _rx) = test_app();
        let no_device = || Msg::ActionDone {
            done: None,
            refresh_queue: true,
            replay: Some(Box::new(waiting_play())),
            result: Err(anyhow::Error::new(ApiError::player(404))),
        };
        app.on_msg(no_device());
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(app.popup, Popup::None));
        assert!(app.pending_play.is_none());

        // También si se cierra sola (no se pudieron listar los dispositivos).
        app.on_msg(no_device());
        app.on_msg(Msg::Devices {
            request: app.devices_request,
            result: Err(anyhow::anyhow!("sin red")),
        });
        assert!(matches!(app.popup, Popup::None));
        assert!(app.pending_play.is_none());

        app.on_event(key(KeyCode::Char('?')));
        app.on_msg(no_device());
        assert!(matches!(app.popup, Popup::Help));
        assert!(app.pending_play.is_none());
        assert!(status(&app).is_some_and(|s| s.contains("dispositivo")));

        // Otros errores de una reproducción no abren la ventana.
        app.on_event(key(KeyCode::Esc));
        app.on_msg(Msg::ActionDone {
            done: None,
            refresh_queue: true,
            replay: Some(Box::new(waiting_play())),
            result: Err(anyhow::Error::new(ApiError::player(403))),
        });
        assert!(matches!(app.popup, Popup::None));
    }

    /// Abre una ventana y la cierra por uno de sus caminos.
    fn open_and_close(app: &mut App, path: &str) {
        match path {
            "ayuda" => {
                app.on_event(key(KeyCode::Char('?')));
                app.on_event(key(KeyCode::Char('x')));
            }
            "dispositivos: Esc" => {
                app.on_event(key(KeyCode::Char('d')));
                app.on_event(key(KeyCode::Esc));
            }
            "dispositivos: error" => {
                app.on_event(key(KeyCode::Char('d')));
                app.on_msg(Msg::Devices {
                    request: app.devices_request,
                    result: Err(anyhow::anyhow!("sin red")),
                });
            }
            "dispositivos: elegir" => {
                app.on_event(key(KeyCode::Char('d')));
                app.on_msg(Msg::Devices {
                    request: app.devices_request,
                    result: Ok(vec![device("Móvil", true)]),
                });
                app.on_event(key(KeyCode::Enter));
            }
            "dispositivos: fallo" => {
                app.on_event(key(KeyCode::Char('d')));
                app.on_msg(crash(CrashOrigin::Other, "fallo simulado"));
            }
            "buscador" => {
                app.on_event(key(KeyCode::Char('/')));
                app.on_event(key(KeyCode::Esc));
            }
            other => panic!("camino desconocido: {other}"),
        }
    }

    /// Con Sixel o iTerm2, cerrar la ayuda o los dispositivos (por cualquier
    /// camino) pide repintar la pantalla entera una vez; con los demás
    /// protocolos, o al cerrar el buscador, no hace falta.
    #[tokio::test]
    async fn closing_popups_over_sixel_or_iterm2_asks_for_a_full_redraw() {
        let paths = [
            "ayuda",
            "dispositivos: Esc",
            "dispositivos: error",
            "dispositivos: elegir",
            "dispositivos: fallo",
            "buscador",
        ];
        for protocol in [
            ProtocolType::Halfblocks,
            ProtocolType::Kitty,
            ProtocolType::Sixel,
            ProtocolType::Iterm2,
        ] {
            for path in paths {
                let (mut app, _rx) = test_app();
                app.picker.set_protocol_type(protocol);
                open_and_close(&mut app, path);
                assert!(matches!(app.popup, Popup::None), "{path}: sigue abierta");
                let expected = path != "buscador"
                    && matches!(protocol, ProtocolType::Sixel | ProtocolType::Iterm2);
                assert_eq!(app.take_full_redraw(), expected, "{protocol:?}, {path}");
                assert!(!app.take_full_redraw(), "se pide una sola vez");
            }
        }
    }

    /// Enter mientras se buscan dispositivos (o sin ninguno) no cierra la
    /// ventana con un aviso engañoso, y 'd' vuelve a buscar.
    #[tokio::test]
    async fn devices_popup_waits_for_a_device_and_d_searches_again() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('d')));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        assert_eq!(status(&app), None);

        app.on_msg(Msg::Devices {
            request: app.devices_request,
            result: Ok(Vec::new()),
        });
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            app.popup,
            Popup::Devices {
                devices: Some(_),
                ..
            }
        ));
        assert_eq!(status(&app), None);

        app.on_event(key(KeyCode::Char('d')));
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(app.popup, Popup::None));
    }
}
