"""La red se cae a mitad de la sesión y vuelve: la app no debe caerse y debe
recuperarse sola, sin reiniciarla."""
import atexit, datetime, fcntl, json, os, pty, select, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, HERE + "/pylib")
import pyte

BIN = sys.argv[1]
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_")
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()

def start_mock(log):
    m = subprocess.Popen([sys.executable, HERE + "/mock_spotify.py", str(port), log], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    atexit.register(m.terminate)
    time.sleep(0.5)
    return m

log1, log2 = f"{work}/antes.log", f"{work}/despues.log"
open(log1, "w").close(); open(log2, "w").close()
mock = start_mock(log1)
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
def alive(): return os.waitpid(pid, os.WNOHANG)[0] == 0
def wait_for(pred, timeout):
    end = time.time() + timeout
    while time.time() < end:
        pump(0.2)
        if pred(): return True
    return False

results = []
def check(name, cond):
    results.append(bool(cond)); print(("  OK   " if cond else "  FALLO") + " " + name)

check("con red: muestra la canción", wait_for(lambda: "Bohemian Rhapsody" in text(), 8))

mock.terminate(); mock.wait()                       # 1. se cae la red
for k in [" ", "n", "p", "\t", "j", "\r", "/", "abba", "\r", "d", "\x1b", "+", "\x1b[C"]:
    os.write(fd, k.encode()); pump(0.3)
check("sin red: avisa de que no hay conexión", wait_for(lambda: "sin conexión" in text(), 8))
pump(5)
check("sin red: la app sigue viva tras 10 s", alive())

mock = start_mock(log2)                             # 2. vuelve la red
os.write(fd, b"R"); pump(0.5)
check("al volver la red: retoma las consultas sola", wait_for(lambda: sum(1 for _ in open(log2)) >= 2, 20))
check("al volver la red: muestra la reproducción y las playlists",
      wait_for(lambda: "Bohemian Rhapsody" in text() and "Rock en español" in text(), 10))
os.write(fd, b"n"); pump(1.5)
check("al volver la red: los controles funcionan (siguiente)", any("POST /v1/me/player/next" in l for l in open(log2)))

os.write(fd, b"q"); pump(1)
_, st = os.waitpid(pid, 0)
check("sale con código 0", os.waitstatus_to_exitcode(st) == 0)
mock.terminate()
print(f"{sum(results)}/{len(results)} comprobaciones correctas")
sys.exit(0 if all(results) else 1)
