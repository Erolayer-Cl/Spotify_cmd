//! Reproducción: controles (play/pausa, saltos, volumen, aleatorio...) con
//! actualización optimista del estado local, agrupado de las teclas
//! mantenidas y sondeo periódico del estado de Spotify.

use std::time::{Duration, Instant};

use anyhow::Result;
use rspotify::model::RepeatState;

use super::{App, LibraryItem, Msg, Popup, SETTLE};
use crate::spotify::{self, Action, ApiError, Playback};

/// Salto de cada avance o retroceso.
const SEEK_STEP_MS: u64 = 10_000;
/// Sin red, el estado se vuelve a consultar cada 10 s en lugar de cada 3 s.
const OFFLINE_RETRY: Duration = Duration::from_secs(10);
/// Espera sin cambios antes de enviar un valor agrupado.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Agrupa cambios rápidos (una tecla mantenida) y entrega solo el último
/// valor cuando pasan [`DEBOUNCE`] sin cambios nuevos.
#[derive(Debug, Default)]
pub(super) struct Debounced<T> {
    pending: Option<(T, Instant)>,
}

impl<T> Debounced<T> {
    fn set(&mut self, value: T) {
        self.pending = Some((value, Instant::now()));
    }

    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Descarta el valor pendiente sin enviarlo.
    fn cancel(&mut self) {
        self.pending = None;
    }

    fn take_ready(&mut self) -> Option<T> {
        match &self.pending {
            Some((_, at)) if at.elapsed() >= DEBOUNCE => self.take(),
            _ => None,
        }
    }

    /// Entrega el valor pendiente ya, sin esperar a [`DEBOUNCE`].
    fn take(&mut self) -> Option<T> {
        self.pending.take().map(|(v, _)| v)
    }
}

/// Si el error de una acción es que no hay ningún dispositivo activo.
fn no_active_device(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(|api| api.status == Some(404))
}

/// Aviso cuando se pulsa un control sin nada sonando.
const NOTHING_PLAYING: &str =
    "No hay nada reproduciéndose. Elige una canción o un dispositivo ('d').";

/// Texto del modo de repetición.
pub fn repeat_label(state: RepeatState) -> &'static str {
    match state {
        RepeatState::Off => "no",
        RepeatState::Context => "lista",
        RepeatState::Track => "canción",
    }
}

impl App {
    // -----------------------------------------------------------------------
    // Sondeo del estado
    // -----------------------------------------------------------------------

    /// Trabajo periódico del bucle principal: envía los cambios agrupados,
    /// consulta el estado cuando toca y lanza la recarga programada.
    pub fn on_tick(&mut self) {
        self.flush_debounced();
        let now = Instant::now();
        if self.refresh_at.is_some_and(|at| now >= at) {
            // Si hay una consulta en curso (o hay que esperar), el refresco no
            // se pierde: se reintenta en el siguiente tick.
            if self.poll_playback() {
                self.refresh_at = None;
            }
        } else if self.refresh_at.is_none() && self.since_last_poll() >= self.poll_every {
            // Con un refresco pendiente no se consulta antes de tiempo:
            // Spotify aún no refleja la última acción.
            self.poll_playback();
        }
        // La recarga programada espera también si Spotify pidió calma.
        if self.list_reload_at.is_some_and(|at| now >= at) && !self.backing_off() {
            self.list_reload_at = None;
            self.reload_active();
        }
        // Al terminar la canción, se consulta enseguida la siguiente.
        if let Some(pb) = &self.playback
            && pb.is_playing
            && pb.duration_ms > 0
            && pb.current_progress_ms() >= pb.duration_ms
            && self.since_last_poll() > Duration::from_secs(1)
        {
            self.poll_playback();
        }
    }

    fn since_last_poll(&self) -> Duration {
        self.last_poll.map_or(Duration::MAX, |t| t.elapsed())
    }

