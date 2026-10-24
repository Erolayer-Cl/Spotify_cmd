"""Sin ningún dispositivo activo: Enter en una canción abre la ventana de
dispositivos y, al elegir uno, la canción elegida suena en él."""
import atexit, datetime, fcntl, json, os, pty, select, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, HERE + "/pylib")
import pyte

BIN = sys.argv[1]
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_")
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
log = f"{work}/peticiones.log"; open(log, "w").close()
mock = subprocess.Popen([sys.executable, HERE + "/mock_spotify.py", str(port), log, "no_device"],
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
atexit.register(mock.terminate)
time.sleep(0.5)
os.makedirs(f"{work}/spotify_cmd")
open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\npoll_secs = 1\n')
scopes = ("user-read-playback-state user-modify-playback-state user-read-currently-playing "
          "playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played")
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)).isoformat()
json.dump({"access_token": "f", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": scopes},
          open(f"{work}/spotify_cmd/token.json", "w"))
screen = pyte.Screen(120, 34); stream = pyte.ByteStream(screen)
pid, fd = pty.fork()
if pid == 0:
    os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1",
                      SPOTIFY_CMD_API_URL=f"http://127.0.0.1:{port}/v1/")
    os.execv(BIN, [BIN, "--halfblocks"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 120, 0, 0))

def pump(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: stream.feed(os.read(fd, 65536))
            except OSError: return
def text(): return "\n".join(screen.display)
def key(k, wait=0.4): os.write(fd, k.encode()); pump(wait)
def requests(): return open(log).read().splitlines()
def wait_for(pred, timeout=6):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.2)
        if pred(): return True
    return False

results = []
def check(name, cond):
    results.append(bool(cond)); print(("  OK   " if cond else "  FALLO") + " " + name)

check("arranca sin nada sonando", wait_for(lambda: "Tus me gusta · 120" in text(), 8) and "Bohemian" not in text())
key("\t", 0.3); key("j", 0.2); key("\r", 1.5)
check("Enter sin dispositivo: pide reproducir sin device_id",
      any(r.startswith("PUT /v1/me/player/play ") for r in requests()))
check("abre la ventana de dispositivos", wait_for(lambda: "Mi móvil" in text(), 4))
check("explica que hay que elegir dónde", "elige dónde reproducir" in text())
key("j", 0.3); key("\r", 1.5)
play = [r for r in requests() if r.startswith("PUT /v1/me/player/play?device_id=dev2")]
check("al elegir, la canción elegida suena en ese dispositivo", play and '"position":1' in play[-1])
check("no hace una transferencia sin canción", not any(r.startswith("PUT /v1/me/player ") for r in requests()))
check("muestra la canción que ahora suena", wait_for(lambda: "Reproduciendo" in text() and "Mi móvil" in text(), 6))

key("d", 1.0); key("\r", 1.0)
check("después, 'd' + Enter solo transfiere", wait_for(lambda: any(r.startswith("PUT /v1/me/player ") for r in requests()), 3))
key("q", 1.0)
_, st = os.waitpid(pid, 0)
check("sale con código 0", os.waitstatus_to_exitcode(st) == 0)
print(f"{sum(results)}/{len(results)} comprobaciones correctas")
sys.exit(0 if all(results) else 1)
