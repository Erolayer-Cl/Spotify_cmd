"""Prueba funcional: spotify_cmd contra un servidor que imita la API de Spotify.

Comprueba lo que se VE en pantalla (emulador de terminal pyte) y las
peticiones que la app ENVÍA a la API."""
import atexit, datetime, fcntl, json, os, pty, select, signal, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE + "/pylib")  # opcional: pyte instalado aquí
import pyte

BIN = sys.argv[1]
ROWS, COLS = 36, 130
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_")
log = f"{work}/requests.log"; open(log, "w").close()

s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
mock = subprocess.Popen([sys.executable, "-I", HERE + "/mock_spotify.py", str(port), log], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
atexit.register(mock.terminate)
time.sleep(0.5)

os.makedirs(f"{work}/spotify_cmd")
open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\npoll_secs = 2\n')
scopes = ("user-read-playback-state user-modify-playback-state user-read-currently-playing "
          "playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played")
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)).isoformat()
json.dump({"access_token": "fake", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": scopes},
          open(f"{work}/spotify_cmd/token.json", "w"))

screen = pyte.Screen(COLS, ROWS); stream = pyte.ByteStream(screen)
pid, fd = pty.fork()
if pid == 0:
    os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", NO_PROXY="127.0.0.1,localhost",
                      no_proxy="127.0.0.1,localhost", SPOTIFY_CMD_API_URL=f"http://127.0.0.1:{port}/v1/")
    os.execv(BIN, [BIN, "--halfblocks"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

def pump(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: data = os.read(fd, 65536)
            except OSError: return
            stream.feed(data)

def text(): return "\n".join(screen.display)
def key(k, wait=0.4): os.write(fd, k.encode()); pump(wait)
def requests(): return open(log).read().splitlines()

results = []
def check(name, cond):
    results.append((name, bool(cond)))
    print(("  OK   " if cond else "  FALLO") + " " + name)

def wait_for(pred, timeout=6):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.2)
        if pred(): return True
    return False

def count(prefix): return sum(1 for r in requests() if r.startswith(prefix))

print("== Arranque")
pump(3)
check("muestra la canción que suena", wait_for(lambda: "Bohemian Rhapsody" in text()))
check("muestra las playlists en la biblioteca", wait_for(lambda: "Rock en español" in text() and "Para estudiar" in text()))
check("carga 'Tus me gusta' completa (120, en paralelo)", wait_for(lambda: "Tus me gusta · 120" in text()))
check("pidió las 3 páginas de canciones guardadas", count("GET /v1/me/tracks") >= 3)
check("descargó la carátula", count("GET /img/") >= 1)
check("estado: reproduciendo, aleatorio sí, volumen 65%", "Reproduciendo" in text() and "Aleatorio: sí" in text() and "Vol 65%" in text())

print("== Controles")
n0 = count("PUT /v1/me/player/pause"); key(" ", 1.0)
check("espacio pausa (PUT /me/player/pause)", count("PUT /v1/me/player/pause") == n0 + 1)
check("la pantalla muestra 'En pausa'", wait_for(lambda: "En pausa" in text(), 3))
key(" ", 1.0)
check("espacio de nuevo reanuda (PUT /me/player/play)", count("PUT /v1/me/player/play") >= 1)
key("n", 1.0); check("n = siguiente (POST /me/player/next)", count("POST /v1/me/player/next") == 1)
key("p", 1.0); check("p = anterior (POST /me/player/previous)", count("POST /v1/me/player/previous") == 1)
key("s", 0.8); check("s = aleatorio no (PUT shuffle?state=false)", any("shuffle?state=false" in r for r in requests()))
key("r", 0.8); check("r = repetir lista (PUT repeat?state=context)", any("repeat?state=context" in r for r in requests()))
v0 = count("PUT /v1/me/player/volume")
for _ in range(4): key("+", 0.05)
pump(1.2)
check("4 pulsaciones de + = UNA sola petición de volumen (agrupadas)", count("PUT /v1/me/player/volume") == v0 + 1)
check("volumen final 85%", any("volume_percent=85" in r for r in requests()))
for _ in range(3): key("\x1b[C", 0.05)
pump(1.2)
check("3 × → = UNA sola petición de posición", count("PUT /v1/me/player/seek") == 1)

print("== Listas")
key("\t", 0.3)   # foco en la lista (arranca en la biblioteca)
key("j", 0.2); key("\r", 1.0)
play = [r for r in requests() if r.startswith("PUT /v1/me/player/play")]
check("Enter en 'Tus me gusta' reproduce por URIs desde la canción elegida", play and '"uris"' in play[-1] and '"position":1' in play[-1])
key("a", 1.0); check("a = añadir a la cola (POST /me/player/queue)", count("POST /v1/me/player/queue") == 1)
key("\t", 0.3)                     # foco a la biblioteca
for _ in range(3): key("j", 0.1)   # Tus me gusta → Recientes → Cola → Rock en español
key("\r", 1.5)
check("abre la playlist (GET /playlists/{id}/items)", count("GET /v1/playlists/") >= 1)
check("muestra sus canciones (aunque rspotify no entienda el formato)", wait_for(lambda: "Pista de playlist 0" in text() and "Soda Stereo" in text()))
check("nunca muestra 'está vacía' para una playlist con canciones", "vacía" not in text())
key("\r", 1.0)
play = [r for r in requests() if r.startswith("PUT /v1/me/player/play")]
check("Enter en la playlist reproduce su contexto con offset", play and '"context_uri":"spotify:playlist:' in play[-1] and '"offset"' in play[-1])

print("== Buscador")
key("/", 0.3); key("queen", 0.3); key("\r", 1.5)
check("busca canciones (GET /search type=track)", any(r.startswith("GET /v1/search") and "type=track" in r for r in requests()))
check("muestra resultados", wait_for(lambda: "Don't Stop Me Now" in text()))
key("/", 0.3); key("\t", 0.2); key("\r", 1.5)
check("Tab cambia a álbumes (type=album)", any("type=album" in r for r in requests()))
check("muestra álbumes", wait_for(lambda: "Innuendo" in text()))
key("\r", 1.0)
play = [r for r in requests() if r.startswith("PUT /v1/me/player/play")]
check("Enter en un álbum reproduce el álbum", play and '"context_uri":"spotify:album:' in play[-1])

print("== Dispositivos y extras")
key("d", 1.5)
check("d lista los dispositivos", wait_for(lambda: "Mi móvil" in text(), 3))
key("j", 0.2); key("\r", 1.0)
check("Enter transfiere la reproducción (PUT /me/player)", any(r.startswith("PUT /v1/me/player ") and "dev2" in r for r in requests()))
key("c", 0.8); check("c muestra la carátula en grande", "Carátula" in text())
key("c", 0.5)
key("?", 0.5); check("? muestra la ayuda", "Ayuda" in text()); key("x", 0.3)
check("la pantalla está limpia (sin restos del banner)", "spotify_cmd · Spotify" not in text())
key("\t", 0.2); key("k", 0.1); key("\r", 1.5)
check("la cola se puede abrir", any(r.startswith("GET /v1/me/player/queue") for r in requests()))

print("== Salida")
key("\x1b", 0.3)
v0 = count("PUT /v1/me/player/volume")
t0 = time.time()
key("-", 0.0); key("q", 0.0)   # volumen y salida sin esperar al agrupado
_, st = os.waitpid(pid, 0)
code = os.waitstatus_to_exitcode(st)
check("q sale con código 0", code == 0)
check("el volumen pulsado justo antes de salir llega a Spotify", count("PUT /v1/me/player/volume") == v0 + 1)
check("y la salida sigue siendo rápida (< 2 s)", time.time() - t0 < 2)
mock.terminate()

failed = [n for n, ok in results if not ok]
print(f"\n{len(results) - len(failed)}/{len(results)} comprobaciones correctas")
if failed:
    print("\n--- pantalla final ---\n" + text())
    print("\n--- últimas peticiones ---\n" + "\n".join(requests()[-15:]))
sys.exit(1 if failed else 0)
