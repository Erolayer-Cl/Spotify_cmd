//! Tareas en segundo plano protegidas: siempre responden al bucle principal
//! con un [`Msg`], también cuando sufren un *panic* (entonces con
//! [`Msg::Crashed`]). Las peticiones a Spotify renuevan antes la sesión si
//! hace falta.

use std::{any::Any, future::Future, panic::AssertUnwindSafe};

use anyhow::Result;
use futures::FutureExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use super::{App, CrashOrigin, Msg};
use crate::spotify::{self, Client};
use crate::terminal;

/// Lanza una tarea que siempre responde con un mensaje: si la tarea sufre un
/// *panic*, se envía [`Msg::Crashed`] (con su `origin`) en lugar de perder la
/// respuesta. Solo una tarea cancelada (con el `AbortHandle` devuelto) no
/// responde.
pub fn spawn_guarded<Fut>(tx: UnboundedSender<Msg>, origin: CrashOrigin, task: Fut) -> AbortHandle
where
    Fut: Future<Output = Msg> + Send + 'static,
{
    tokio::spawn(async move {
        let msg = match AssertUnwindSafe(task).catch_unwind().await {
            Ok(msg) => msg,
            Err(payload) => crashed(origin, payload),
        };
        let _ = tx.send(msg);
    })
    .abort_handle()
}

/// Igual que [`spawn_guarded`] pero para trabajo pesado de CPU (en otro hilo).
pub fn spawn_blocking_guarded<F>(tx: UnboundedSender<Msg>, origin: CrashOrigin, work: F)
where
    F: FnOnce() -> Msg + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let msg = std::panic::catch_unwind(AssertUnwindSafe(work))
            .unwrap_or_else(|payload| crashed(origin, payload));
        let _ = tx.send(msg);
    });
}

/// Mensaje de un *panic* en una tarea de origen `origin`.
fn crashed(origin: CrashOrigin, payload: Box<dyn Any + Send>) -> Msg {
    Msg::Crashed {
        origin,
        reason: terminal::panic_message(payload),
    }
}

/// Estado de la sesión antes de enviar una petición.
enum Session {
    /// Se puede enviar.
    Ready,
    /// No se pudo renovar (por este motivo) y el token en memoria ya caducó:
    /// Spotify respondería 401 y el aviso pediría `--logout` sin motivo (el
    /// refresh token puede seguir valiendo). La petición no se envía y
    /// responde con un [`SessionUnusable`].
    Unusable(String),
    /// La renovación sufrió un *panic*: la petición que la esperaba lo
    /// comunica como suyo.
    Crashed(Box<dyn Any + Send>),
}

/// Error de una petición que no se envió porque la sesión no sirve (ver
/// [`Session::Unusable`]). Su motivo ya se avisó con [`Msg::SessionProblem`].
#[derive(Debug)]
struct SessionUnusable(String);

impl std::fmt::Display for SessionUnusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionUnusable {}

/// Renueva la sesión si hace falta, en su propia tarea: si se cancela la
/// petición que la necesitaba (una lista que ya no interesa), la renovación
/// termina igualmente. Cortarla a medias perdería la sesión, porque con PKCE
/// cada refresh token solo sirve una vez.
///
/// Si no se pudo renovar, se avisa. La petición solo sigue si no lleva un
/// token caducado (sin token falla sola, sin salir a la red).
async fn renew_session(sp: &Client, tx: &UnboundedSender<Msg>) -> Session {
    let renewal = {
        let sp = sp.clone();
        tokio::spawn(async move { spotify::ensure_token(&sp).await })
    };
    match renewal.await {
        Ok(Ok(())) => Session::Ready,
        Ok(Err(e)) => {
            let reason = e.to_string();
            let _ = tx.send(Msg::SessionProblem(reason.clone()));
            if spotify::token_expired(sp).await {
                Session::Unusable(reason)
            } else {
                Session::Ready
            }
        }
        Err(e) => match e.try_into_panic() {
            Ok(payload) => Session::Crashed(payload),
            Err(_) => Session::Ready,
        },
    }
}

impl App {
    /// Lanza en segundo plano la petición `query` a Spotify (renovando antes
    /// la sesión si hace falta) y responde con `reply` y su resultado. Si la
    /// sesión no sirve (ver [`Session::Unusable`]), `query` no se ejecuta y
    /// `reply` recibe el error de la renovación. Devuelve el `AbortHandle`
    /// para cancelarla.
    pub(super) fn spawn<T, Q, Fut, R>(&self, query: Q, reply: R) -> AbortHandle
    where
        Q: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        R: FnOnce(Result<T>) -> Msg + Send + 'static,
    {
        self.spawn_as(CrashOrigin::Other, query, reply)
    }

    /// Como [`Self::spawn`]; un *panic* (también el de la renovación de la
    /// sesión) se comunica como de origen `origin`.
    fn spawn_as<T, Q, Fut, R>(&self, origin: CrashOrigin, query: Q, reply: R) -> AbortHandle
    where
        Q: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        R: FnOnce(Result<T>) -> Msg + Send + 'static,
    {
        let sp = self.spotify.clone();
        let tx = self.tx.clone();
        spawn_guarded(self.tx.clone(), origin, async move {
            let result = match renew_session(&sp, &tx).await {
                Session::Ready => query(sp).await,
                Session::Unusable(reason) => Err(anyhow::Error::new(SessionUnusable(reason))),
                Session::Crashed(payload) => return crashed(origin, payload),
            };
            reply(result)
        })
    }