    /// Consulta el estado de reproducción, salvo que ya haya una consulta en
    /// curso, una acción sin respuesta (Spotify aún no la refleja) o haya que
    /// esperar (429 o sin red). Devuelve si se lanzó.
    pub(super) fn poll_playback(&mut self) -> bool {
        if self.playback_inflight || self.actions_inflight > 0 || self.backing_off() {
            return false;
        }
        self.playback_inflight = true;
        self.last_poll = Some(Instant::now());
        self.poll_seq += 1;
        let seq = self.poll_seq;
        self.spawn(
            |sp| async move { spotify::playback(&sp).await },
            move |result| Msg::Playback { seq, result },
        );
        true
    }

    /// Respuesta de la consulta número `seq` del estado de reproducción.
    pub(super) fn on_playback(&mut self, seq: u64, result: Result<Option<Playback>>) {
        // Tras un fallo inesperado puede haber dos consultas en curso: la
        // marca solo se quita con la respuesta de la última.
        if seq == self.poll_seq {
            self.playback_inflight = false;
        }
        match result {
            Ok(pb) => {
                self.session_problem = None;
                // Una respuesta obsoleta desharía la última acción o pisaría
                // una más reciente: se ignora.
                if seq >= self.fresh_poll {
                    self.fresh_poll = seq + 1;
                    self.set_playback(pb);
                }
            }
            Err(e) => {
                self.note_rate_limit(&e);
                self.show_request_error("Error al consultar la reproducción", &e);
            }
        }
    }

    /// Aplica el estado recibido de Spotify, sin deshacer los cambios locales
    /// aún sin enviar. Con otra canción recarga la cola o las recientes que
    /// se vean y sigue su carátula.
    pub(super) fn set_playback(&mut self, mut pb: Option<Playback>) {
        // Mientras haya un cambio sin enviar, se conserva el valor local para
        // que la pantalla no "salte" hacia atrás.
        if let (Some(new), Some(old)) = (pb.as_mut(), self.playback.as_ref()) {
            // Un salto pendiente era para la canción anterior: no se envía ni
            // se lleva su posición a la nueva.
            if new.item_uri != old.item_uri {
                self.pending_seek.cancel();
            }
            if self.pending_seek.is_pending() {
                new.progress_ms = old.current_progress_ms();
                new.fetched_at = Instant::now();
            }
            if self.pending_volume.is_pending() {
                new.volume = old.volume;
            }
        }
        // Con otra canción cambian la cola y las recientes que se ven.
        let old_uri = self.playback.as_ref().and_then(|p| p.item_uri.as_deref());
        let new_uri = pb.as_ref().and_then(|p| p.item_uri.as_deref());
        if old_uri != new_uri
            && matches!(
                self.active_library,
                Some(LibraryItem::Queue | LibraryItem::Recent)
            )
        {
            self.schedule_list_reload();
        }
        self.follow_cover(pb.as_ref().and_then(|p| p.cover_url.clone()));
        self.playback = pb;
    }

    /// Hay que esperar antes de consultar (429 de Spotify o sin red).
    fn backing_off(&self) -> bool {
        self.backoff_until.is_some_and(|t| Instant::now() < t)
    }

    /// Programa la consulta del estado para dentro de `after`. Si ya había
    /// una pendiente se aplaza: tras varias acciones seguidas, se consulta
    /// cuando Spotify ya refleja la última.
    fn schedule_refresh(&mut self, after: Duration) {
        self.refresh_at = Some(Instant::now() + after);
    }

    /// Espacia las consultas periódicas cuando Spotify pide calma (429) o
    /// cuando no hay red, para no saturar ni gastar batería.
    fn note_rate_limit(&mut self, err: &anyhow::Error) {
        let Some(api) = err.downcast_ref::<ApiError>() else {
            return;
        };
        let wait = match api.status {
            Some(429) => api.retry_after.unwrap_or(Duration::from_secs(5)),
            None => OFFLINE_RETRY,
            Some(_) => return,
        };
        self.backoff_until = Some(Instant::now() + wait.min(Duration::from_secs(120)));
    }

