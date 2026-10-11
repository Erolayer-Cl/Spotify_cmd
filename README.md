# spotify_cmd

Cliente de **Spotify para la terminal**, escrito en Rust.

```
╭ Biblioteca · 12 playlists ╮╭ Tus me gusta · 250 ──────────────────────────────────────────╮
│▌♥ Tus me gusta            ││ #    Título                 Artista            Álbum        Dur. │
│ ◷ Recientes               ││                                                                 │
│ ≡ Cola                    ││ ♪    Bohemian Rhapsody      Queen              A Night at…  5:55 │
│ ♫ Éxitos de hoy 50        ││▌2    Billie Jean            Michael Jackson    Thriller     4:54 │
╰───────────────────────────╯╰─────────────────────────────────────────────────────────────────╯
╭ Reproduciendo ────────────────────────────────────────────────────────────────────────────────╮
│▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   Bohemian Rhapsody                                                         │
│▄▄▄▄ carátula ▄▄▄▄   Queen                                                                     │
│▄▄▄▄ del álbum ▄▄▄   A Night at the Opera                                                      │
│▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   ▶ Reproduciendo   ⇄ Aleatorio: sí   ↻ Repetir: lista   Vol 65%            │
│▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄   1:23 / 5:55  ━━━━━━━━━━━━━━━━━━━━━━━━──────────────────────────────────────│
╰───────────────────────────────────────────────────────────────────────────────────────────────╯
```

## Funciones

- **Lo que suena ahora**: título, artista, álbum, dispositivo, volumen y barra de progreso en tiempo real.
- **Carátula del álbum** dentro de la terminal. Se usa el mejor protocolo gráfico disponible
  (Kitty, Sixel o iTerm2: WezTerm, Kitty, Ghostty, iTerm2, foot, Konsole…) y, si la terminal
  no tiene ninguno, se dibuja con bloques de color (funciona en cualquier terminal).
  Pulsa `c` para verla en grande.
- **Controles**: reproducir/pausar, canción **siguiente / anterior**, adelantar/retroceder 10 s,
  volumen, aleatorio y modo de repetición (no → lista → canción).
- **Cambiar de lista**: tus canciones guardadas, reproducidas recientemente, la cola y todas tus
  playlists. Al reproducir desde una playlist, siguiente/anterior siguen el orden de esa lista.
- **Buscador** de canciones, álbumes, artistas y playlists (`/`, y `Tab` para cambiar el tipo).
- **Cola**: añade cualquier canción con `a`.
- **Dispositivos**: elige dónde suena la música (móvil, PC, altavoz…) con `d`.

## Requisitos

