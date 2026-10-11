//! Sesión de Spotify: renovación del token de una en una ([`ensure_token`]),
//! espera a la renovación en curso antes de salir ([`wait_for_renewal`]) y
//! guardado del token en disco ([`save_session`]).
//!
//! Con PKCE cada refresh token solo sirve una vez: por eso las renovaciones
//! van por turnos, se espera tras un fallo ([`RefreshFailure`]) y no se corta
//! ninguna a medias al cerrar la app.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use rspotify::{ClientError, prelude::*};
use tokio::sync::RwLockReadGuard;

use super::Client;
use super::errors::{OFFLINE, error_response, is_network_error, retry_after};

/// Guarda en disco el token que hay en memoria (rspotify no lo hace: la caché
/// de `token.json` la gestiona [`crate::config::save_token`]). La escritura
/// se hace en un hilo aparte, de uno en uno con los demás guardados.
pub async fn save_session(sp: &Client) -> Result<()> {
    TOKEN_GATE.save(sp).await
}

async fn token_in_memory(sp: &Client) -> Option<rspotify::Token> {
    match sp.token.lock().await {
        Ok(token) => token.clone(),
        Err(_) => None,
    }
}

/// Espera tras un rechazo de Spotify al renovar la sesión.
const REFRESH_RETRY_REJECTED: Duration = Duration::from_secs(30);
/// Espera tras un fallo de red al renovar la sesión.
const REFRESH_RETRY_OFFLINE: Duration = Duration::from_secs(10);
/// Espera tras un fallo pasajero de Spotify al renovar (5xx, respuesta rara).
const REFRESH_RETRY_TRANSIENT: Duration = Duration::from_secs(15);
/// Espera máxima que se acepta del `Retry-After` de un 429.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Mensaje cuando Spotify no acepta renovar la sesión (token revocado, etc.).
const SESSION_EXPIRED: &str = "La sesión de Spotify caducó y no se pudo renovar: \
     sal con q y ejecuta `spotify_cmd --logout` para volver a entrar";
/// Mensaje cuando Spotify falla al renovar pero la sesión sigue siendo válida.
const REFRESH_TROUBLE: &str = "Spotify tiene problemas para renovar la sesión; se reintentará solo";
/// Mensaje cuando hace falta renovar pero la app ya está cerrándose.
const CLOSING: &str = "La app se está cerrando: ya no se renueva la sesión";

/// Por qué falló la renovación del token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailure {
    /// No se pudo hablar con Spotify (sin red, DNS, tiempo agotado).
    Offline,
    /// Spotify rechazó el refresh token (revocado o ya usado): hay que volver
    /// a iniciar sesión.
    Rejected,
    /// Fallo pasajero de Spotify (5xx, 429...): el refresh token sigue
    /// valiendo y se reintenta tras la espera indicada.
    Transient(Duration),
}

impl RefreshFailure {
    /// Clasifica el error de una renovación.
    pub fn of(err: &ClientError) -> Self {
        if is_network_error(err) {
            return RefreshFailure::Offline;
        }
        match error_response(err) {
            Some(resp) => Self::from_status(resp.status().as_u16(), retry_after(resp)),
            // Una respuesta ilegible no demuestra que la sesión no valga.
            None => RefreshFailure::Transient(REFRESH_RETRY_TRANSIENT),
        }
    }

    /// Solo un 400 o un 401 (`invalid_grant`, `invalid_client`) rechazan el
    /// refresh token. Un 429 espera lo que pida Spotify (con tope) y el resto
    /// (5xx...) se trata como un fallo pasajero.
    fn from_status(status: u16, retry_after: Option<Duration>) -> Self {
        match status {
            400 | 401 => RefreshFailure::Rejected,
            429 => RefreshFailure::Transient(
                retry_after
                    .unwrap_or(REFRESH_RETRY_TRANSIENT)
                    .min(MAX_RETRY_AFTER),
            ),
            _ => RefreshFailure::Transient(REFRESH_RETRY_TRANSIENT),
        }
    }