    // -----------------------------------------------------------------------
    // Acciones
    // -----------------------------------------------------------------------

    /// Envía una acción a Spotify en segundo plano.
    pub(super) fn do_action(&mut self, action: Action) {
        // Las consultas del estado ya lanzadas no reflejan esta acción: su
        // respuesta desharía el cambio optimista (p. ej. la pausa recién
        // pulsada). Hasta que responda no se lanzan otras (`poll_playback`).
        self.fresh_poll = self.poll_seq + 1;
        self.actions_inflight = self.actions_inflight.saturating_add(1);
        // Un salto de posición aún sin enviar caería en la canción nueva.
        if action.changes_track() {
            self.pending_seek.cancel();
        }
        let done = action.done_message();
        let refresh_queue = action.changes_queue();
        // Una reproducción nueva sustituye a la que esperaba dispositivo.
        let replay = action.starts_playback().then(|| {
            self.pending_play = None;
            Box::new(action.clone())
        });
        // En la fila: salen a Spotify de una en una y en el orden de las teclas.
        self.spawn_in_order(
            |sp| async move { spotify::perform(&sp, action).await },
            move |result| Msg::ActionDone {
                done,
                refresh_queue,
                replay,
                result,
            },
        );
    }

    /// Respuesta de una acción: se avisa del resultado y se vuelve a
    /// consultar el estado cuando Spotify ya lo refleje.
    pub(super) fn on_action_done(
        &mut self,
        done: Option<&'static str>,
        refresh_queue: bool,
        replay: Option<Box<Action>>,
        result: Result<()>,
    ) {
        self.actions_inflight = self.actions_inflight.saturating_sub(1);
        // Una consulta lanzada mientras la acción viajaba (posible tras un
        // fallo inesperado, que reinicia la cuenta) tampoco la refleja.
        self.fresh_poll = self.poll_seq + 1;
        let succeeded = result.is_ok();
        match result {
            Ok(()) => {
                if let Some(text) = done {
                    self.set_status(text, false);
                }
                // Ya suena: lo que esperaba dispositivo quedó atrás.
                if replay.is_some() {
                    self.pending_play = None;
                }
            }
            Err(e) => {
                self.note_rate_limit(&e);
                match replay {
                    Some(play) if no_active_device(&e) => self.play_without_device(*play, &e),
                    _ => self.set_status(e.to_string(), true),
                }
            }
        }
        self.schedule_refresh(SETTLE);
        // La cola visible se recarga sin quitar el foco ni la selección.
        if succeeded && refresh_queue && self.active_library == Some(LibraryItem::Queue) {
            self.schedule_list_reload();
        }
    }

    /// La reproducción `play` falló (`err`) por no haber dispositivo activo.
    /// Con el reproductor local en marcha, suena en este equipo; si no, se
    /// pide elegir dónde (salvo que haya otra ventana abierta: solo se avisa).
    fn play_without_device(&mut self, play: Action, err: &anyhow::Error) {
        let local = self.local_device.as_ref().map(|d| d.id.clone());
        // Si ya se intentó en este equipo, no se repite: se pide elegir.
        let already_local = matches!(
            &play,
            Action::PlayContext { device_id: Some(id), .. } | Action::PlayTracks { device_id: Some(id), .. }
                if Some(id) == local.as_ref()
        );
        match local {
            Some(id) if !already_local => {
                if let Some(here) = play.on_device(id) {
                    self.set_status("Reproduciendo en este equipo", false);
                    self.do_action(here);
                }
            }
            _ if matches!(self.popup, Popup::None) => self.ask_device_for(play),
            _ => self.set_status(err.to_string(), true),
        }
    }

    /// La reproducción `play` falló por no haber dispositivo activo: se abre
    /// la ventana de dispositivos y se repetirá en el que se elija.
    fn ask_device_for(&mut self, play: Action) {
        self.open_devices();
        self.pending_play = Some(play);
        self.set_status(
            "No hay ningún dispositivo activo: elige dónde reproducir",
            true,
        );
    }

