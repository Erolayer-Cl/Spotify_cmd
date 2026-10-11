//! Autenticación con Spotify usando Authorization Code + PKCE.
//!
//! Solo hace falta el Client ID (no el secreto). El token se guarda en disco y
//! se renueva automáticamente, así que el navegador solo se abre la primera vez.

use std::{io, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use rspotify::{AuthCodePkceSpotify, Credentials, OAuth, prelude::*, scopes};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

use crate::config::{self, Config};
use crate::logo;
use crate::spotify;

const AUTH_TIMEOUT: Duration = Duration::from_secs(300);

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
    let mut rs_config = rspotify::Config {
        token_cached: true,
        token_refreshing: true,
        cache_path: config::token_path()?,
        ..Default::default()
    };
    // Solo para pruebas: apuntar a un servidor que imita la API de Spotify.
    if let Ok(url) = std::env::var("SPOTIFY_CMD_API_URL") {
        rs_config.api_base_url = url;
    }
    if let Ok(url) = std::env::var("SPOTIFY_CMD_AUTH_URL") {
        rs_config.auth_base_url = url;
    }
    let mut spotify = AuthCodePkceSpotify::with_config(creds, oauth, rs_config);

    // 1. Token guardado de una sesión anterior.
    if let Ok(Some(token)) = spotify.read_token_cache(true).await {
        let expired = token.is_expired();
        if let Ok(mut slot) = spotify.token.lock().await {
            *slot = Some(token);
        }
        if !expired {
            return Ok(spotify);
        }
        match spotify.refresh_token().await {
            Ok(()) => return Ok(spotify),
            // Sin red no tiene sentido abrir el navegador: se avisa y se sale.
            Err(e) if spotify::is_network_error(&e) => {
                bail!("no hay conexión con Spotify ({e}). Revisa tu red e inténtalo de nuevo")
            }
            Err(e) => say_err!(
                "No se pudo renovar el token guardado ({e}); hay que iniciar sesión de nuevo."
            ),
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

    let listen_addr = spotify.get_socket_address(&spotify.oauth.redirect_uri);
    let code = match listen_addr {
        Some(addr) if !manual => {
            let _ = webbrowser::open(&url);
            say!("Esperando la redirección en http://{addr} ... (Ctrl+C para cancelar)");
            tokio::time::timeout(AUTH_TIMEOUT, wait_for_code(&spotify, addr))
                .await
                .map_err(|_| anyhow!("se agotó el tiempo de espera de la autorización"))??
        }
        _ => {
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
    say!("{}", logo::green("¡Sesión iniciada!"));
    Ok(spotify)
}

/// Mini servidor HTTP que recibe la redirección de Spotify y extrae el código.
async fn wait_for_code(
    spotify: &AuthCodePkceSpotify,
    addr: std::net::SocketAddr,
) -> Result<String> {
    let listener = TcpListener::bind(addr).await.with_context(|| {
        format!("no se pudo escuchar en {addr}; prueba con `spotify_cmd --manual-auth`")
    })?;

    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            continue;
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let full_url = format!("http://{addr}{path}");

        if let Some(code) = spotify.parse_response_code(&full_url) {
            respond(
                &mut stream,
                "200 OK",
                "¡Listo! Ya puedes volver a la terminal.",
            )
            .await;
            return Ok(code);
        }
        if path.contains("error=") {
            respond(&mut stream, "400 Bad Request", "Autorización cancelada.").await;
            bail!("la autorización fue rechazada en el navegador");
        }
        // favicon.ico u otras peticiones: se ignoran.
        respond(&mut stream, "404 Not Found", "").await;
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

    /// Simula al navegador: primero pide el favicon y luego vuelve con el código.
    #[tokio::test]
    async fn listener_extracts_code_from_redirect() {
        let spotify = AuthCodePkceSpotify::default();
        let state = spotify.oauth.state.clone();
        let addr: std::net::SocketAddr = "127.0.0.1:38123".parse().unwrap();

        let server = tokio::spawn(async move { wait_for_code(&spotify, addr).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let send = |path: String| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut resp = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut s, &mut resp).await;
            resp
        };
        assert!(
            send("/favicon.ico".into())
                .await
                .starts_with("HTTP/1.1 404")
        );
        let ok = send(format!("/callback?code=abc123&state={state}")).await;
        assert!(ok.starts_with("HTTP/1.1 200"));

        assert_eq!(server.await.unwrap().unwrap(), "abc123");
    }
}
