//! Consultas a la API de Spotify: estado de reproducción, listas (playlists,
//! álbumes, canciones guardadas, recientes y cola), búsqueda, dispositivos y
//! descarga de carátulas.
//!
//! Las listas largas se piden por páginas, varias a la vez y con un tope de
//! elementos para que las bibliotecas enormes carguen rápido.

use std::future::Future;

use anyhow::Result;
use futures::{StreamExt, TryFutureExt, TryStreamExt};
use rspotify::{
    model::{AdditionalType, AlbumId, Market, Page, PlaylistId, SearchResult, SearchType},
    prelude::*,
};

use super::Client;
use super::errors::{Friendly, Kind, friendly};
use super::models::{
    ContextEntry, DeviceEntry, Playback, PlaylistEntry, SearchKind, SearchResults, TrackEntry,
    artist_names, millis, playable_entry, playback_from_context, playlist_entry, track_entry,
};

/// Límite de resultados por búsqueda. Las apps en "Development mode" de
/// Spotify aceptan como máximo 10 desde febrero de 2026.
const SEARCH_LIMIT: u32 = 10;

pub async fn playback(sp: &Client) -> Result<Option<Playback>> {
    let types = [AdditionalType::Track, AdditionalType::Episode];
    let ctx = sp.current_playback(None, Some(&types)).await.friendly()?;
    Ok(ctx.map(playback_from_context))
}

/// Elementos por página (el máximo que acepta Spotify en estas rutas).
const PAGE_SIZE: u32 = 50;
/// Páginas que se piden a la vez.
const PARALLEL_PAGES: usize = 4;
/// Límites para que las bibliotecas enormes carguen rápido.
const MAX_PLAYLISTS: u32 = 500;
const MAX_PLAYLIST_TRACKS: u32 = 2000;
const MAX_SAVED_TRACKS: u32 = 1000;

/// Descarga una lista paginada: primero la página 0 (que trae el total) y
/// después el resto en paralelo, conservando el orden. Mucho más rápido que
/// pedirlas una detrás de otra. Cada página va con el desplazamiento con el
/// que se pidió.
async fn fetch_pages<T, F, Fut>(max_items: u32, fetch_page: F) -> Result<Vec<(u32, Page<T>)>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Page<T>>>,
    T: serde::de::DeserializeOwned,
{
    let first = fetch_page(0).await?;
    let wanted = first.total.min(max_items);

    let offsets = (PAGE_SIZE..wanted).step_by(PAGE_SIZE as usize);
    let rest: Vec<(u32, Page<T>)> = futures::stream::iter(offsets)
        .map(|offset| fetch_page(offset).map_ok(move |page| (offset, page)))
        .buffered(PARALLEL_PAGES)
        .try_collect()
        .await?;
    Ok(std::iter::once((0, first)).chain(rest).collect())
}

/// Como [`fetch_pages`], pero solo los elementos, en orden.
async fn fetch_paged<T, F, Fut>(max_items: u32, fetch_page: F) -> Result<Vec<T>>
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = Result<Page<T>>>,
    T: serde::de::DeserializeOwned,
{
    let pages = fetch_pages(max_items, fetch_page).await?;
    Ok(pages
        .into_iter()
        .flat_map(|(_, page)| page.items)
        .take(max_items as usize)
        .collect())
}

/// Los elementos de las páginas con su posición real en la lista. Spotify
/// omite los elementos nulos sin avisar: si a una página le falta alguno,
/// las posiciones de esa página no son fiables y quedan en `None`.
fn with_positions<T>(pages: Vec<(u32, Page<T>)>, max_items: u32) -> Vec<(Option<u32>, T)>
where
    T: serde::de::DeserializeOwned,
{
    pages
        .into_iter()
        .flat_map(|(offset, page)| {
            let expected = page.total.saturating_sub(offset).min(PAGE_SIZE);
            let exact = page.items.len() == expected as usize;
            page.items.into_iter().enumerate().map(move |(i, item)| {
                let position = offset.checked_add(i as u32).filter(|_| exact);
                (position, item)
            })
        })
        .take(max_items as usize)
        .collect()
}

