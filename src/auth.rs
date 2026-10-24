//! Autenticación con Spotify usando Authorization Code + PKCE.
//!
//! Solo hace falta el Client ID (no el secreto). El token se guarda en disco y
//! se renueva automáticamente, así que el navegador solo se abre la primera vez.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use rspotify::{AuthCodePkceSpotify, Credentials, OAuth, prelude::*, scopes};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

use crate::config::{self, Config};
use crate::logo;
use crate::spotify::{self, RefreshFailure};

const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
/// Si la redirección no llega en este tiempo, se explica qué suele fallar.
const HINT_AFTER: Duration = Duration::from_secs(45);
/// Tiempo máximo para recibir la primera línea de una petición en el servidor
/// de la redirección. Los navegadores abren conexiones de reserva que no
/// envían nada; no deben retrasar el login.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Tamaño máximo de la primera línea de una petición (la de Spotify ocupa
/// menos de 1 KiB).
const MAX_REQUEST_LINE: u64 = 8 * 1024;

pub async fn authenticate(cfg: &Config, manual: bool) -> Result<AuthCodePkceSpotify> {
    let creds = Credentials::new_pkce(&cfg.client_id);
    let oauth = OAuth {
        redirect_uri: cfg.redirect_uri.clone(),
        scopes: scopes!(
            "user-read-playback-state",
            "user-modify-playback-state",
            "user-read-currently-playing",
            "playlist-read-private",
            "playlist-read-collaborative",
            "user-library-read",
            "user-read-recently-played"
        ),
        ..Default::default()
    };
    let token_path = config::token_path()?;
    // Temporales que dejó una escritura del token cortada en otra ejecución.
    config::remove_orphan_temps(&token_path);
    let mut spotify = AuthCodePkceSpotify::with_config(creds, oauth, client_config(token_path));

    // 1. Token guardado de una sesión anterior.
    if let Some(token) = config::load_token(&spotify.config.cache_path, &spotify.oauth.scopes) {
        let expired = token.is_expired();
        if let Ok(mut slot) = spotify.token.lock().await {
            *slot = Some(token);
        }
        if !expired {
            return Ok(spotify);
        }
        match spotify::refresh_keeping(&spotify).await {
            Ok(()) => {
                save_session_or_warn(&spotify).await;
                return Ok(spotify);
            }
            // Sin red, o si Spotify falla (5xx, 429), el token guardado sigue
            // valiendo: no tiene sentido abrir el navegador. Se avisa y se sale.
            Err(e) => match RefreshFailure::of(&e) {
                RefreshFailure::Offline => {
                    bail!("no hay conexión con Spotify ({e}). Revisa tu red e inténtalo de nuevo")
                }
                RefreshFailure::Transient(_) => {
                    bail!("Spotify no responde ahora mismo ({e}); inténtalo en unos minutos")
                }
                RefreshFailure::Rejected => say_err!(
                    "No se pudo renovar el token guardado ({e}); hay que iniciar sesión de nuevo."
                ),
            },
        }
    }

    // 2. Flujo completo en el navegador.
    let hint = redirect_hint(&spotify.oauth.redirect_uri);
    let intro = "Inicia sesión en Spotify para autorizar spotify_cmd.";
    let code = browser_code(&mut spotify, manual, intro, &hint).await?;

    spotify
        .request_token(&code)
        .await
        .context("Spotify rechazó el código de autorización")?;
    save_session_or_warn(&spotify).await;
    say!("{}", logo::green("¡Sesión iniciada!"));
    Ok(spotify)
}

