//! Autenticación con Spotify usando Authorization Code + PKCE.
//!
//! Solo hace falta el Client ID (no el secreto). El token se guarda en disco y
//! se renueva automáticamente, así que el navegador solo se abre la primera vez.

use std::{io, net::SocketAddr, time::Duration};

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
    let mut spotify =
        AuthCodePkceSpotify::with_config(creds, oauth, client_config(config::token_path()?));

    // 1. Token guardado de una sesión anterior.
    if let Some(token) = config::load_token(&spotify.config.cache_path, &spotify.oauth.scopes) {
        let expired = token.is_expired();
        if let Ok(mut slot) = spotify.token.lock().await {
            *slot = Some(token);
        }
        if !expired {
            return Ok(spotify);
        }
        match spotify.refresh_token().await {
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
    let url = spotify
        .get_authorize_url(None)
        .context("no se pudo generar la URL de autorización")?;

    say!("Inicia sesión en Spotify para autorizar spotify_cmd.\n");
    say!(
        "Si el navegador no se abre solo, visita esta URL:\n\n  {}\n",
        logo::green(&url)
    );

    // El puerto se reserva antes de abrir el navegador: si está ocupado, el
    // código de Spotify acabaría en otro programa.
    let listener = if manual {
        None
    } else {
        bind_redirect(spotify.get_socket_address(&spotify.oauth.redirect_uri)).await
    };
    if !manual {
        let _ = webbrowser::open(&url);
    }
    let code = match listener {
        Some(listener) => {
            let addr = listener
                .local_addr()
                .map_or_else(|_| "127.0.0.1".to_string(), |a| a.to_string());
            say!("Esperando la redirección en http://{addr} ... (Ctrl+C para cancelar)");
            let state = spotify.oauth.state.clone();
            tokio::time::timeout(AUTH_TIMEOUT, wait_for_code(listener, state))
                .await
                .map_err(|_| anyhow!("se agotó el tiempo de espera de la autorización"))??
        }
        None => {
            say!("Tras aceptar, el navegador te llevará a una página que puede no cargar.");
            say_inline!("Copia la URL completa de la barra de direcciones y pégala aquí: ");
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            spotify
                .parse_response_code(input.trim())
                .context("la URL pegada no contiene un código válido")?
        }
    };

    spotify
        .request_token(&code)
        .await
        .context("Spotify rechazó el código de autorización")?;
    save_session_or_warn(&spotify).await;
    say!("{}", logo::green("¡Sesión iniciada!"));
    Ok(spotify)
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
    if let Ok(url) = std::env::var("SPOTIFY_CMD_API_URL") {
        rs_config.api_base_url = url;
    }
    if let Ok(url) = std::env::var("SPOTIFY_CMD_AUTH_URL") {
        rs_config.auth_base_url = url;
    }
    rs_config
}

/// Guarda el token nuevo. Si no se puede, la sesión sigue en memoria: se avisa
/// de que habrá que volver a entrar la próxima vez, pero la app arranca.
async fn save_session_or_warn(spotify: &AuthCodePkceSpotify) {
    if let Err(e) = spotify::save_session(spotify).await {
        say_err!(
            "Aviso: no se pudo guardar la sesión ({e:#}); habrá que volver a entrar la próxima vez."
        );
    }
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
