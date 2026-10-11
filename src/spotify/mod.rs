//! Capa fina sobre `rspotify`: convierte los modelos de la API en estructuras
//! sencillas para la interfaz y traduce los errores a mensajes legibles.
//!
//! Se reparte en submódulos, uno por responsabilidad:
//!
//! - `models`: estructuras que muestra la interfaz y su conversión desde los
//!   modelos de `rspotify`, con lectura tolerante de lo que no entiende.
//! - `errors`: errores de la API ([`ApiError`]) y su traducción a mensajes en
//!   español.
//! - `session`: renovación del token, de una en una, y guardado de la sesión.
//! - `queries`: consultas (reproducción, listas, búsqueda, dispositivos) y
//!   descarga de carátulas.
//! - `actions`: acciones de reproducción ([`Action`]) y su envío a Spotify.
//!
//! El resto de la app usa solo lo que se reexporta aquí.

mod actions;
mod errors;
mod models;
mod queries;
mod session;

use rspotify::AuthCodePkceSpotify;

pub use actions::{Action, ContextStart, perform};
pub use errors::ApiError;
pub use models::{
    ContextEntry, DeviceEntry, Playback, PlaylistEntry, SearchKind, SearchResults, TrackEntry,
};
pub use queries::{
    album_tracks, devices, download_image, playback, playlist_tracks, playlists, queue,
    recently_played, saved_tracks, search,
};
pub use session::{RefreshFailure, ensure_token, save_session, wait_for_renewal};

/// Cliente de Spotify de la app (autorización PKCE).
pub type Client = AuthCodePkceSpotify;

#[cfg(test)]
mod tls {
    /// El cliente HTTP confía también en los certificados del sistema, así que
    /// funciona detrás de proxies corporativos que reemplazan el certificado.
    /// Necesita red (se ejecuta a mano): `cargo test -- --ignored tls`.
    #[tokio::test]
    #[ignore]
    async fn https_works_behind_intercepting_proxy() {
        let client = reqwest::Client::new();
        let resp = client
            .get("https://index.crates.io/config.json")
            .send()
            .await
            .expect("la conexión TLS debe funcionar");
        assert!(resp.status().is_success());
    }
}
