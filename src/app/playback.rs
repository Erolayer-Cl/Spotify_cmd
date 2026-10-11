//! Reproducción: controles (play/pausa, saltos, volumen, aleatorio...) con
//! actualización optimista del estado local, agrupado de las teclas
//! mantenidas y sondeo periódico del estado de Spotify.

use std::time::{Duration, Instant};

use anyhow::Result;
use rspotify::model::RepeatState;

use super::{App, LibraryItem, Msg, SETTLE};
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

    fn take_ready(&mut self) -> Option<T> {
        match &self.pending {
            Some((_, at)) if at.elapsed() >= DEBOUNCE => self.pending.take().map(|(v, _)| v),
            _ => None,
        }
    }
}

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
        } else if self.since_last_poll() >= self.poll_every {
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
    /// curso o haya que esperar (429 o sin red). Devuelve si se lanzó.
    pub(super) fn poll_playback(&mut self) -> bool {
        if self.playback_inflight || self.backing_off() {
            return false;
        }
        self.playback_inflight = true;
        self.last_poll = Some(Instant::now());
        self.poll_seq += 1;
        let seq = self.poll_seq;
        self.spawn(move |sp| async move {
            Msg::Playback {
                seq,
                result: spotify::playback(&sp).await,
            }
        });
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
                self.set_status(format!("Error al consultar la reproducción: {e}"), true);
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

    fn schedule_refresh(&mut self, after: Duration) {
        let at = Instant::now() + after;
        self.refresh_at = Some(self.refresh_at.map_or(at, |r| r.min(at)));
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
        // respuesta desharía el cambio optimista (p. ej. la pausa recién pulsada).
        self.fresh_poll = self.poll_seq + 1;
        let done = action.done_message();
        let refresh_queue = action.changes_queue();
        self.spawn(move |sp| async move {
            Msg::ActionDone {
                done,
                refresh_queue,
                result: spotify::perform(&sp, action).await,
            }
        });
    }

    /// Respuesta de una acción: se avisa del resultado y se vuelve a
    /// consultar el estado cuando Spotify ya lo refleje.
    pub(super) fn on_action_done(
        &mut self,
        done: Option<&'static str>,
        refresh_queue: bool,
        result: Result<()>,
    ) {
        let succeeded = result.is_ok();
        match result {
            Ok(()) => {
                if let Some(text) = done {
                    self.set_status(text, false);
                }
            }
            Err(e) => {
                self.note_rate_limit(&e);
                self.set_status(e.to_string(), true);
            }
        }
        self.schedule_refresh(SETTLE);
        // La cola visible se recarga sin quitar el foco ni la selección.
        if succeeded && refresh_queue && self.active_library == Some(LibraryItem::Queue) {
            self.schedule_list_reload();
        }
    }

    // -----------------------------------------------------------------------
    // Controles (con actualización optimista del estado local)
    // -----------------------------------------------------------------------

    pub(super) fn toggle_play(&mut self) {
        let Some(pb) = self.playback.as_mut() else {
            self.set_status(
                "No hay nada reproduciéndose. Elige una canción o un dispositivo ('d').",
                true,
            );
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
        let Some(pb) = self.playback.as_mut() else {
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
        let Some(pb) = self.playback.as_mut() else {
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

    pub(super) fn toggle_shuffle(&mut self) {
        let Some(pb) = self.playback.as_mut() else {
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
        let Some(pb) = self.playback.as_mut() else {
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
    use crate::app::tests::{crash, key, now_playing, test_app};

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
        // Las consultas posteriores a la acción sí se aplican.
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
