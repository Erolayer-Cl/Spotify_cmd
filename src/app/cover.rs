//! Carátula del álbum que suena: descarga en segundo plano (con reintento
//! si falla), preparación para el protocolo gráfico de la terminal y
//! redimensionado en otro hilo. Si el redimensionado falla, la carátula se
//! vuelve a preparar una vez; si vuelve a fallar, se muestra el logo.

use std::time::{Duration, Instant};

use anyhow::Result;
use image::DynamicImage;
use ratatui_image::{errors::Errors, picker::ProtocolType, thread::ResizeResponse};

use super::{App, CrashOrigin, Msg, Popup, spawn_guarded};
use crate::spotify;

/// Tras fallar la descarga de una carátula, espera antes de reintentarla.
const COVER_RETRY: Duration = Duration::from_secs(30);

impl App {
    /// Hay una carátula que mostrar (aunque ahora se esté redimensionando).
    pub fn has_cover(&self) -> bool {
        self.cover_image.is_some()
    }

    /// Muestra `img` como carátula.
    pub fn set_cover_image(&mut self, img: DynamicImage) {
        self.cover_rebuilds = 0;
        self.prepare_cover(img.clone());
        self.cover_image = Some(img);
    }

    /// Generación de la carátula preparada: los redimensionados que se pidan
    /// ahora la llevan en su respuesta ([`Msg::Resized`]).
    pub fn cover_generation(&self) -> u64 {
        self.cover_generation
    }

    // -----------------------------------------------------------------------
    // Descarga
    // -----------------------------------------------------------------------

    /// Sigue la carátula `url` de lo que suena: si es otra, o la anterior
    /// falló y ya toca reintentarlo, la descarga; sin carátula, la quita.
    pub(super) fn follow_cover(&mut self, url: Option<String>) {
        if !self.cover_needs_download(&url) {
            return;
        }
        self.cover_url = url.clone();
        self.cover_retry_at = None;
        match url {
            // Mientras se descarga se sigue viendo la carátula anterior.
            Some(url) => self.download_cover(url),
            None => self.clear_cover(),
        }
    }

    /// Si hay que descargar la carátula `url`: es otra, o la anterior falló y
    /// ya toca reintentarlo.
    fn cover_needs_download(&self, url: &Option<String>) -> bool {
        *url != self.cover_url || self.cover_retry_at.is_some_and(|t| Instant::now() >= t)
    }

    /// Descarga la carátula en segundo plano; responde con [`Msg::Cover`].
    fn download_cover(&self, url: String) {
        let http = self.http.clone();
        spawn_guarded(self.tx.clone(), CrashOrigin::Other, async move {
            let result = spotify::download_image(&http, &url).await;
            Msg::Cover { url, result }
        });
    }

