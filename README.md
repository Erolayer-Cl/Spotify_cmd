# spotify_cmd

Controla Spotify desde la terminal: mira qué suena (con la carátula del álbum), pasa de canción, cambia de lista y busca música.

```
        ▄▄▄████████▄▄▄
     ▄▄████████████████▄▄
   ▄██████████████████████▄
  ▄████████████████████████▄
 ████████▀▀▀▀▀▀▀▀▀▀▀▀████████
▄████                    ████▄
█████ ▄▄▄████████████▄▄▄ █████
███████▀▀▀          ▀▀▀███████
███████  ▄▄▄▄████▄▄▄▄  ███████
▀█████████▀▀▀▀▀▀▀▀▀▀█████████▀
 ███████  ▄▄▄▄▄▄▄▄▄▄  ███████
  ▀████████████████████████▀
   ▀██████████████████████▀
     ▀▀████████████████▀▀
        ▀▀▀████████▀▀▀
```

## Instalar

Necesitas [Rust](https://rustup.rs) y Spotify **Premium** (los controles lo exigen).

```bash
git clone https://github.com/erolayer-cl/spotify_cmd
cd spotify_cmd
cargo install --path .
```

## Configurar (solo la primera vez)

1. Entra en <https://developer.spotify.com/dashboard> y crea una app.
2. En **Redirect URIs** pon: `http://127.0.0.1:8888/callback`
3. Copia el **Client ID**.
4. Ejecuta `spotify_cmd`, pega el Client ID y autoriza en el navegador.

Ten Spotify abierto en algún dispositivo (móvil, PC…): esta herramienta es un mando a distancia.

## Teclas

| Tecla | Acción |
|---|---|
| `espacio` | Play / pausa |
| `n` / `p` | Siguiente / anterior |
| `←` / `→` | -10 s / +10 s |
| `/` | Buscar (`Tab` cambia el tipo) |
| `Tab` | Cambiar entre biblioteca y lista |
| `Enter` | Abrir lista / reproducir |
| `s` / `r` | Aleatorio / repetir |
| `c` | Carátula en grande |
| `d` | Elegir dispositivo |
| `?` | Todas las teclas |
| `q` | Salir |

¿Problemas? `spotify_cmd --help` (opciones `--manual-auth` para SSH, `--halfblocks` si la imagen se ve mal y `--logout`). Si no hay ningún dispositivo activo, `Enter` en una canción abre la lista de dispositivos y suena en el que elijas.

## Pruebas

```bash
cargo test                  # unas 150 pruebas: UI en todos los tamaños, miles de teclas al azar, respuestas reales de Spotify
tests/e2e/run_all.sh        # app real contra una API simulada: funcional, sin dispositivo, red caída, token, caos, velocidad, memoria y ventanas (requiere `pip install pyte`)
```