pub async fn playlists(sp: &Client) -> Result<Vec<PlaylistEntry>> {
    let items = fetch_paged(MAX_PLAYLISTS, |offset| async move {
        sp.current_user_playlists_manual(Some(PAGE_SIZE), Some(offset))
            .await
            .friendly()
    })
    .await?;
    Ok(items.into_iter().map(playlist_entry).collect())
}

pub async fn playlist_tracks(sp: &Client, id: PlaylistId<'static>) -> Result<Vec<TrackEntry>> {
    let id = &id;
    let pages = fetch_pages(MAX_PLAYLIST_TRACKS, |offset| async move {
        sp.playlist_items_manual(
            id.as_ref(),
            None,
            Some(Market::FromToken),
            Some(PAGE_SIZE),
            Some(offset),
        )
        .await
        .map_err(|e| friendly(e, Kind::PlaylistItems))
    })
    .await?;
    // La posición se calcula antes de descartar los elementos ilegibles.
    Ok(with_positions(pages, MAX_PLAYLIST_TRACKS)
        .into_iter()
        .filter_map(|(position, i)| {
            let entry = i.item.as_ref().and_then(playable_entry)?;
            Some(TrackEntry { position, ..entry })
        })
        .collect())
}

pub async fn album_tracks(
    sp: &Client,
    id: AlbumId<'static>,
    album_name: &str,
) -> Result<Vec<TrackEntry>> {
    let items: Vec<_> = sp
        .album_track(id, Some(Market::FromToken))
        .try_collect()
        .await
        .friendly()?;
    Ok(items
        .into_iter()
        .enumerate()
        .map(|(i, t)| TrackEntry {
            artists: artist_names(&t.artists),
            album: album_name.to_string(),
            duration_ms: millis(t.duration),
            name: t.name,
            id: t.id.map(PlayableId::Track),
            position: u32::try_from(i).ok(),
        })
        .collect())
}

/// Canciones guardadas ("Tus me gusta"), las más recientes primero.
pub async fn saved_tracks(sp: &Client) -> Result<Vec<TrackEntry>> {
    let items = fetch_paged(MAX_SAVED_TRACKS, |offset| async move {
        sp.current_user_saved_tracks_manual(Some(Market::FromToken), Some(PAGE_SIZE), Some(offset))
            .await
            .friendly()
    })
    .await?;
    Ok(items.iter().map(|s| track_entry(&s.track)).collect())
}

pub async fn recently_played(sp: &Client) -> Result<Vec<TrackEntry>> {
    let page = sp
        .current_user_recently_played(Some(50), None)
        .await
        .friendly()?;
    Ok(page.items.iter().map(|h| track_entry(&h.track)).collect())
}

pub async fn queue(sp: &Client) -> Result<Vec<TrackEntry>> {
    let q = sp.current_user_queue().await.friendly()?;
    Ok(q.queue.iter().filter_map(playable_entry).collect())
}

pub async fn search(sp: &Client, query: &str, kind: SearchKind) -> Result<SearchResults> {
    let search_type = match kind {
        SearchKind::Tracks => SearchType::Track,
        SearchKind::Albums => SearchType::Album,
        SearchKind::Artists => SearchType::Artist,
        SearchKind::Playlists => SearchType::Playlist,
    };
    let result = sp
        .search(
            query,
            search_type,
            Some(Market::FromToken),
            None,
            Some(SEARCH_LIMIT),
            None,
        )
        .await
        .friendly()?;

    Ok(match result {
        SearchResult::Tracks(page) => {
            SearchResults::Tracks(page.items.iter().map(track_entry).collect())
        }
        SearchResult::Albums(page) => SearchResults::Contexts(
            page.items
                .into_iter()
                .filter_map(|a| {
                    let year = a
                        .release_date
                        .as_deref()
                        .map(|d| d.chars().take(4).collect::<String>());
                    let artists = artist_names(&a.artists);
                    Some(ContextEntry {
                        detail: match year {
                            Some(y) if !y.is_empty() => format!("{artists} · {y}"),
                            _ => artists,
                        },
                        name: a.name,
                        id: PlayContextId::Album(a.id?),
                    })
                })
                .collect(),
        ),
        SearchResult::Artists(page) => SearchResults::Contexts(
            page.items
                .into_iter()
                .map(|a| ContextEntry {
                    detail: "Artista".into(),
                    name: a.name,
                    id: PlayContextId::Artist(a.id),
                })
                .collect(),
        ),
        SearchResult::Playlists(page) => SearchResults::Contexts(
            page.items
                .into_iter()
                .map(|p| {
                    let p = playlist_entry(p);
                    ContextEntry {
                        detail: format!("de {} · {} canciones", p.owner, p.total),
                        name: p.name,
                        id: PlayContextId::Playlist(p.id),
                    }
                })
                .collect(),
        ),
        _ => SearchResults::Tracks(Vec::new()),
    })
}