    /// Respuesta de la descarga de la carátula `url` (si sigue siendo la
    /// actual).
    pub(super) fn on_cover(&mut self, url: String, result: Result<DynamicImage>) {
        if self.cover_url.as_deref() != Some(url.as_str()) {
            return;
        }
        match result {
            Ok(img) => self.set_cover_image(img),
            Err(e) => {
                // Mejor el logo que la carátula de la canción anterior;
                // la descarga se reintentará más tarde.
                self.clear_cover();
                self.cover_retry_at = Some(Instant::now() + COVER_RETRY);
                self.set_status(format!("No se pudo cargar la carátula: {e}"), true);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Preparación y redimensionado
    // -----------------------------------------------------------------------

    /// Prepara `img` para dibujarla; los redimensionados pendientes de la
    /// anterior quedan obsoletos.
    fn prepare_cover(&mut self, img: DynamicImage) {
        let protocol = self.picker.new_resize_protocol(img);
        self.cover.replace_protocol(protocol);
        self.cover_generation = self.cover_generation.wrapping_add(1);
    }

    /// Quita la carátula: en su lugar se ve el logo.
    fn clear_cover(&mut self) {
        self.cover_image = None;
        self.cover.empty_protocol();
        self.cover_generation = self.cover_generation.wrapping_add(1);
    }

    /// La carátula actual está fuera del protocolo, redimensionándose.
    pub(super) fn resizing_cover(&self) -> bool {
        self.has_cover() && self.cover.protocol_type().is_none()
    }

    /// Respuesta del redimensionado pedido para la generación `generation`.
    /// La de una carátula ya sustituida (o ya preparada) no cuenta: aplicar
    /// su error cancelaría el redimensionado sano de la actual.
    pub(super) fn on_resized(
        &mut self,
        generation: u64,
        result: std::result::Result<ResizeResponse, Errors>,
    ) {
        if generation != self.cover_generation || !self.resizing_cover() {
            return;
        }
        match result {
            Ok(resp) => {
                if self.cover.update_resized_protocol(resp) {
                    self.cover_rebuilds = 0;
                }
                // Se enviará en el próximo dibujado, bajo la ventana abierta.
                if self.picker.protocol_type() == ProtocolType::Kitty
                    && !matches!(self.popup, Popup::None)
                {
                    self.cover_sent_under_popup = true;
                }
            }
            Err(e) => {
                if !self.rebuild_cover() {
                    self.set_status(format!("Error al dibujar la carátula: {e}"), true);
                }
            }
        }
    }

    /// Vuelve a enviar la carátula si se envió con una ventana encima (ver
    /// `cover_sent_under_popup`). Se llama al cerrar una ventana.
    pub(super) fn resend_cover_if_hidden(&mut self) {
        if std::mem::take(&mut self.cover_sent_under_popup)
            && let Some(img) = self.cover_image.clone()
        {
            self.prepare_cover(img);
        }
    }

    /// Tras un fallo al redimensionar, la carátula se vuelve a preparar una
    /// sola vez; si vuelve a fallar sin un éxito entre medias, se quita (sale
    /// el logo). Devuelve si se pudo volver a preparar.
    pub(super) fn rebuild_cover(&mut self) -> bool {
        let img = match &self.cover_image {
            Some(img) if self.cover_rebuilds == 0 => img.clone(),
            _ => {
                self.clear_cover();
                return false;
            }
        };
        self.cover_rebuilds += 1;
        self.prepare_cover(img);
        true
    }
}

#[cfg(test)]
mod tests {
    use ratatui_image::thread::ResizeRequest;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crossterm::event::KeyCode;

    use crate::app::tests::{crash, key, now_playing, resize_error, status, test_app};
    use crate::spotify::Playback;

    fn tiny_image() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
    }

    /// Error de redimensionado de la carátula de la generación actual.
    fn encode_error(app: &App) -> Msg {
        resize_error(app.cover_generation())
    }

    /// Como al dibujar: la imagen sale del protocolo para redimensionarse.
    /// Devuelve la petición, para poder completarla.
    fn start_cover_resize(
        app: &mut App,
        rx: &mut UnboundedReceiver<ResizeRequest>,
    ) -> ResizeRequest {
        use ratatui::layout::Size;
        use ratatui_image::{Resize, ResizeEncodeRender};
        app.cover.resize_encode(&Resize::Fit(None), Size::new(8, 4));
        assert!(app.cover.protocol_type().is_none());
        rx.try_recv().expect("se pidió el redimensionado")
    }

    /// Completa `request` como el hilo de redimensionado del bucle principal.
    fn finish_resize(request: ResizeRequest, generation: u64) -> Msg {
        Msg::Resized {
            generation,
            result: request.resize_encode(),
        }
    }

    /// Si falla la descarga de la carátula nueva no se queda la anterior: se
    /// ve el logo y se reintenta pasado un tiempo, no en cada sondeo.
    #[tokio::test]
    async fn failed_cover_download_shows_the_logo_and_retries_later() {
        let (mut app, _rx) = test_app();
        // Puerto local cerrado: si la descarga llegara a lanzarse, falla ya.
        let url = "http://127.0.0.1:9/carátula.jpg".to_string();
        let with_cover = Playback {
            cover_url: Some(url.clone()),
            ..now_playing("B", true)
        };
        app.set_cover_image(tiny_image());
        app.set_playback(Some(with_cover.clone()));
        assert!(app.has_cover(), "mientras descarga se ve la anterior");

        app.on_msg(Msg::Cover {
            url: url.clone(),
            result: Err(anyhow::anyhow!("tiempo agotado")),
        });
        assert!(!app.has_cover());
        assert!(app.cover.protocol_type().is_none());

        app.set_playback(Some(with_cover.clone()));
        assert!(app.cover_retry_at.is_some(), "aún no toca reintentar");
        app.cover_retry_at = Some(Instant::now());
        app.set_playback(Some(with_cover));
        assert!(app.cover_retry_at.is_none(), "se vuelve a descargar");
        assert_eq!(app.cover_url.as_deref(), Some(url.as_str()));
    }

    /// Si falla el redimensionado de la carátula (o sufre un panic), se
    /// vuelve a preparar una vez; si falla otra vez, se muestra el logo.
    #[tokio::test]
    async fn failed_cover_resize_is_rebuilt_once() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let _ = start_cover_resize(&mut app, &mut rx);
        assert!(app.has_cover(), "redimensionando no es «sin carátula»");
        app.on_msg(encode_error(&app));
        assert!(app.cover.protocol_type().is_some(), "se vuelve a preparar");
        assert_eq!(status(&app), None);

        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert!(!app.has_cover(), "al segundo fallo, el logo");
        assert!(status(&app).is_some_and(|s| s.contains("carátula")));

        app.set_cover_image(tiny_image());
        let _ = start_cover_resize(&mut app, &mut rx);
        let origin = CrashOrigin::Resize(app.cover_generation());
        app.on_msg(crash(origin, "panic al codificar"));
        assert!(app.cover.protocol_type().is_some());
    }

    /// Con Kitty, una carátula enviada bajo una ventana se vuelve a enviar al
    /// cerrarla (la ventana pudo tapar la celda que la lleva). Sin ventana,
    /// o con otros protocolos, no se reenvía.
    #[tokio::test]
    async fn kitty_cover_sent_under_a_popup_is_sent_again() {
        let (mut app, mut rx) = test_app();
        app.picker.set_protocol_type(ProtocolType::Kitty);
        app.set_cover_image(tiny_image());
        app.on_event(key(KeyCode::Char('?')));
        let request = start_cover_resize(&mut app, &mut rx);
        app.on_msg(finish_resize(request, app.cover_generation()));
        assert!(app.cover.protocol_type().is_some(), "lista para enviarse");
        let generation = app.cover_generation();
        app.on_event(key(KeyCode::Esc));
        assert_eq!(
            app.cover_generation(),
            generation + 1,
            "se prepara otra vez"
        );
        assert!(app.cover.protocol_type().is_some());

        // Ya enviada sin ventana encima: cerrar otra ventana no la reenvía.
        let request = start_cover_resize(&mut app, &mut rx);
        app.on_msg(finish_resize(request, app.cover_generation()));
        app.on_event(key(KeyCode::Char('?')));
        let generation = app.cover_generation();
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.cover_generation(), generation);

        // Con Sixel (que se repinta entero al cerrar) tampoco.
        app.picker.set_protocol_type(ProtocolType::Sixel);
        app.on_event(key(KeyCode::Char('?')));
        let request = start_cover_resize(&mut app, &mut rx);
        app.on_msg(finish_resize(request, app.cover_generation()));
        let generation = app.cover_generation();
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.cover_generation(), generation);
    }

    /// El error tardío del redimensionado de la carátula anterior no toca la
    /// nueva: ni cancela su redimensionado ni gasta su reintento.
    #[tokio::test]
    async fn stale_resize_error_does_not_spend_the_retry() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let _old_request = start_cover_resize(&mut app, &mut rx);
        let old = app.cover_generation();

        app.set_cover_image(tiny_image());
        let request = start_cover_resize(&mut app, &mut rx);
        let current = app.cover_generation();
        app.on_msg(resize_error(old));
        assert!(
            app.cover.protocol_type().is_none(),
            "sigue redimensionándose"
        );
        assert_eq!(app.cover_rebuilds, 0);
        assert_eq!(status(&app), None);

        app.on_msg(finish_resize(request, current));
        assert!(
            app.cover.protocol_type().is_some(),
            "la respuesta sana vale"
        );
        // El reintento sigue disponible para un fallo de verdad.
        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert!(app.has_cover() && app.cover.protocol_type().is_some());
    }

    /// Un *panic* ajeno (u obsoleto) mientras se redimensiona la carátula no
    /// la vuelve a preparar: el redimensionado en curso sigue valiendo.
    #[tokio::test]
    async fn unrelated_crash_does_not_rebuild_the_cover() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let request = start_cover_resize(&mut app, &mut rx);
        let current = app.cover_generation();
        app.on_msg(crash(CrashOrigin::Other, "fallo en otra tarea"));
        app.on_msg(crash(
            CrashOrigin::Resize(current - 1),
            "redimensionado viejo",
        ));
        app.report_crash("al dibujar: fallo simulado");
        assert!(app.cover.protocol_type().is_none());
        assert_eq!(app.cover_rebuilds, 0);

        app.on_msg(finish_resize(request, current));
        assert!(app.cover.protocol_type().is_some());
    }

    /// Un redimensionado que sale bien devuelve el reintento: un fallo
    /// pasajero posterior (al cambiar el tamaño de la ventana) no quita la
    /// carátula.
    #[tokio::test]
    async fn successful_resize_restores_the_retry() {
        let (mut app, mut rx) = test_app();
        app.set_cover_image(tiny_image());
        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert_eq!(app.cover_rebuilds, 1);

        let request = start_cover_resize(&mut app, &mut rx);
        app.on_msg(finish_resize(request, app.cover_generation()));
        assert_eq!(app.cover_rebuilds, 0);

        let _ = start_cover_resize(&mut app, &mut rx);
        app.on_msg(encode_error(&app));
        assert!(app.has_cover() && app.cover.protocol_type().is_some());
        assert_eq!(status(&app), None);
    }
}