    // -----------------------------------------------------------------------
    // Controles (con actualización optimista del estado local)
    // -----------------------------------------------------------------------

    /// El estado de reproducción para un control, o un aviso si no suena nada
    /// (así ninguna tecla "no hace nada" en silencio).
    fn playback_or_warn(&mut self) -> Option<&mut Playback> {
        if self.playback.is_none() {
            self.set_status(NOTHING_PLAYING, true);
            return None;
        }
        self.playback.as_mut()
    }

    pub(super) fn toggle_play(&mut self) {
        let Some(pb) = self.playback_or_warn() else {
            return;
        };
        let playing = pb.is_playing;
        pb.progress_ms = pb.current_progress_ms();
        pb.fetched_at = Instant::now();
        pb.is_playing = !playing;
        self.do_action(Action::TogglePlay { playing });
    }

    /// Siguiente o anterior canción, con su aviso.
    pub(super) fn skip(&mut self, action: Action, label: &str) {
        self.set_status(label, false);
        self.do_action(action);
    }

    pub(super) fn seek(&mut self, forward: bool) {
        let Some(pb) = self.playback_or_warn() else {
            return;
        };
        let current = pb.current_progress_ms();
        let target = if forward {
            (current + SEEK_STEP_MS).min(pb.duration_ms.saturating_sub(1000))
        } else {
            current.saturating_sub(SEEK_STEP_MS)
        };
        pb.progress_ms = target;
        pb.fetched_at = Instant::now();
        // Se envía al soltar la tecla: mantenerla pulsada no satura la API.
        self.pending_seek.set(target);
    }

    pub(super) fn change_volume(&mut self, delta: i32) {
        let Some(pb) = self.playback_or_warn() else {
            return;
        };
        let Some(vol) = pb.volume else {
            self.set_status("Este dispositivo no permite cambiar el volumen", true);
            return;
        };
        let new = (vol as i32 + delta).clamp(0, 100) as u32;
        pb.volume = Some(new);
        self.pending_volume.set(new as u8);
    }

    /// Envía los cambios agrupados de volumen y posición cuando se dejan de pulsar.
    fn flush_debounced(&mut self) {
        if let Some(position_ms) = self.pending_seek.take_ready() {
            self.do_action(Action::Seek { position_ms });
        }
        if let Some(volume) = self.pending_volume.take_ready() {
            self.do_action(Action::Volume(volume));
        }
    }

    /// Al salir: envía ya el avance y el volumen pendientes (sin esperar a
    /// [`DEBOUNCE`]; si no, la última tecla se perdería) y devuelve si quedan
    /// acciones por responder. Si quedan, lo avisa en la barra de estado.
    pub fn prepare_exit(&mut self) -> bool {
        if let Some(position_ms) = self.pending_seek.take() {
            self.do_action(Action::Seek { position_ms });
        }
        if let Some(volume) = self.pending_volume.take() {
            self.do_action(Action::Volume(volume));
        }
        let waiting = self.has_actions_inflight();
        if waiting {
            self.set_status("Enviando los últimos cambios a Spotify…", false);
        }
        waiting
    }

    /// Si alguna acción enviada aún no ha respondido.
    pub fn has_actions_inflight(&self) -> bool {
        self.actions_inflight > 0
    }

    pub(super) fn toggle_shuffle(&mut self) {
        let Some(pb) = self.playback_or_warn() else {
            return;
        };
        pb.shuffle = !pb.shuffle;
        let state = pb.shuffle;
        self.set_status(
            if state {
                "Aleatorio activado"
            } else {
                "Aleatorio desactivado"
            },
            false,
        );
        self.do_action(Action::Shuffle(state));
    }