/// Pide al usuario que autorice en el navegador y devuelve el código de
/// autorización: lo recoge la redirección a esta máquina o, en modo manual
/// (o si no se puede escuchar), la URL que el usuario pega. `intro` explica
/// para qué es; `hint`, qué revisar si la redirección no llega.
async fn browser_code(
    spotify: &mut AuthCodePkceSpotify,
    manual: bool,
    intro: &str,
    hint: &str,
) -> Result<String> {
    let url = spotify
        .get_authorize_url(None)
        .context("no se pudo generar la URL de autorización")?;

    say!("{intro}\n");
    say!(
        "Si el navegador no se abre solo, visita esta URL:\n\n  {}\n",
        logo::green(&url)
    );

    // El puerto se reserva antes de abrir el navegador: si está ocupado, el
    // código de Spotify acabaría en otro programa.
    let listener = if manual {
        None
    } else {
        bind_redirect(redirect_address(&spotify.oauth.redirect_uri)).await
    };
    if !manual {
        let _ = webbrowser::open(&url);
    }
    match listener {
        Some(listener) => {
            let addr = listener
                .local_addr()
                .map_or_else(|_| "127.0.0.1".to_string(), |a| a.to_string());
            say!("Esperando la redirección en http://{addr} ... (Ctrl+C para cancelar)");
            let state = spotify.oauth.state.clone();
            wait_with_hint(wait_for_code(listener, state), HINT_AFTER, || {
                say!("\n{hint}")
            })
            .await
        }
        None => {
            say!("Tras aceptar, el navegador te llevará a una página que puede no cargar.");
            say_inline!("Copia la URL completa de la barra de direcciones y pégala aquí: ");
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            spotify
                .parse_response_code(input.trim())
                .context("la URL pegada no contiene un código válido")
        }
    }
}

/// Configuración de rspotify. La caché del token (`token.json`) la gestiona la
/// app ([`config::load_token`] y [`config::save_token`]): así se escribe con
/// permisos 0600 y de forma atómica, y un fallo al guardarla no convierte una
/// renovación correcta en un error.
fn client_config(cache_path: std::path::PathBuf) -> rspotify::Config {
    let mut rs_config = rspotify::Config {
        token_cached: false,
        // Las renovaciones las hace solo `spotify::ensure_token`, de una en una
        // y esperando tras un fallo. Si rspotify también renovara por su
        // cuenta, un token revocado provocaría un intento por cada petición.
        token_refreshing: false,
        cache_path,
        ..Default::default()
    };
    // Solo para pruebas: apuntar a un servidor que imita la API de Spotify.
    if let Some(url) = std::env::var("SPOTIFY_CMD_API_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        rs_config.api_base_url = url;
    }
    if let Some(url) = std::env::var("SPOTIFY_CMD_AUTH_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        rs_config.auth_base_url = url;
    }
    rs_config
}

/// Guarda el token nuevo (la escritura va a un hilo aparte: ver
/// [`spotify::save_session`]). Si no se puede, la sesión sigue en memoria: se
/// avisa de que habrá que volver a entrar la próxima vez, pero la app arranca.
async fn save_session_or_warn(spotify: &AuthCodePkceSpotify) {
    if let Err(e) = spotify::save_session(spotify).await {
        say_err!(
            "Aviso: no se pudo guardar la sesión ({e:#}); habrá que volver a entrar la próxima vez."
        );
    }
}

/// Dirección local donde escuchar la redirección, o `None` (modo manual) si
/// la Redirect URI no es `http://` en esta máquina. Con `https://` el
/// navegador hablaría TLS con un servidor que no lo entiende, y con otro host
/// la redirección no llegaría aquí: el login no terminaría nunca.
///
/// No se usa `get_socket_address` de rspotify: con un host que no es una IP
/// (`localhost`, un dominio) escucha en `0.0.0.0`, abierto a toda la red.
fn redirect_address(redirect_uri: &str) -> Option<SocketAddr> {
    let address = local_http_address(redirect_uri);
    if address.is_none() {
        say_err!("La Redirect URI ({redirect_uri}) no es http:// local; se pasa al modo manual.\n");
    }
    address
}