pub async fn devices(sp: &Client) -> Result<Vec<DeviceEntry>> {
    let devices = sp.device().await.friendly()?;
    Ok(devices
        .into_iter()
        .map(|d| DeviceEntry {
            id: d.id,
            name: d.name,
            kind: format!("{:?}", d._type),
            active: d.is_active,
        })
        .collect())
}

/// Tamaño máximo aceptado para una carátula (las de Spotify pesan ~50 KB).
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub async fn download_image(http: &reqwest::Client, url: &str) -> Result<image::DynamicImage> {
    let response = http.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|len| len as usize > MAX_IMAGE_BYTES)
    {
        anyhow::bail!("la imagen es demasiado grande");
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("la imagen es demasiado grande");
    }
    let img = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??;
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(items: Vec<u32>, total: u32) -> Page<u32> {
        Page {
            href: String::new(),
            items,
            limit: PAGE_SIZE,
            next: None,
            offset: 0,
            previous: None,
            total,
        }
    }

    /// Simula una lista de `total` elementos numerados desde 0.
    async fn fake_page(offset: u32, total: u32) -> Result<Page<u32>> {
        let end = (offset + PAGE_SIZE).min(total);
        Ok(page((offset..end).collect(), total))
    }

    #[tokio::test]
    async fn fetch_paged_keeps_order_and_limits() {
        let all = fetch_paged(1000, |o| fake_page(o, 237)).await.unwrap();
        assert_eq!(all, (0..237).collect::<Vec<_>>());

        let capped = fetch_paged(120, |o| fake_page(o, 5000)).await.unwrap();
        assert_eq!(capped, (0..120).collect::<Vec<_>>());

        let empty = fetch_paged(1000, |o| fake_page(o, 0)).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn fetch_paged_propagates_errors() {
        let result = fetch_paged(1000, |o| async move {
            if o == 100 {
                anyhow::bail!("fallo en la página 3")
            } else {
                fake_page(o, 300).await
            }
        })
        .await;
        assert!(result.is_err());
    }

    /// Página de `items` pedida en `offset`, de una lista de `total` elementos.
    fn page_at(offset: u32, items: Vec<u32>, total: u32) -> (u32, Page<u32>) {
        (
            offset,
            Page {
                offset,
                ..page(items, total)
            },
        )
    }

    #[tokio::test]
    async fn fetch_pages_keep_the_requested_offsets() {
        let pages = fetch_pages(1000, |o| fake_page(o, 120)).await.unwrap();
        let offsets: Vec<u32> = pages.iter().map(|(o, _)| *o).collect();
        assert_eq!(offsets, vec![0, 50, 100]);
        let positions = with_positions(pages, 1000);
        assert_eq!(positions.len(), 120);
        assert!(positions.iter().all(|(p, item)| *p == Some(*item)));
    }

    /// Si Spotify omitió un elemento nulo de una página, sus posiciones dejan
    /// de ser fiables; las de las demás páginas se conservan.
    #[test]
    fn positions_of_a_page_with_missing_items_are_unknown() {
        let total = 60;
        let first: Vec<u32> = (0..50).filter(|n| *n != 7).collect();
        let pages = vec![
            page_at(0, first, total),
            page_at(50, (50..60).collect(), total),
        ];
        let positions = with_positions(pages, 1000);
        assert_eq!(positions.len(), 59);
        assert!(positions[..49].iter().all(|(p, _)| p.is_none()));
        assert_eq!(positions[49], (Some(50), 50));
        assert_eq!(positions[58], (Some(59), 59));
        // El límite se respeta.
        let capped = with_positions(vec![page_at(0, (0..50).collect(), 50)], 10);
        assert_eq!(capped.len(), 10);
    }
}