    /// Cuánto esperar antes de reintentar y qué avisar mientras tanto.
    fn retry(self) -> (Duration, &'static str) {
        match self {
            RefreshFailure::Offline => (REFRESH_RETRY_OFFLINE, OFFLINE),
            RefreshFailure::Rejected => (REFRESH_RETRY_REJECTED, SESSION_EXPIRED),
            RefreshFailure::Transient(wait) => (wait, REFRESH_TROUBLE),
        }
    }
}

async fn token_is_valid(sp: &Client) -> bool {
    match sp.token.lock().await {
        Ok(token) => token.as_ref().is_some_and(|t| !t.is_expired()),
        Err(_) => false,
    }
}

/// Refresh token que hay en memoria.
async fn refresh_token_in_memory(sp: &Client) -> Option<String> {
    match sp.token.lock().await {
        Ok(token) => token.as_ref().and_then(|t| t.refresh_token.clone()),
        Err(_) => None,
    }
}

async fn set_token(sp: &Client, token: rspotify::Token) {
    if let Ok(mut slot) = sp.token.lock().await {
        *slot = Some(token);
    }
}

/// Próximo reintento permitido tras un fallo al renovar y el aviso que se da
/// hasta entonces.
type RetryState = Option<(Instant, &'static str)>;

/// Turno para renovar el token y para guardarlo en disco.
struct TokenGate {
    /// Se mantiene tomado mientras dura una renovación.
    retry: tokio::sync::Mutex<RetryState>,
    /// Puerta cerrada al salir: desde que se empieza a esperar a la
    /// renovación en curso, no empieza ninguna otra. Se consulta con `retry`
    /// tomado, así que una renovación o empezó antes (y se la espera) o ya no
    /// empieza.
    closed: AtomicBool,
    /// Cada guardado tras una renovación tiene tomada una lectura; quien
    /// quiere esperar a que acaben todos pide la escritura.
    saves: tokio::sync::RwLock<()>,
    /// Turno de escritura en disco: los guardados van de uno en uno.
    order: tokio::sync::Mutex<()>,
}

impl TokenGate {
    const fn new() -> Self {
        TokenGate {
            retry: tokio::sync::Mutex::const_new(None),
            closed: AtomicBool::new(false),
            saves: tokio::sync::RwLock::const_new(()),
            order: tokio::sync::Mutex::const_new(()),
        }
    }

    /// Guarda el token que hay en memoria. La escritura (con `fsync`) se hace
    /// en un hilo aparte para no bloquear el runtime de tokio si el disco va
    /// lento. Cada guardado lee el token cuando le llega el turno, así que el
    /// último en escribir guarda el más reciente.
    async fn save(&self, sp: &Client) -> Result<()> {
        let _turn = self.order.lock().await;
        let Some(token) = token_in_memory(sp).await else {
            return Ok(());
        };
        let path = sp.config.cache_path.clone();
        tokio::task::spawn_blocking(move || crate::config::save_token(&path, &token))
            .await
            .map_err(|e| anyhow!("no se pudo guardar la sesión: {e}"))?
    }

    /// Cierra la puerta (no empieza ninguna renovación más) y espera, como
    /// mucho `max`, a que no quede ninguna renovación ni ningún guardado de
    /// una renovación en curso. Devuelve si terminaron a tiempo. La puerta
    /// sigue cerrada aunque se agote la espera.
    async fn wait_idle(&self, max: Duration) -> bool {
        self.closed.store(true, Ordering::SeqCst);
        tokio::time::timeout(max, self.idle()).await.is_ok()
    }

    /// `true` desde que se empezó a esperar para salir.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn idle(&self) {
        let _renewal = self.retry.lock().await;
        let _saves = self.saves.write().await;
    }
}

/// El de la app: compartido por todas las peticiones.
static TOKEN_GATE: TokenGate = TokenGate::new();

