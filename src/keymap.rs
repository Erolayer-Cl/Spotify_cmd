//! Teclas → órdenes (patrón *Command*).
//!
//! Separar "qué tecla se pulsó" de "qué hace la aplicación" permite cambiar
//! un atajo en un único sitio y probar el mapa sin dibujar nada.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Todo lo que el usuario puede pedir con el teclado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    Back,
    TogglePlay,
    Next,
    Previous,
    SeekForward,
    SeekBackward,
    VolumeUp,
    VolumeDown,
    ToggleShuffle,
    CycleRepeat,
    OpenSearch,
    OpenDevices,
    ToggleCover,
    ShowHelp,
    Refresh,
    SwitchFocus,
    QueueSelected,
    OpenSelected,
    /// Mover la selección `n` posiciones (negativo = hacia arriba).
    Move(i32),
    /// Ir al principio o al final de la lista.
    Jump(Edge),
    Activate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Start,
    End,
}

const PAGE: i32 = 10;

/// Traduce una tecla a una orden. `None` = la tecla no hace nada.
pub fn command_for(key: KeyEvent) -> Option<Command> {
    use Command::*;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(Quit),
            KeyCode::Char('r') => Some(Refresh),
            _ => None,
        };
    }
    let command = match key.code {
        KeyCode::Char('q') => Quit,
        KeyCode::Esc => Back,
        KeyCode::Char(' ') => TogglePlay,
        KeyCode::Char('n') => Next,
        KeyCode::Char('p') => Previous,
        KeyCode::Right => SeekForward,
        KeyCode::Left => SeekBackward,
        KeyCode::Char('+') | KeyCode::Char('=') => VolumeUp,
        KeyCode::Char('-') => VolumeDown,
        KeyCode::Char('s') => ToggleShuffle,
        KeyCode::Char('r') => CycleRepeat,
        KeyCode::Char('/') => OpenSearch,
        KeyCode::Char('d') => OpenDevices,
        KeyCode::Char('c') => ToggleCover,
        KeyCode::Char('?') | KeyCode::F(1) => ShowHelp,
        KeyCode::Char('R') | KeyCode::F(5) => Refresh,
        KeyCode::Tab | KeyCode::BackTab => SwitchFocus,
        KeyCode::Char('a') => QueueSelected,
        KeyCode::Char('o') => OpenSelected,
        KeyCode::Up | KeyCode::Char('k') => Move(-1),
        KeyCode::Down | KeyCode::Char('j') => Move(1),
        KeyCode::PageUp => Move(-PAGE),
        KeyCode::PageDown => Move(PAGE),
        KeyCode::Home | KeyCode::Char('g') => Jump(Edge::Start),
        KeyCode::End | KeyCode::Char('G') => Jump(Edge::End),
        KeyCode::Enter => Activate,
        _ => return None,
    };
    Some(command)
}

/// Nueva posición de una selección dentro de una lista de `len` elementos.
/// Función pura, compartida por la biblioteca, la lista principal y los popups.
pub fn step_index(current: Option<usize>, len: usize, command: Command) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len as i64 - 1;
    let cur = current.unwrap_or(0) as i64;
    let next = match command {
        Command::Move(delta) => cur + delta as i64,
        Command::Jump(Edge::Start) => 0,
        Command::Jump(Edge::End) => last,
        _ => cur,
    };
    Some(next.clamp(0, last) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keys_map_to_commands() {
        assert_eq!(
            command_for(press(KeyCode::Char(' '))),
            Some(Command::TogglePlay)
        );
        assert_eq!(command_for(press(KeyCode::Char('n'))), Some(Command::Next));
        assert_eq!(
            command_for(press(KeyCode::PageDown)),
            Some(Command::Move(PAGE))
        );
        assert_eq!(
            command_for(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Command::Quit)
        );
        assert_eq!(command_for(press(KeyCode::Char('x'))), None);
    }

    #[test]
    fn step_index_stays_inside_the_list() {
        assert_eq!(step_index(Some(0), 5, Command::Move(-1)), Some(0));
        assert_eq!(step_index(Some(3), 5, Command::Move(10)), Some(4));
        assert_eq!(step_index(None, 5, Command::Move(1)), Some(1));
        assert_eq!(step_index(Some(2), 5, Command::Jump(Edge::End)), Some(4));
        assert_eq!(step_index(Some(2), 5, Command::Jump(Edge::Start)), Some(0));
        assert_eq!(step_index(Some(2), 0, Command::Move(1)), None);
    }
}
