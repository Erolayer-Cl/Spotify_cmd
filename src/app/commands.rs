//! Teclado: cada tecla se convierte en una orden (ver [`crate::keymap`]) y
//! se reparte según la ventana abierta. Aquí se ejecutan las órdenes de la
//! pantalla principal; las de las ventanas emergentes, en `popups`.

use crossterm::event::{Event, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Focus, Popup};
use crate::keymap::{Command, command_for, step_index};
use crate::spotify::Action;

/// Paso del volumen con cada pulsación.
const VOLUME_STEP: i32 = 5;

impl App {
    /// Atiende un evento de la terminal (tecla o texto pegado).
    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Paste(text) if matches!(self.popup, Popup::Search) => {
                self.on_search_paste(&text);
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let command = command_for(key);
        if command == Some(Command::Quit) && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        match self.popup {
            Popup::Search => self.on_search_key(key),
            Popup::Help => self.popup = Popup::None,
            Popup::Devices { .. } => {
                if let Some(command) = command {
                    self.on_devices_command(command);
                }
            }
            Popup::None => {
                if let Some(command) = command {
                    self.run(command);
                }
            }
        }
    }

    /// Ejecuta una orden de la pantalla principal.
    fn run(&mut self, command: Command) {
        use Command::*;
        match command {
            // Con la carátula grande la lista no se ve: no se actúa a ciegas
            // sobre ella (la carátula se cierra con 'c').
            Activate | QueueSelected | OpenSelected | Move(_) | Jump(_) if self.list_hidden() => {}
            Quit => self.should_quit = true,
            Back => {}
            TogglePlay => self.toggle_play(),
            Next => self.skip(Action::Next, "Siguiente ⏭"),
            Previous => self.skip(Action::Previous, "Anterior ⏮"),
            SeekForward => self.seek(true),
            SeekBackward => self.seek(false),
            VolumeUp => self.change_volume(VOLUME_STEP),
            VolumeDown => self.change_volume(-VOLUME_STEP),
            ToggleShuffle => self.toggle_shuffle(),
            CycleRepeat => self.cycle_repeat(),
            OpenSearch => self.popup = Popup::Search,
            OpenDevices => self.open_devices(),
            ToggleCover => self.big_cover = !self.big_cover,
            ShowHelp => self.popup = Popup::Help,
            Refresh => self.refresh(),
            SwitchFocus => self.switch_focus(),
            QueueSelected => self.queue_selected(),
            OpenSelected => self.open_selected(),
            Move(_) | Jump(_) => self.move_selection(command),
            Activate => self.activate(),
        }
    }

    /// Vuelve a pedir las playlists, el estado y la lista que se ve.
    fn refresh(&mut self) {
        self.load_playlists();
        self.poll_playback();
        // También la lista que se ve, salvo que ya se esté cargando.
        if !self.list_loading() {
            self.reload_active();
        }
        self.set_status("Actualizando…", false);
    }

    /// Pasa el foco de la biblioteca a la lista principal y viceversa.
    fn switch_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Library => Focus::Main,
            Focus::Main => Focus::Library,
        }
    }

    /// Enter: abre la entrada de la biblioteca o reproduce la fila elegida.
    fn activate(&mut self) {
        match self.focus {
            Focus::Library => {
                if let Some(item) = self
                    .library_state
                    .selected()
                    .and_then(|i| self.library_item(i))
                {
                    self.open_library(item);
                }
            }
            Focus::Main => self.activate_main(),
        }
    }

    /// Mueve el cursor del panel que tiene el foco.
    fn move_selection(&mut self, command: Command) {
        match self.focus {
            Focus::Library => {
                let len = self.library_len();
                let next = step_index(self.library_state.selected(), len, command);
                self.library_state.select(next);
            }
            Focus::Main => {
                let len = self.main_len();
                let next = step_index(self.main_state.selected(), len, command);
                self.main_state.select(next);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::app::tests::{ID_A, ID_B, key, status, test_app, track};
    use crate::app::{MainContent, TrackSource};

    /// La lista que tapa la carátula grande no se reproduce ni se mueve a
    /// ciegas; al cerrar la carátula vuelve a responder.
    #[tokio::test]
    async fn hidden_list_ignores_play_queue_and_moves() {
        let (mut app, _rx) = test_app();
        app.main = MainContent::Tracks {
            title: "Lista".into(),
            source: TrackSource::Uris,
            tracks: vec![track("A", ID_A, None), track("B", ID_B, None)],
        };
        app.main_state.select(Some(0));
        app.focus = Focus::Main;
        app.big_cover = true;
        for code in [KeyCode::Char('j'), KeyCode::Enter, KeyCode::Char('a')] {
            app.on_event(key(code));
        }
        assert_eq!(app.main_state.selected(), Some(0));
        assert_eq!(status(&app), None);
        assert_eq!(app.fresh_poll, 0, "no se envió ninguna acción");
        app.on_event(key(KeyCode::Char('c')));
        app.on_event(key(KeyCode::Char('j')));
        assert_eq!(app.main_state.selected(), Some(1));
    }
}