/// Renueva el token si ha caducado, de uno en uno. Con PKCE cada refresh token
/// solo sirve una vez, así que varias tareas renovándolo a la vez fallarían.
///
/// Si la renovación falla, no se reintenta en cada petición: se espera un
/// rato (según [`RefreshFailure`]).
pub async fn ensure_token(sp: &Client) -> Result<()> {
    ensure_token_with(sp, &TOKEN_GATE).await
}

/// Espera, como mucho `max`, a que terminen la renovación del token en curso
/// y su guardado; devuelve si terminaron a tiempo. Se llama antes de salir:
/// cortarlas a medias puede dejar en `token.json` un refresh token ya gastado
/// (con PKCE solo sirven una vez) y obligar a iniciar sesión otra vez en el
/// navegador.
///
/// Desde la primera llamada ya no empieza ninguna renovación: las tareas de
/// la app siguen vivas hasta que termina el proceso y, si alguna empezara una
/// ahora, el cierre la cortaría a medias.
pub async fn wait_for_renewal(max: Duration) -> bool {
    TOKEN_GATE.wait_idle(max).await
}

async fn ensure_token_with(sp: &Client, gate: &'static TokenGate) -> Result<()> {
    let mut retry = gate.retry.lock().await;
    if token_is_valid(sp).await {
        *retry = None;
        return Ok(());
    }
    if gate.is_closed() {
        anyhow::bail!(CLOSING);
    }
    if let Some((at, reason)) = *retry
        && Instant::now() < at
    {
        anyhow::bail!(reason);
    }
    match refresh_session(sp).await {
        Ok(()) => {
            *retry = None;
            // El guardado se apunta antes de soltar el turno (así quien espera
            // a que todo acabe, al salir, también lo espera). Nunca espera:
            // solo `idle` pide la escritura, y lo hace con el turno tomado.
            let saving = gate.saves.read().await;
            drop(retry);
            save_in_background(sp.clone(), gate, saving);
            Ok(())
        }
        Err(failure) => {
            let (wait, reason) = failure.retry();
            *retry = Some((Instant::now() + wait, reason));
            anyhow::bail!(reason)
        }
    }
}

/// Guarda la sesión renovada en segundo plano: el token en memoria ya vale,
/// así que ni la petición que pidió renovar ni las demás esperan al disco.
/// Si no se puede guardar (disco lleno, permisos), la app sigue con el token
/// en memoria; solo habrá que volver a entrar la próxima vez que se abra.
fn save_in_background(sp: Client, gate: &'static TokenGate, saving: RwLockReadGuard<'static, ()>) {
    tokio::spawn(async move {
        let _ = gate.save(&sp).await;
        drop(saving);
    });
}

/// Renueva el token. Si Spotify rechaza el refresh token, puede que otra
/// instancia de la app ya lo usara y guardara uno nuevo en `token.json`: en
/// ese caso se adopta el del disco antes de dar la sesión por perdida.
async fn refresh_session(sp: &Client) -> Result<(), RefreshFailure> {
    let failure = match sp.refresh_token().await {
        Ok(()) => return Ok(()),
        Err(e) => RefreshFailure::of(&e),
    };
    if failure != RefreshFailure::Rejected {
        return Err(failure);
    }
    adopt_saved_token(sp).await
}