/// Dirección de una URI `http://` de esta máquina (IP de *loopback* o
/// `localhost`, que se escucha solo en 127.0.0.1).
fn local_http_address(uri: &str) -> Option<SocketAddr> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let ip = match url.host()? {
        url::Host::Ipv4(ip) => IpAddr::V4(ip),
        url::Host::Ipv6(ip) => IpAddr::V6(ip),
        url::Host::Domain(name) if name.eq_ignore_ascii_case("localhost") => {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
        url::Host::Domain(_) => return None,
    };
    if !ip.is_loopback() {
        return None;
    }
    Some(SocketAddr::new(ip, url.port_or_known_default()?))
}

/// Qué revisar si tras aceptar en el navegador no llega la redirección.
fn redirect_hint(redirect_uri: &str) -> String {
    format!(
        "¿Ya aceptaste y no pasa nada? Revisa que en tu app de Spotify \
         (developer.spotify.com/dashboard → Settings → Redirect URIs) esté \
         exactamente {redirect_uri}\n\
         Si no, sal con Ctrl+C y usa `spotify_cmd --manual-auth`."
    )
}

/// Espera a `code` como mucho [`AUTH_TIMEOUT`]. Si a los `hint_after` aún
/// no ha llegado, llama una vez a `hint` y sigue esperando.
async fn wait_with_hint(
    code: impl Future<Output = Result<String>>,
    hint_after: Duration,
    hint: impl FnOnce(),
) -> Result<String> {
    let code = tokio::time::timeout(AUTH_TIMEOUT, code);
    tokio::pin!(code);
    let result = tokio::select! {
        result = &mut code => result,
        () = tokio::time::sleep(hint_after) => {
            hint();
            code.await
        }
    };
    result.map_err(|_| anyhow!("se agotó el tiempo de espera de la autorización"))?
}

/// Reserva la dirección de la redirección. Devuelve `None` (y se pasa al modo
/// manual) si la Redirect URI no es local o si otro programa usa el puerto.
async fn bind_redirect(addr: Option<SocketAddr>) -> Option<TcpListener> {
    let addr = addr?;
    match TcpListener::bind(addr).await {
        Ok(listener) => Some(listener),
        Err(e) => {
            say_err!(
                "No se pudo escuchar en {addr} ({e}); quizá otro programa (¿Jupyter?) \
                 usa ese puerto. Se pasa al modo manual.\n"
            );
            None
        }
    }
}

/// Qué significa una petición que llega a la dirección de redirección.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// Spotify devolvió el código de autorización.
    Code(String),
    /// El usuario rechazó la autorización en el navegador.
    Denied,
    /// Trae `code` o `error`, pero no el `state` de este inicio de sesión: no
    /// viene de la redirección de Spotify y se descarta.
    WrongState,
    /// Cualquier otra cosa (favicon.ico, etc.): se ignora.
    Other,
}

/// Interpreta la ruta de una petición (`/callback?code=...&state=...`). Tanto
/// `code` como `error` solo se aceptan con el `state` esperado, para que otro
/// programa no pueda colar un código ni cancelar el login.
fn classify_callback(path: &str, expected_state: &str) -> Callback {
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{path}")) else {
        return Callback::Other;
    };
    let param = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    let (code, error) = (param("code"), param("error"));
    if code.is_none() && error.is_none() {
        return Callback::Other;
    }
    if param("state").as_deref() != Some(expected_state) {
        return Callback::WrongState;
    }
    code.map_or(Callback::Denied, Callback::Code)
}

/// Lee la primera línea de una petición HTTP (`GET /ruta HTTP/1.1`) y devuelve
/// la ruta. Con límite de tiempo y de tamaño: una conexión que no envía nada o
/// que envía una línea interminable se descarta.
async fn read_request_path<S: AsyncRead + Unpin>(stream: &mut S) -> Option<String> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_LINE));
    let mut line = String::new();
    match tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(_)) if line.ends_with('\n') => {
            Some(line.split_whitespace().nth(1).unwrap_or("/").to_string())
        }
        _ => None,
    }
}

