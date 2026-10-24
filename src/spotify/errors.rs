//! Errores de la API de Spotify: [`ApiError`] conserva el código HTTP para
//! que la interfaz pueda reaccionar, y cada error se traduce a un mensaje en
//! español con una pista útil según el tipo de petición que falló.

use std::time::Duration;

use anyhow::{Result, anyhow};
use rspotify::{ClientError, http::HttpError};

/// Error de la API con el código HTTP, para que la interfaz pueda reaccionar
/// (por ejemplo, esperar lo que indique `Retry-After` ante un 429).
#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
impl ApiError {
    pub fn rate_limited(retry_after: Option<Duration>) -> Self {
        ApiError {
            status: Some(429),
            retry_after,
            message: status_message(429, Kind::Query, retry_after),
        }
    }

    /// Error de una acción del reproductor con el código `status`.
    pub fn player(status: u16) -> Self {
        ApiError {
            status: Some(status),
            retry_after: None,
            message: status_message(status, Kind::Player, None),
        }
    }

    /// Error al pedir las canciones de una playlist: con el código `status`
    /// o, si es `None`, sin red.
    pub fn playlist_items(status: Option<u16>) -> Self {
        let message = match status {
            Some(code) => status_message(code, Kind::PlaylistItems, None),
            None => OFFLINE.into(),
        };
        ApiError {
            status,
            retry_after: None,
            message,
        }
    }
}

/// `true` si el error es de conexión (sin red, DNS, tiempo agotado), no una
/// respuesta de Spotify.
pub(super) fn is_network_error(err: &ClientError) -> bool {
    matches!(err, ClientError::Http(http) if matches!(http.as_ref(), HttpError::Client(_)))
}

/// Aviso cuando no se puede hablar con Spotify (sin red, DNS...).
pub(super) const OFFLINE: &str = "sin conexión con Spotify: revisa tu red (se reintentará solo)";

/// Qué tipo de petición falló: cambia el significado de algunos códigos.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Query,
    Player,
    /// Canciones de una playlist: en modo desarrollo, Spotify solo las da
    /// para las playlists del usuario o colaborativas (403 con el resto).
    PlaylistItems,
}

/// Traduce un error de la API a un mensaje en español con una pista útil.
pub(super) fn friendly(err: ClientError, kind: Kind) -> anyhow::Error {
    let ClientError::Http(http) = &err else {
        return anyhow!("{err}");
    };
    let resp = match http.as_ref() {
        HttpError::StatusCode(resp) => resp,
        HttpError::Client(e) => {
            let message = if e.is_timeout() {
                "Spotify no respondió a tiempo; se reintentará"
            } else {
                OFFLINE
            };
            return anyhow::Error::new(ApiError {
                status: None,
                retry_after: None,
                message: message.into(),
            });
        }
        #[allow(unreachable_patterns)]
        _ => return anyhow!("error de red: {err}"),
    };
    let status = resp.status().as_u16();
    let retry_after = retry_after(resp);
    let message = status_message(status, kind, retry_after);
    anyhow::Error::new(ApiError {
        status: Some(status),
        retry_after,
        message,
    })
}

/// Segundos que pide esperar la cabecera `Retry-After` de una respuesta.
pub(super) fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Respuesta de Spotify con código de error, si la hubo.
pub(super) fn error_response(err: &ClientError) -> Option<&reqwest::Response> {
    match err {
        ClientError::Http(http) => match http.as_ref() {
            HttpError::StatusCode(resp) => Some(resp),
            _ => None,
        },
        _ => None,
    }
}

fn status_message(status: u16, kind: Kind, retry_after: Option<Duration>) -> String {
    match (status, kind) {
        (401, _) => {
            "sesión caducada o inválida (ejecuta `spotify_cmd --logout` y vuelve a entrar)".into()
        }
        (403, Kind::Player) => {
            "Spotify denegó la acción (los controles requieren Spotify Premium, \
                                o la acción no está permitida ahora)"
                .into()
        }
        (403, Kind::PlaylistItems) => {
            "Spotify solo deja ver las canciones de tus playlists o de las colaborativas".into()
        }
        (403, _) => "Spotify no permite este acceso a la app (en modo desarrollo solo se \
                     ven tus propias playlists; revisa también que tu usuario esté en \
                     «User Management» de la app)"
            .into(),
        (404, Kind::Player) => {
            "no hay ningún dispositivo activo: abre Spotify en algún dispositivo \
                                o pulsa 'd' para elegir uno"
                .into()
        }
        (404, _) => "Spotify no encontró ese contenido".into(),
        (429, _) => match retry_after {
            // Solo el estado se vuelve a consultar solo: las acciones no se
            // reintentan, así que se dice cuánto esperar, sin prometer más.
            Some(d) => format!(
                "demasiadas peticiones a Spotify: espera {} s y vuelve a intentarlo",
                d.as_secs()
            ),
            None => "demasiadas peticiones a Spotify, espera unos segundos".into(),
        },
        (500..=599, _) => format!("Spotify tiene problemas ahora mismo (error {status})"),
        (code, _) => format!("Spotify respondió con el código {code}"),
    }
}

/// Traduce el error de una petición a `rspotify` con [`friendly`], según sea
/// una consulta o una acción del reproductor.
pub(super) trait Friendly<T> {
    fn friendly(self) -> Result<T>;
    fn friendly_player(self) -> Result<T>;
}

impl<T> Friendly<T> for std::result::Result<T, ClientError> {
    fn friendly(self) -> Result<T> {
        self.map_err(|e| friendly(e, Kind::Query))
    }
    fn friendly_player(self) -> Result<T> {
        self.map_err(|e| friendly(e, Kind::Player))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_messages_depend_on_request_kind() {
        assert!(status_message(404, Kind::Player, None).contains("dispositivo"));
        assert!(!status_message(404, Kind::Query, None).contains("dispositivo"));
        assert!(status_message(429, Kind::Query, Some(Duration::from_secs(7))).contains("7 s"));
    }

    /// Un 403 al pedir las canciones de una playlist ajena no culpa a la
    /// configuración de la app; el resto de códigos se explican igual.
    #[test]
    fn playlist_items_explain_foreign_playlists() {
        let foreign = status_message(403, Kind::PlaylistItems, None);
        assert!(foreign.contains("tus playlists") && !foreign.contains("User Management"));
        assert_ne!(status_message(403, Kind::Query, None), foreign);
        assert_eq!(
            status_message(404, Kind::PlaylistItems, None),
            status_message(404, Kind::Query, None)
        );
    }
}
