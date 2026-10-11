//! Ventanas emergentes: el buscador, donde las teclas escriben texto en vez
//! de ejecutar órdenes, y la lista de dispositivos donde reproducir.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use super::{App, Msg, Popup};
use crate::keymap::{Command, step_index};
use crate::spotify::{self, Action, DeviceEntry};

impl App {
    // -----------------------------------------------------------------------
    // Buscador
    // -----------------------------------------------------------------------

    /// En el buscador las teclas escriben texto en vez de ejecutar órdenes.
    pub(super) fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.popup = Popup::None,
            KeyCode::Enter => {
                self.popup = Popup::None;
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
            KeyCode::Char(c) if is_text_input(key.modifiers) => self.search_input.push(c),
            _ => {}
        }
    }

    /// Texto pegado en el buscador.
    pub(super) fn on_search_paste(&mut self, text: &str) {
        self.search_input.push_str(&pasted_text(text));
    }

    // -----------------------------------------------------------------------
    // Dispositivos
    // -----------------------------------------------------------------------

    /// Abre la ventana de dispositivos y los pide a Spotify.
    pub(super) fn open_devices(&mut self) {
        self.popup = Popup::Devices {
            devices: None,
            state: ListState::default(),
        };
        self.spawn(|sp| async move { Msg::Devices(spotify::devices(&sp).await) });
    }

    /// Orden recibida con la ventana de dispositivos abierta.
    pub(super) fn on_devices_command(&mut self, command: Command) {
        let Popup::Devices { devices, state } = &mut self.popup else {
            return;
        };
        match command {
            Command::Back | Command::Quit => self.popup = Popup::None,
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
                self.popup = Popup::None;
                match device {
                    Some(device_id) => self.do_action(Action::Transfer { device_id }),
                    None => self.set_status("Ese dispositivo no se puede controlar", true),
                }
            }
            _ => {}
        }
    }

    /// Respuesta de la lista de dispositivos (si la ventana sigue abierta).
    pub(super) fn on_devices(&mut self, result: Result<Vec<DeviceEntry>>) {
        if let Popup::Devices { devices, state } = &mut self.popup {
            match result {
                Ok(list) => {
                    let active = list.iter().position(|d| d.active).unwrap_or(0);
                    state.select((!list.is_empty()).then_some(active));
                    *devices = Some(list);
                }
                Err(e) => {
                    self.popup = Popup::None;
                    self.set_status(format!("No se pudieron listar los dispositivos: {e}"), true);
                }
            }
        }
    }
}

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
    use crate::app::MainContent;
    use crate::app::tests::{album, key, status, test_app};
    use crate::spotify::{SearchKind, SearchResults};

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
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

    /// Enter mientras se buscan dispositivos (o sin ninguno) no cierra la
    /// ventana con un aviso engañoso, y 'd' vuelve a buscar.
    #[tokio::test]
    async fn devices_popup_waits_for_a_device_and_d_searches_again() {
        let (mut app, _rx) = test_app();
        app.on_event(key(KeyCode::Char('d')));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.popup, Popup::Devices { devices: None, .. }));
        assert_eq!(status(&app), None);

        app.on_msg(Msg::Devices(Ok(Vec::new())));
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