/// Atiende una conexión: responde al navegador y devuelve lo que trajo.
async fn handle_connection(mut stream: TcpStream, state: &str) -> Callback {
    let Some(path) = read_request_path(&mut stream).await else {
        return Callback::Other;
    };
    let callback = classify_callback(&path, state);
    let (status, message) = match &callback {
        Callback::Code(_) => ("200 OK", "¡Listo! Ya puedes volver a la terminal."),
        Callback::Denied => ("400 Bad Request", "Autorización cancelada."),
        Callback::WrongState => ("400 Bad Request", "Petición no válida."),
        Callback::Other => ("404 Not Found", ""),
    };
    respond(&mut stream, status, message).await;
    callback
}

/// Mini servidor HTTP que recibe la redirección de Spotify y extrae el código.
/// Cada conexión se atiende en su propia tarea, así que una conexión inactiva
/// (p. ej. una preconexión del navegador) no retrasa a las demás.
async fn wait_for_code(listener: TcpListener, state: String) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<Callback>(8);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let (tx, state) = (tx.clone(), state.clone());
                tokio::spawn(async move {
                    let callback = handle_connection(stream, &state).await;
                    let _ = tx.send(callback).await;
                });
            }
            Some(callback) = rx.recv() => match callback {
                Callback::Code(code) => return Ok(code),
                Callback::Denied => bail!("la autorización fue rechazada en el navegador"),
                Callback::WrongState | Callback::Other => {}
            },
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>spotify_cmd</title>\
         <body style=\"font-family:sans-serif;background:#121212;color:#1db954;\
         display:flex;align-items:center;justify-content:center;height:90vh\">\
         <h1>{message}</h1></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinHandle;

    const STATE: &str = "estado-de-prueba";

    /// Arranca el servidor de la redirección en un puerto libre.
    async fn start_server() -> (SocketAddr, JoinHandle<Result<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (addr, tokio::spawn(wait_for_code(listener, STATE.into())))
    }

    /// Simula una petición del navegador y devuelve la respuesta completa.
    async fn send(addr: SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp).await;
        resp
    }

    /// Simula al navegador: primero pide el favicon y luego vuelve con el código.
    #[tokio::test]
    async fn listener_extracts_code_from_redirect() {
        let (addr, server) = start_server().await;
        assert!(send(addr, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let ok = send(addr, &format!("/callback?code=abc123&state={STATE}")).await;
        assert!(ok.starts_with("HTTP/1.1 200"));
        assert_eq!(server.await.unwrap().unwrap(), "abc123");
    }

    /// Una conexión abierta que no envía nada (preconexión del navegador, `nc`)
    /// no debe retrasar la redirección real.
    #[tokio::test]
    async fn idle_connection_does_not_block_the_callback() {
        let (addr, server) = start_server().await;
        let _mute = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ok = send(addr, &format!("/callback?code=xyz&state={STATE}")).await;
        assert!(ok.starts_with("HTTP/1.1 200"));
        let code = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("la conexión muda bloqueó el login");
        assert_eq!(code.unwrap().unwrap(), "xyz");
    }

    /// Otro programa no puede cancelar el login ni colar un código sin el
    /// `state` correcto; el rechazo real de Spotify sí lo cancela.
    #[tokio::test]
    async fn error_and_code_need_the_right_state() {
        let (addr, server) = start_server().await;
        let fake_error = send(addr, "/callback?error=x").await;
        assert!(fake_error.starts_with("HTTP/1.1 400"));
        let fake_code = send(addr, "/callback?code=malo&state=otro").await;
        assert!(fake_code.starts_with("HTTP/1.1 400"));
        // Se deja tiempo al servidor para procesarlas: debe seguir esperando.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!server.is_finished());

        let denied = send(
            addr,
            &format!("/callback?error=access_denied&state={STATE}"),
        )
        .await;
        assert!(denied.starts_with("HTTP/1.1 400"));
        let result = server.await.unwrap();
        assert!(result.unwrap_err().to_string().contains("rechazada"));
    }

    #[test]
    fn classify_callback_checks_state() {
        let ok = format!("/callback?code=a%2Fb&state={STATE}");
        assert_eq!(
            classify_callback(&ok, STATE),
            Callback::Code("a/b".to_string())
        );
        let denied = format!("/callback?error=access_denied&state={STATE}");
        assert_eq!(classify_callback(&denied, STATE), Callback::Denied);
        assert_eq!(
            classify_callback("/callback?error=x", STATE),
            Callback::WrongState
        );
        assert_eq!(
            classify_callback("/callback?code=a&state=otro", STATE),
            Callback::WrongState
        );
        assert_eq!(classify_callback("/favicon.ico", STATE), Callback::Other);
        assert_eq!(classify_callback("/", STATE), Callback::Other);
        assert_eq!(classify_callback("", STATE), Callback::Other);
    }

    /// Una línea sin terminar o enorme se descarta sin acumular memoria.
    #[tokio::test]
    async fn request_line_is_bounded() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        client.write_all(&[b'a'; 20 * 1024]).await.unwrap();
        assert_eq!(read_request_path(&mut server).await, None);

        let (mut client, mut server) = tokio::io::duplex(1024);
        client
            .write_all(b"GET /callback?code=1 HTTP/1.1\r\n")
            .await
            .unwrap();
        assert_eq!(
            read_request_path(&mut server).await.as_deref(),
            Some("/callback?code=1")
        );
    }

    /// Si el puerto está ocupado (Jupyter usa el 8888), se pasa al modo manual
    /// en lugar de abrir el navegador hacia otro servidor.
    #[tokio::test]
    async fn busy_port_falls_back_to_manual() {
        let busy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = busy.local_addr().unwrap();
        assert!(bind_redirect(Some(addr)).await.is_none());
        assert!(bind_redirect(None).await.is_none());
    }

    /// Solo se escucha la redirección si es `http://` en esta máquina: con
    /// `https://` (o en otra máquina) el login no terminaría nunca.
    #[test]
    fn only_local_http_redirects_are_listened_to() {
        let addr = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        for (uri, expected) in [
            ("http://127.0.0.1:8898/callback", addr("127.0.0.1:8898")),
            ("HTTP://127.0.0.1:8898/", addr("127.0.0.1:8898")),
            ("http://[::1]:8898/callback", addr("[::1]:8898")),
            ("http://127.0.0.1/callback", addr("127.0.0.1:80")),
            // `localhost` solo en 127.0.0.1, nunca en todas las interfaces.
            ("http://localhost:8888/callback", addr("127.0.0.1:8888")),
            ("https://127.0.0.1:8898/callback", None),
            ("http://example.com/callback", None),
            ("http://192.168.1.10:8898/callback", None),
            ("http://0.0.0.0:8898/callback", None),
            ("miapp://callback", None),
            ("no es una url", None),
        ] {
            assert_eq!(local_http_address(uri), expected, "{uri}");
        }
    }

    /// Si la redirección tarda, se explica qué revisar una sola vez y se
    /// sigue esperando; si llega antes, no se dice nada.
    #[tokio::test]
    async fn slow_redirect_gives_a_hint_and_keeps_waiting() {
        let late = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok("codigo".to_string())
        };
        let mut hints = 0;
        let code = wait_with_hint(late, Duration::from_millis(30), || hints += 1).await;
        assert_eq!(code.unwrap(), "codigo");
        assert_eq!(hints, 1);

        let mut hints = 0;
        let quick = async { Ok("rapido".to_string()) };
        let code = wait_with_hint(quick, Duration::from_millis(30), || hints += 1).await;
        assert_eq!(code.unwrap(), "rapido");
        assert_eq!(hints, 0);
        assert!(redirect_hint("http://127.0.0.1:8898/callback").contains("8898/callback"));
    }

    /// rspotify no escribe la caché: si lo hiciera, un fallo al guardar
    /// convertiría una renovación correcta en un error.
    #[test]
    fn token_cache_is_managed_by_the_app() {
        let cfg = client_config("token.json".into());
        assert!(!cfg.token_cached);
        assert!(!cfg.token_refreshing);
        assert_eq!(cfg.cache_path, std::path::PathBuf::from("token.json"));
    }
}