    /// Muestra en la barra de estado el error `err` de una petición, tras
    /// `context`. Si no se envió porque la sesión no sirve, no se muestra: el
    /// motivo ya está a la vista (sin prefijo, para que se lea entero con su
    /// pista) y esto lo taparía.
    pub(super) fn show_request_error(&mut self, context: &str, err: &anyhow::Error) {
        if !err.is::<SessionUnusable>() {
            self.set_status(format!("{context}: {err}"), true);
        }
    }

    /// Lanza la carga de la petición de lista en curso (como [`Self::spawn`])
    /// y guarda su tarea para poder cancelarla. Si sufre un *panic*, el fallo
    /// se atribuye a esta petición.
    pub(super) fn spawn_list<T, Q, Fut, R>(&mut self, query: Q, reply: R)
    where
        Q: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        R: FnOnce(Result<T>) -> Msg + Send + 'static,
    {
        let origin = CrashOrigin::List(self.request_id);
        self.list_task = Some(self.spawn_as(origin, query, reply));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::app::tests::{responses, status, test_app};

    /// Cliente con un token caducado cuya renovación falla: nadie escucha en
    /// la dirección de los tokens (ni en la de la API, por si se enviara la
    /// petición).
    async fn expired_session() -> Client {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .unwrap()
            .port();
        let dir = std::env::temp_dir().join(format!(
            "spotify_cmd_test_{}_expired_request",
            std::process::id()
        ));
        let config = rspotify::Config {
            token_cached: false,
            token_refreshing: false,
            cache_path: dir.join("token.json"),
            auth_base_url: format!("http://127.0.0.1:{port}/"),
            api_base_url: format!("http://127.0.0.1:{port}/v1/"),
            ..Default::default()
        };
        let sp = Client::with_config(Default::default(), Default::default(), config);
        *sp.token.lock().await.unwrap() = Some(rspotify::Token {
            access_token: "caducado".into(),
            refresh_token: Some("r0".into()),
            expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            ..Default::default()
        });
        sp
    }

    /// Si la sesión no se pudo renovar y el token en memoria caducó, la
    /// petición no se envía (Spotify respondería 401 y el aviso pediría
    /// `--logout` sin motivo): responde con el error de la renovación.
    #[tokio::test]
    async fn expired_session_answers_without_sending_the_request() {
        let (mut app, _rx) = test_app();
        app.spotify = expired_session().await;
        let sent = Arc::new(AtomicBool::new(false));
        let flag = sent.clone();
        app.spawn(
            move |_| async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            },
            |result| Msg::ActionDone {
                done: None,
                refresh_queue: false,
                result,
            },
        );
        let msgs = responses(&mut app).await;
        assert!(!sent.load(Ordering::SeqCst), "la petición no debe enviarse");
        let problem = msgs
            .iter()
            .find_map(|m| match m {
                Msg::SessionProblem(reason) => Some(reason.clone()),
                _ => None,
            })
            .expect("se avisa del problema de sesión");
        assert!(msgs.iter().any(|m| matches!(
            m,
            Msg::ActionDone { result: Err(e), .. } if e.to_string() == problem
        )));
    }

    /// La consulta periódica que no se envió por la sesión no tapa su aviso
    /// con uno más largo (que la barra cortaría, p. ej. antes de `--logout`).
    #[tokio::test]
    async fn skipped_poll_keeps_the_session_notice_readable() {
        let (mut app, _rx) = test_app();
        app.spotify = expired_session().await;
        assert!(app.poll_playback());
        for msg in responses(&mut app).await {
            app.on_msg(msg);
        }
        let problem = app.session_problem().map(str::to_owned);
        assert!(problem.is_some(), "se avisa del problema de sesión");
        assert_eq!(status(&app), problem.as_deref());
        assert!(!app.playback_inflight, "se puede volver a consultar");
    }

    /// Los demás errores de una petición se muestran con su contexto.
    #[tokio::test]
    async fn request_errors_are_shown_with_their_context() {
        let (mut app, _rx) = test_app();
        app.show_request_error("Error al consultar", &anyhow::anyhow!("sin red"));
        assert_eq!(status(&app), Some("Error al consultar: sin red"));
        let skipped = anyhow::Error::new(SessionUnusable("sesión caducada".into()));
        app.show_request_error("Otro contexto", &skipped);
        assert_eq!(status(&app), Some("Error al consultar: sin red"));
    }

    #[tokio::test]
    async fn a_panicking_task_is_reported_not_fatal() {
        let (tx, mut rx) = unbounded_channel();
        spawn_guarded(tx.clone(), CrashOrigin::List(7), async {
            panic!("la API devolvió algo imposible")
        });
        match rx.recv().await {
            Some(Msg::Crashed {
                origin: CrashOrigin::List(7),
                reason,
            }) => assert!(reason.contains("imposible")),
            _ => panic!("se esperaba Msg::Crashed de la lista 7"),
        }
        spawn_blocking_guarded(tx, CrashOrigin::Resize(3), || {
            panic!("fallo al redimensionar")
        });
        assert!(matches!(
            rx.recv().await,
            Some(Msg::Crashed {
                origin: CrashOrigin::Resize(3),
                ..
            })
        ));
    }
}
