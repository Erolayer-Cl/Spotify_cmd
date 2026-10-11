//! Tareas en segundo plano protegidas: siempre responden al bucle principal
//! con un [`Msg`], también cuando sufren un *panic* (entonces con
//! [`Msg::Crashed`]). Las peticiones a Spotify renuevan antes la sesión si
//! hace falta.

use std::{future::Future, panic::AssertUnwindSafe};

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
fn crashed(origin: CrashOrigin, payload: Box<dyn std::any::Any + Send>) -> Msg {
    Msg::Crashed {
        origin,
        reason: terminal::panic_message(payload),
    }
}

/// Renueva la sesión si hace falta, en su propia tarea: si se cancela la
/// petición que la necesitaba (una lista que ya no interesa), la renovación
/// termina igualmente. Cortarla a medias perdería la sesión, porque con PKCE
/// cada refresh token solo sirve una vez.
///
/// Si no se pudo renovar, se avisa y la petición sigue (fallará con su propio
/// mensaje, sin bloquear la interfaz). Devuelve el *panic* de la renovación,
/// si lo hubo: la petición que la esperaba lo comunica como suyo.
async fn renew_session(
    sp: Client,
    tx: &UnboundedSender<Msg>,
) -> Option<Box<dyn std::any::Any + Send>> {
    match tokio::spawn(async move { spotify::ensure_token(&sp).await }).await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => {
            let _ = tx.send(Msg::SessionProblem(e.to_string()));
            None
        }
        Err(e) => e.try_into_panic().ok(),
    }
}

impl App {
    /// Lanza una petición a Spotify en segundo plano (renovando antes la
    /// sesión si hace falta). Devuelve el `AbortHandle` para cancelarla.
    pub(super) fn spawn<F, Fut>(&self, f: F) -> AbortHandle
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        self.spawn_as(CrashOrigin::Other, f)
    }

    /// Como [`Self::spawn`]; un *panic* (también el de la renovación de la
    /// sesión) se comunica como de origen `origin`.
    fn spawn_as<F, Fut>(&self, origin: CrashOrigin, f: F) -> AbortHandle
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        let sp = self.spotify.clone();
        let tx = self.tx.clone();
        spawn_guarded(self.tx.clone(), origin, async move {
            if let Some(payload) = renew_session(sp.clone(), &tx).await {
                return crashed(origin, payload);
            }
            f(sp).await
        })
    }

    /// Lanza la carga de la petición de lista en curso y guarda su tarea para
    /// poder cancelarla. Si sufre un *panic*, el fallo se atribuye a esta
    /// petición.
    pub(super) fn spawn_list<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Client) -> Fut + Send + 'static,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        let origin = CrashOrigin::List(self.request_id);
        self.list_task = Some(self.spawn_as(origin, f));
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

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