    pub(super) fn cycle_repeat(&mut self) {
        let Some(pb) = self.playback_or_warn() else {
            return;
        };
        pb.repeat = match pb.repeat {
            RepeatState::Off => RepeatState::Context,
            RepeatState::Context => RepeatState::Track,
            RepeatState::Track => RepeatState::Off,
        };
        let state = pb.repeat;
        self.set_status(format!("Repetir: {}", repeat_label(state)), false);
        self.do_action(Action::Repeat(state));
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::app::CrashOrigin;
    use crate::app::tests::{ID_A, ID_B, crash, key, now_playing, status, test_app};

    /// Respuesta correcta de una acción.
    fn action_done() -> Msg {
        Msg::ActionDone {
            done: None,
            refresh_queue: false,
            replay: None,
            result: Ok(()),
        }
    }

    /// Estado de reproducción con la canción `title` (de id `id`) sonando.
    fn playing_track(title: &str, id: &str) -> Playback {
        Playback {
            item_uri: Some(format!("spotify:track:{id}")),
            ..now_playing(title, true)
        }
    }

    /// Aplica el estado `pb` como respuesta de una consulta nueva.
    fn poll_answer(app: &mut App, pb: Playback) {
        assert!(app.poll_playback());
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(Some(pb)),
        });
    }

    fn progress(app: &App) -> u64 {
        app.playback.as_ref().map_or(0, |pb| pb.progress_ms)
    }

    fn is_playing(app: &App) -> bool {
        app.playback.as_ref().is_some_and(|pb| pb.is_playing)
    }

    /// Sin nada sonando, ningún control "no hace nada" en silencio: avisa y
    /// no envía ninguna petición.
    #[tokio::test]
    async fn controls_without_playback_warn_and_send_nothing() {
        let (mut app, _rx) = test_app();
        for code in [
            KeyCode::Char(' '),
            KeyCode::Char('s'),
            KeyCode::Char('r'),
            KeyCode::Char('+'),
            KeyCode::Char('-'),
            KeyCode::Left,
            KeyCode::Right,
        ] {
            app.status = None;
            app.on_event(key(code));
            assert_eq!(status(&app), Some(NOTHING_PLAYING), "{code:?}");
        }
        assert_eq!(app.actions_inflight, 0, "no se envió ninguna acción");
        assert!(!app.pending_seek.is_pending() && !app.pending_volume.is_pending());
    }

    /// Con la cola a la vista, cambiar el aleatorio la recarga (Spotify la
    /// reordena).
    #[tokio::test]
    async fn shuffle_reloads_the_visible_queue() {
        let (mut app, _rx) = test_app();
        app.active_library = Some(LibraryItem::Queue);
        app.playback = Some(now_playing("A", true));
        app.on_event(key(KeyCode::Char('s')));
        app.on_msg(Msg::ActionDone {
            done: None,
            refresh_queue: true,
            replay: None,
            result: Ok(()),
        });
        assert!(app.list_reload_at.is_some(), "la cola debe recargarse");
    }

    #[tokio::test]
    async fn rate_limit_pauses_polling() {
        let (mut app, _rx) = test_app();
        let err = anyhow::Error::new(ApiError::rate_limited(Some(Duration::from_secs(30))));
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Err(err),
        });
        assert!(
            app.backoff_until
                .is_some_and(|t| t > Instant::now() + Duration::from_secs(25))
        );
        app.poll_playback();
        assert!(
            !app.playback_inflight,
            "no debe consultar durante la espera"
        );
    }

    /// El refresco tras una acción no se pierde si coincide con una consulta
    /// en curso: se hace en cuanto esta termina.
    #[tokio::test]
    async fn refresh_waits_for_the_poll_in_flight() {
        let (mut app, _rx) = test_app();
        assert!(app.poll_playback());
        app.schedule_refresh(Duration::ZERO);
        app.on_tick();
        assert!(app.refresh_at.is_some(), "el refresco debe esperar");
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(None),
        });
        assert!(!app.playback_inflight);
        app.on_tick();
        assert!(app.playback_inflight, "se consulta en cuanto se puede");
        assert!(app.refresh_at.is_none());
    }

    /// Una consulta enviada antes de pausar no vuelve a mostrar «reproduciendo».
    #[tokio::test]
    async fn poll_sent_before_a_pause_does_not_undo_it() {
        let (mut app, _rx) = test_app();
        app.playback = Some(now_playing("A", true));
        assert!(app.poll_playback());
        let before_pause = app.poll_seq;
        app.on_event(key(KeyCode::Char(' ')));
        app.on_msg(Msg::Playback {
            seq: before_pause,
            result: Ok(Some(now_playing("A", true))),
        });
        assert!(app.playback.as_ref().is_some_and(|pb| !pb.is_playing));
        assert!(!app.playback_inflight, "se puede volver a consultar");
        // Las consultas posteriores a la respuesta de la acción sí se aplican.
        app.on_msg(action_done());
        assert!(app.poll_playback());
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(Some(now_playing("B", true))),
        });
        assert_eq!(app.playback.as_ref().map(|pb| pb.title.as_str()), Some("B"));
    }

    /// Si tras un fallo inesperado hay dos consultas en vuelo, la respuesta
    /// vieja nunca pisa a la nueva, llegue en el orden que llegue.
    #[tokio::test]
    async fn older_poll_never_overwrites_a_newer_one() {
        let (mut app, _rx) = test_app();
        let title = |app: &App| app.playback.as_ref().map(|pb| pb.title.clone());

        // La vieja llega primero: se aplica, pero sigue esperando a la nueva.
        assert!(app.poll_playback());
        let old = app.poll_seq;
        app.on_msg(crash(CrashOrigin::Other, "boom"));
        assert!(app.poll_playback());
        let new = app.poll_seq;
        app.on_msg(Msg::Playback {
            seq: old,
            result: Ok(Some(now_playing("Vieja", true))),
        });
        assert!(app.playback_inflight);
        app.on_msg(Msg::Playback {
            seq: new,
            result: Ok(Some(now_playing("Nueva", true))),
        });
        assert_eq!(title(&app).as_deref(), Some("Nueva"));
        assert!(!app.playback_inflight);

        // La nueva llega primero: la vieja se ignora.
        assert!(app.poll_playback());
        let old = app.poll_seq;
        app.on_msg(crash(CrashOrigin::Other, "boom"));
        assert!(app.poll_playback());
        app.on_msg(Msg::Playback {
            seq: app.poll_seq,
            result: Ok(Some(now_playing("Nueva 2", true))),
        });
        app.on_msg(Msg::Playback {
            seq: old,
            result: Ok(Some(now_playing("Vieja 2", true))),
        });
        assert_eq!(title(&app).as_deref(), Some("Nueva 2"));
        assert!(!app.playback_inflight);
    }

    /// Mientras la acción viaja, y hasta que Spotify ya la refleja, no se
    /// consulta el estado: la respuesta desharía el cambio optimista (la
    /// pausa volvería a verse como «reproduciendo»).
    #[tokio::test]
    async fn no_poll_until_the_action_settles() {
        let (mut app, _rx) = test_app();
        app.playback = Some(now_playing("A", true));
        app.on_event(key(KeyCode::Char(' ')));
        // Ya toca la consulta periódica (nunca se ha consultado).
        app.on_tick();
        assert!(
            !app.playback_inflight,
            "con la pausa en vuelo no se consulta"
        );
        app.on_msg(action_done());
        app.on_tick();
        assert!(!app.playback_inflight, "ni antes de que Spotify la refleje");
        app.refresh_at = Some(Instant::now());
        app.on_tick();
        assert!(app.playback_inflight, "el refresco es la primera consulta");
        assert!(app.refresh_at.is_none());
    }

    /// Con varias acciones seguidas, el refresco de una anterior no se
    /// adelanta a que Spotify refleje la última.
    #[tokio::test]
    async fn refresh_waits_for_the_last_action() {
        let (mut app, _rx) = test_app();
        app.playback = Some(now_playing("A", true));
        app.on_event(key(KeyCode::Char('s')));
        app.on_msg(action_done());
        // Pasa el tiempo: el refresco de la primera ya toca...
        app.refresh_at = Some(Instant::now());
        // ...pero llega otra acción y responde.
        app.on_event(key(KeyCode::Char('r')));
        app.on_msg(action_done());
        assert!(
            app.refresh_at
                .is_some_and(|at| at > Instant::now() + SETTLE / 2)
        );
        app.on_tick();
        assert!(!app.playback_inflight);
    }

    /// Si tras un fallo inesperado se llegó a consultar con una acción en
    /// vuelo, esa respuesta no se aplica una vez que la acción respondió.
    #[tokio::test]
    async fn poll_sent_while_an_action_flew_is_stale() {
        let (mut app, _rx) = test_app();
        app.playback = Some(now_playing("A", true));
        app.on_event(key(KeyCode::Char(' ')));
        app.on_msg(crash(CrashOrigin::Other, "boom"));
        assert!(
            app.poll_playback(),
            "un fallo no deja las consultas bloqueadas"
        );
        let during = app.poll_seq;
        app.on_msg(action_done());
        app.on_msg(Msg::Playback {
            seq: during,
            result: Ok(Some(now_playing("A", true))),
        });
        assert!(app.playback.is_some() && !is_playing(&app));
    }

    /// Al salir, el volumen y el avance pendientes se envían ya, sin esperar
    /// a que pase el agrupado; sin nada pendiente no hay que esperar.
    #[tokio::test]
    async fn exit_sends_pending_changes_right_away() {
        let (mut app, _rx) = test_app();
        assert!(!app.prepare_exit(), "sin nada pendiente se sale ya");
        app.playback = Some(now_playing("A", true));
        app.on_event(key(KeyCode::Char('+')));
        app.on_event(key(KeyCode::Right));
        assert!(app.pending_volume.is_pending() && app.pending_seek.is_pending());
        assert!(app.prepare_exit());
        assert!(!app.pending_volume.is_pending() && !app.pending_seek.is_pending());
        assert_eq!(app.actions_inflight, 2);
        for msg in crate::app::tests::responses(&mut app).await {
            app.on_msg(msg);
        }
        assert!(!app.has_actions_inflight(), "las dos responden");
    }

    /// Un salto de posición aún sin enviar no cae en la canción siguiente
    /// (o anterior): se descarta al cambiar de canción.
    #[tokio::test]
    async fn changing_track_drops_the_pending_seek() {
        for code in [KeyCode::Char('n'), KeyCode::Char('p')] {
            let (mut app, _rx) = test_app();
            app.playback = Some(now_playing("A", true));
            app.on_event(key(KeyCode::Right));
            assert!(app.pending_seek.is_pending());
            app.on_event(key(code));
            assert!(!app.pending_seek.is_pending(), "con {code:?}");
        }
    }

    /// Si empieza otra canción con un salto pendiente (terminó la anterior),
    /// el salto se descarta y se ve la posición real de la nueva. En la misma
    /// canción se conserva la posición local hasta enviarlo.
    #[tokio::test]
    async fn new_song_drops_the_pending_seek() {
        let (mut app, _rx) = test_app();
        app.playback = Some(playing_track("A", ID_A));
        app.on_event(key(KeyCode::Right));

        poll_answer(&mut app, playing_track("A", ID_A));
        assert!(app.pending_seek.is_pending());
        assert!(progress(&app) >= SEEK_STEP_MS);

        poll_answer(&mut app, playing_track("B", ID_B));
        assert!(!app.pending_seek.is_pending());
        assert!(progress(&app) < SEEK_STEP_MS);
    }

    #[test]
    fn debounced_delivers_only_the_last_value() {
        let mut d = Debounced::default();
        d.set(10);
        d.set(15);
        d.set(20);
        assert_eq!(d.take_ready(), None, "aún no pasó el tiempo de espera");
        std::thread::sleep(DEBOUNCE + Duration::from_millis(20));
        assert_eq!(d.take_ready(), Some(20));
        assert_eq!(d.take_ready(), None);
        assert!(!d.is_pending());
    }
}