- [Rust](https://rustup.rs) 1.85 o superior.
- Una cuenta de Spotify. **Los controles de reproducción requieren Spotify Premium** (es una
  limitación de la API de Spotify); con una cuenta gratuita puedes ver lo que suena, tus listas
  y buscar.
- Spotify abierto en algún dispositivo (la app de escritorio, el móvil, la web…). Esta
  herramienta es un *mando a distancia*: no reproduce el audio por sí misma.

## Instalación

```bash
git clone https://github.com/erolayer-cl/spotify_cmd
cd spotify_cmd
cargo install --path .
```

O bien ejecútalo directamente con `cargo run --release`.

## Configuración (solo la primera vez)

Spotify exige que cada herramienta tenga su propia "app" de desarrollador. Es gratis y tarda un
minuto:

1. Entra en <https://developer.spotify.com/dashboard> e inicia sesión.
2. Pulsa **Create app**. Pon el nombre que quieras.
3. En **Redirect URIs** añade exactamente: `http://127.0.0.1:8888/callback`
4. Marca **Web API** y guarda.
5. Copia el **Client ID**.

Ejecuta `spotify_cmd`: te pedirá el Client ID, lo guardará en
`~/.config/spotify_cmd/config.toml` y abrirá el navegador para que autorices el acceso. El token
se guarda y se renueva solo, así que no tendrás que volver a iniciar sesión.

> No hace falta el *Client Secret*: se usa el flujo OAuth con PKCE.

> Las apps en *Development mode* solo pueden usarlas el propietario y los usuarios que añadas en
> la sección **User Management** del panel de Spotify.

### Archivo de configuración

```toml
# ~/.config/spotify_cmd/config.toml
client_id = "tu_client_id"
redirect_uri = "http://127.0.0.1:8888/callback"  # debe coincidir con la de tu app
poll_secs = 3                                     # cada cuánto se consulta Spotify
```

También puedes usar las variables de entorno `SPOTIFY_CLIENT_ID` y `SPOTIFY_REDIRECT_URI`.

## Uso

```
spotify_cmd [OPCIONES]

  --manual-auth   Inicia sesión pegando la URL a mano (útil por SSH)
  --halfblocks    Dibuja la carátula con caracteres (sin protocolo gráfico)
  --logout        Borra el token guardado
  -h, --help      Ayuda
```

### Atajos de teclado

| Tecla              | Acción                                          |
|--------------------|-------------------------------------------------|
| `espacio`          | Reproducir / pausar                             |
| `n` / `p`          | Canción siguiente / anterior                    |
| `←` / `→`          | Retroceder / adelantar 10 segundos              |
| `+` / `-`          | Subir / bajar volumen                           |
| `s`                | Aleatorio sí/no                                 |
| `r`                | Repetir: no → lista → canción                   |
| `/`                | Buscar (`Tab` cambia entre canciones, álbumes, artistas y playlists) |
| `Tab`              | Cambiar el foco entre la biblioteca y la lista  |
| `↑` `↓` / `j` `k`  | Moverse (`PgUp`/`PgDn`, `g`/`G` inicio/fin)     |
| `Enter`            | Abrir la lista seleccionada / reproducir        |
| `a`                | Añadir la canción a la cola                     |
| `o`                | Abrir un álbum o playlist de los resultados     |
| `c`                | Ver la carátula en grande                       |
| `d`                | Elegir dispositivo                              |
| `R` / `F5`         | Actualizar                                      |
| `?`                | Ayuda                                           |
| `q` / `Ctrl+C`     | Salir                                           |

## Solución de problemas

- **"no hay ningún dispositivo activo"**: abre Spotify en algún dispositivo y reproduce algo una
  vez, o pulsa `d` y elige uno.
- **"Spotify denegó la acción"**: los controles requieren Premium.
- **INVALID_CLIENT: Invalid redirect URI** en el navegador: la Redirect URI de tu app debe ser
  exactamente `http://127.0.0.1:8888/callback` (Spotify ya no acepta `localhost`).
- **La carátula se ve mal o descolocada** (por ejemplo dentro de tmux): usa `--halfblocks`.
- **Por SSH** el navegador no puede volver a tu máquina: usa `--manual-auth` y pega la URL a la
  que te redirige Spotify.

## Estructura del código

| Archivo          | Contenido                                                       |
|------------------|-----------------------------------------------------------------|
| `src/main.rs`    | Argumentos, arranque de la terminal y bucle de eventos          |
| `src/config.rs`  | Configuración y rutas (`~/.config/spotify_cmd`)                 |
| `src/auth.rs`    | Inicio de sesión OAuth PKCE con servidor local de redirección   |
| `src/spotify.rs` | Llamadas a la Web API (vía `rspotify`) y traducción de errores  |
| `src/app.rs`     | Estado de la aplicación y manejo de teclas/mensajes             |
| `src/ui.rs`      | Interfaz con `ratatui` y carátula con `ratatui-image`           |

Las llamadas a Spotify se hacen en tareas de `tokio` en segundo plano, así que la interfaz nunca
se bloquea; la carátula se redimensiona también fuera del hilo de dibujo.

## Licencia

MIT