/// Usa el token de `token.json` si su refresh token es distinto del que hay
/// en memoria: tal cual si sigue vigente o, si ha caducado, renovándolo una
/// sola vez más.
async fn adopt_saved_token(sp: &Client) -> Result<(), RefreshFailure> {
    let saved = crate::config::load_token(&sp.config.cache_path, &sp.oauth.scopes)
        .filter(|t| t.refresh_token.is_some())
        .ok_or(RefreshFailure::Rejected)?;
    if saved.refresh_token == refresh_token_in_memory(sp).await {
        return Err(RefreshFailure::Rejected);
    }
    let expired = saved.is_expired();
    set_token(sp, saved).await;
    if !expired {
        return Ok(());
    }
    sp.refresh_token().await.map_err(|e| RefreshFailure::of(&e))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Solo un 400/401 de accounts.spotify.com significa que hay que volver a
    /// entrar; una caída (5xx) o un 429 se reintentan solos.
    #[test]
    fn refresh_failures_are_classified_by_status() {
        let transient = RefreshFailure::Transient(REFRESH_RETRY_TRANSIENT);
        assert_eq!(
            RefreshFailure::from_status(400, None),
            RefreshFailure::Rejected
        );
        assert_eq!(
            RefreshFailure::from_status(401, None),
            RefreshFailure::Rejected
        );
        assert_eq!(RefreshFailure::from_status(500, None), transient);
        assert_eq!(RefreshFailure::from_status(503, None), transient);
        assert_eq!(RefreshFailure::from_status(429, None), transient);
        assert_eq!(
            RefreshFailure::from_status(429, Some(Duration::from_secs(7))),
            RefreshFailure::Transient(Duration::from_secs(7))
        );
        assert_eq!(
            RefreshFailure::from_status(429, Some(Duration::from_secs(3600))),
            RefreshFailure::Transient(MAX_RETRY_AFTER)
        );
        // Un error que no es una respuesta de Spotify no demuestra nada.
        assert_eq!(RefreshFailure::of(&ClientError::InvalidToken), transient);

        assert!(RefreshFailure::Rejected.retry().1.contains("--logout"));
        assert!(!transient.retry().1.contains("--logout"));
        assert!(!RefreshFailure::Offline.retry().1.contains("--logout"));
    }

    // -- Renovación del token contra un servidor de tokens de prueba --------

    /// Peticiones recibidas por el servidor de tokens de prueba (sus cuerpos).
    type Requests = Arc<std::sync::Mutex<Vec<String>>>;

    fn http_response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    fn rejected() -> String {
        http_response("400 Bad Request", "", r#"{"error":"invalid_grant"}"#)
    }

    fn renewed() -> String {
        let body = r#"{"access_token":"renovado","token_type":"Bearer","expires_in":3600,"refresh_token":"r2","scope":""}"#;
        http_response("200 OK", "", body)
    }

    /// Lee una petición HTTP completa y devuelve su cuerpo.
    async fn read_body(stream: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return String::new();
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).into_owned();
            let Some(end) = text.find("\r\n\r\n") else {
                continue;
            };
            let len = text[..end]
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if data.len() >= end + 4 + len {
                return String::from_utf8_lossy(&data[end + 4..]).into_owned();
            }
        }
    }

    /// Imita accounts.spotify.com: responde a cada petición con la siguiente
    /// respuesta de la lista (la última se repite). Devuelve su URL.
    async fn token_server(responses: Vec<String>) -> (String, Requests) {
        let (url, requests, replies) = held_token_server(responses).await;
        replies.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        (url, requests)
    }

    /// Como [`token_server`], pero cada respuesta espera a que la prueba la
    /// deje salir con `add_permits(1)` en el semáforo devuelto: así la
    /// renovación sigue en curso todo el tiempo que haga falta, sin depender
    /// de esperas.
    async fn held_token_server(
        responses: Vec<String>,
    ) -> (String, Requests, Arc<tokio::sync::Semaphore>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Requests::default();
        let seen = requests.clone();
        let replies = Arc::new(tokio::sync::Semaphore::new(0));
        let permits = replies.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let body = read_body(&mut stream).await;
                let n = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(body);
                    seen.len()
                };
                let response = &responses[(n - 1).min(responses.len() - 1)];
                permits.acquire().await.unwrap().forget();
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (url, requests, replies)
    }

    fn session_token(access: &str, refresh: &str, valid: bool) -> rspotify::Token {
        rspotify::Token {
            access_token: access.into(),
            refresh_token: Some(refresh.into()),
            expires_at: valid.then(|| chrono::Utc::now() + chrono::Duration::hours(1)),
            ..Default::default()
        }
    }

    /// Cliente con un token caducado (refresh token `r0`) en memoria, que
    /// renueva contra `auth_url` y guarda la sesión en un directorio propio.
    async fn expired_session(name: &str, auth_url: &str) -> (Client, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("spotify_cmd_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = rspotify::Config {
            token_cached: false,
            token_refreshing: false,
            cache_path: dir.join("token.json"),
            auth_base_url: auth_url.into(),
            ..Default::default()
        };
        let sp = Client::with_config(Default::default(), Default::default(), config);
        set_token(&sp, session_token("viejo", "r0", false)).await;
        (sp, dir)
    }

    async fn access_token(sp: &Client) -> Option<String> {
        sp.token
            .lock()
            .await
            .unwrap()
            .as_ref()
            .map(|t| t.access_token.clone())
    }

    /// Una caída de accounts.spotify.com no da la sesión por perdida: avisa
    /// de que se reintentará y no insiste en cada petición.
    #[tokio::test]
    async fn server_errors_while_refreshing_are_transient() {
        let down = http_response("503 Service Unavailable", "", "");
        let (url, requests) = token_server(vec![down]).await;
        let (sp, dir) = expired_session("refresh_503", &url).await;
        let gate = test_gate();

        let err = ensure_token_with(&sp, gate).await.unwrap_err().to_string();
        assert_eq!(err, REFRESH_TROUBLE);
        let err = ensure_token_with(&sp, gate).await.unwrap_err().to_string();
        assert_eq!(err, REFRESH_TROUBLE);
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "espera antes de reintentar"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ante un 429 se espera lo que indique `Retry-After`.
    #[tokio::test]
    async fn rate_limited_refresh_waits_retry_after() {
        let busy = http_response("429 Too Many Requests", "retry-after: 60\r\n", "");
        let (url, _requests) = token_server(vec![busy]).await;
        let (sp, dir) = expired_session("refresh_429", &url).await;
        let gate = test_gate();

        let err = ensure_token_with(&sp, gate).await.unwrap_err().to_string();
        assert!(!err.contains("--logout"));
        let (at, _) = gate.retry.lock().await.unwrap();
        assert!(at > Instant::now() + Duration::from_secs(50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spotify rechaza el refresh token y no hay otro en disco: hay que
    /// volver a entrar.
    #[tokio::test]
    async fn rejected_refresh_expires_the_session() {
        let (url, requests) = token_server(vec![rejected()]).await;
        let (sp, dir) = expired_session("refresh_rejected", &url).await;
        // En disco está el mismo refresh token que en memoria.
        crate::config::save_token(&sp.config.cache_path, &session_token("viejo", "r0", false))
            .unwrap();
        let gate = test_gate();

        let err = ensure_token_with(&sp, gate).await.unwrap_err().to_string();
        assert_eq!(err, SESSION_EXPIRED);
        assert_eq!(requests.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Otra instancia ya renovó (el refresh token de memoria ya no vale) y
    /// guardó en disco un token vigente: se adopta sin pedir nada más.
    #[tokio::test]
    async fn adopts_valid_token_saved_by_another_instance() {
        let (url, requests) = token_server(vec![rejected()]).await;
        let (sp, dir) = expired_session("adopt_valid", &url).await;
        crate::config::save_token(
            &sp.config.cache_path,
            &session_token("de-disco", "r1", true),
        )
        .unwrap();
        let gate = test_gate();

        ensure_token_with(&sp, gate).await.unwrap();
        assert_eq!(access_token(&sp).await.as_deref(), Some("de-disco"));
        assert_eq!(requests.lock().unwrap().len(), 1);
        gate.wait_idle(Duration::from_secs(5)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El token del disco también caducó: se renueva una vez con su refresh
    /// token y el resultado se guarda.
    #[tokio::test]
    async fn renews_with_refresh_token_saved_by_another_instance() {
        let (url, requests) = token_server(vec![rejected(), renewed()]).await;
        let (sp, dir) = expired_session("adopt_expired", &url).await;
        crate::config::save_token(
            &sp.config.cache_path,
            &session_token("de-disco", "r1", false),
        )
        .unwrap();
        let gate = test_gate();

        ensure_token_with(&sp, gate).await.unwrap();
        assert_eq!(access_token(&sp).await.as_deref(), Some("renovado"));
        gate.wait_idle(Duration::from_secs(5)).await;
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("refresh_token=r0"));
        assert!(requests[1].contains("refresh_token=r1"));
        let saved = crate::config::load_token(&sp.config.cache_path, &Default::default()).unwrap();
        assert_eq!(saved.access_token, "renovado");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Puerta propia de cada prueba (como la de la app, vive hasta el final).
    fn test_gate() -> &'static TokenGate {
        Box::leak(Box::new(TokenGate::new()))
    }

    /// Espera (con tope) a que el servidor de prueba reciba `n` peticiones.
    async fn wait_requests(requests: &Requests, n: usize) {
        for _ in 0..200 {
            if requests.lock().unwrap().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("el servidor de tokens no recibió la petición");
    }

    /// Espera (con tope) a que la renovación deje el token nuevo en memoria,
    /// y un poco más para que la tarea llegue hasta donde pueda avanzar.
    async fn wait_renewed(sp: &Client) {
        for _ in 0..200 {
            if access_token(sp).await.as_deref() == Some("renovado") {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("la renovación no terminó");
    }

    fn saved_access_token(sp: &Client) -> Option<String> {
        crate::config::load_token(&sp.config.cache_path, &Default::default())
            .map(|t| t.access_token)
    }

    /// Lanza la renovación del token de `sp` en su propia tarea.
    fn spawn_renewal(sp: &Client, gate: &'static TokenGate) -> tokio::task::JoinHandle<Result<()>> {
        let sp = sp.clone();
        tokio::spawn(async move { ensure_token_with(&sp, gate).await })
    }

    /// Al salir con una renovación en curso, se espera a que termine y quede
    /// guardada: cortarla dejaría en disco un refresh token ya gastado.
    ///
    /// Para eso el guardado se apunta en `saves` antes de soltar el turno: si
    /// no, quien espera para salir podría tomar el turno justo entre medias y
    /// dar todo por terminado sin esperar al guardado. Ese hueco es tan corto
    /// que el planificador casi nunca lo deja ver, así que se fuerza: con el
    /// registro de guardados ocupado, la renovación que termina debe seguir
    /// con el turno hasta conseguir apuntarse.
    #[tokio::test]
    async fn exit_waits_for_the_renewal_in_progress_and_its_save() {
        let (url, requests, replies) = held_token_server(vec![renewed()]).await;
        let (sp, dir) = expired_session("exit_wait", &url).await;
        let gate = test_gate();
        let renewal = spawn_renewal(&sp, gate);
        wait_requests(&requests, 1).await;

        // 1. Con la renovación en curso, la salida espera.
        assert!(!gate.wait_idle(Duration::from_millis(100)).await);

        // 2. La renovación termina, pero no puede apuntar su guardado: no
        //    suelta el turno.
        let registry_busy = gate.saves.write().await;
        replies.add_permits(1);
        wait_renewed(&sp).await;
        assert!(
            gate.retry.try_lock().is_err(),
            "el turno se soltó antes de apuntar el guardado"
        );

        // 3. Ya apuntado, la salida espera también a que llegue al disco.
        drop(registry_busy);
        assert!(gate.wait_idle(Duration::from_secs(5)).await);
        assert_eq!(saved_access_token(&sp).as_deref(), Some("renovado"));
        assert!(renewal.await.unwrap().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Desde que se empieza a esperar para salir no empieza ninguna
    /// renovación, aunque la espera se agote: el cierre la cortaría a medias.
    /// Un token que aún vale se sigue usando.
    #[tokio::test]
    async fn no_renewal_starts_once_the_exit_wait_begins() {
        let (url, requests) = token_server(vec![renewed()]).await;
        let (sp, dir) = expired_session("closed_gate", &url).await;
        let gate = test_gate();
        // La espera se agota con una renovación colgada que luego termina.
        let hung = gate.retry.lock().await;
        assert!(!gate.wait_idle(Duration::from_millis(50)).await);
        drop(hung);

        let err = ensure_token_with(&sp, gate).await.unwrap_err().to_string();
        assert_eq!(err, CLOSING);
        assert!(requests.lock().unwrap().is_empty(), "no se pidió renovar");
        assert_eq!(access_token(&sp).await.as_deref(), Some("viejo"));

        set_token(&sp, session_token("vigente", "r0", true)).await;
        assert!(ensure_token_with(&sp, gate).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Si el guardado no termina (disco colgado), la espera de la salida no
    /// pasa del tope (y `main` termina el proceso sin esperar al hilo del
    /// guardado).
    #[tokio::test]
    async fn exit_wait_is_bounded() {
        let gate = test_gate();
        assert!(gate.wait_idle(Duration::from_secs(5)).await);

        let _hung_save = gate.saves.read().await;
        let start = Instant::now();
        assert!(!gate.wait_idle(Duration::from_millis(100)).await);
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(100));
        assert!(waited < Duration::from_secs(3));
    }

    /// Un disco lento al guardar no frena ninguna petición, tampoco la que
    /// pidió renovar: el token en memoria ya vale. La salida, en cambio, sí
    /// espera (con tope) a que llegue al disco.
    #[tokio::test]
    async fn slow_save_does_not_hold_requests() {
        let (url, _requests) = token_server(vec![renewed()]).await;
        let (sp, dir) = expired_session("slow_save", &url).await;
        let gate = test_gate();
        // Simula un disco lento: el turno de escritura está ocupado.
        let slow_disk = gate.order.lock().await;

        let quick = Duration::from_secs(2);
        let renewal = tokio::time::timeout(quick, ensure_token_with(&sp, gate)).await;
        assert!(renewal.expect("la renovación esperó al disco").is_ok());
        let other = tokio::time::timeout(quick, ensure_token_with(&sp, gate)).await;
        assert!(other.expect("otra petición esperó al disco").is_ok());
        assert_eq!(access_token(&sp).await.as_deref(), Some("renovado"));
        assert_eq!(saved_access_token(&sp), None, "aún no se ha escrito");

        let start = Instant::now();
        assert!(!gate.wait_idle(Duration::from_millis(300)).await);
        assert!(start.elapsed() >= Duration::from_millis(300));

        drop(slow_disk);
        assert!(gate.wait_idle(Duration::from_secs(5)).await);
        assert_eq!(saved_access_token(&sp).as_deref(), Some("renovado"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El token renovado en memoria se guarda en la ruta de la configuración.
    #[tokio::test]
    async fn save_session_writes_the_token_in_memory() {
        let dir = std::env::temp_dir().join(format!(
            "spotify_cmd_test_{}_save_session",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        let config = rspotify::Config {
            token_cached: false,
            cache_path: path.clone(),
            ..Default::default()
        };
        let sp = Client::with_config(Default::default(), Default::default(), config);

        // Sin token en memoria no se escribe nada.
        save_session(&sp).await.unwrap();
        assert!(!path.exists());

        *sp.token.lock().await.unwrap() = Some(rspotify::Token {
            access_token: "renovado".into(),
            ..Default::default()
        });
        save_session(&sp).await.unwrap();
        let saved = crate::config::load_token(&path, &Default::default()).unwrap();
        assert_eq!(saved.access_token, "renovado");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
