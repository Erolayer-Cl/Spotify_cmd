"""El token caduca con la app abierta: debe renovarse solo y seguir funcionando.
Si Spotify rechaza la renovación, la app avisa pero no se cae."""
import atexit, datetime, fcntl, json, os, pty, select, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, HERE + "/pylib")
import pyte
BIN = sys.argv[1]
results = []
def check(name, cond):
    results.append(bool(cond)); print(("  OK   " if cond else "  FALLO") + " " + name)

def run(mode, break_cache=False):
    work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_"); log = f"{work}/requests.log"; open(log, "w").close()
    s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
    mock = subprocess.Popen([sys.executable, HERE + "/mock_spotify.py", str(port), log, mode], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); time.sleep(0.5)
    atexit.register(mock.terminate)
    os.makedirs(f"{work}/spotify_cmd")
    open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\npoll_secs = 1\n')
    scopes = ("user-read-playback-state user-modify-playback-state user-read-currently-playing "
              "playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played")
    # Caduca en 14 s; rspotify lo da por caducado 10 s antes: a los ~4 s de uso.
    exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=14)).isoformat()
    exp_epoch = int(datetime.datetime.fromisoformat(exp).timestamp())
    json.dump({"access_token": f"exp-{exp_epoch}", "expires_in": 14, "expires_at": exp, "refresh_token": "r1", "scope": scopes},
              open(f"{work}/spotify_cmd/token.json", "w"))
    screen = pyte.Screen(120, 34); stream = pyte.ByteStream(screen)
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1",
                          SPOTIFY_CMD_API_URL=f"http://127.0.0.1:{port}/v1/", SPOTIFY_CMD_AUTH_URL=f"http://127.0.0.1:{port}/")
        os.execv(BIN, [BIN, "--halfblocks"])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 120, 0, 0))
    def pump(sec):
        end = time.time() + sec
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try: stream.feed(os.read(fd, 65536))
                except OSError: return
    if break_cache:
        # Con la app abierta, el token deja de poder guardarse en disco.
        pump(1.5)
        os.remove(f"{work}/spotify_cmd/token.json"); os.mkdir(f"{work}/spotify_cmd/token.json")
        pump(7.5)
    else:
        pump(9)
    for k in ["n", " ", " "]:
        os.write(fd, k.encode()); pump(0.8)
    lines = open(log).read().splitlines()
    text = "\n".join(screen.display)
    alive = os.waitpid(pid, os.WNOHANG)[0] == 0
    os.write(fd, b"q"); pump(1)
    _, st = os.waitpid(pid, 0); mock.terminate()
    token_path = f"{work}/spotify_cmd/token.json"
    token = json.load(open(token_path)) if os.path.isfile(token_path) else {}
    return lines, text, alive, os.waitstatus_to_exitcode(st), token

print("== Renovación correcta")
lines, text, alive, code, token = run("normal")
refreshes = [l for l in lines if l.startswith("POST /api/token")]
check("renovó el token una vez (sin peticiones duplicadas)", len(refreshes) == 1)
after = lines[lines.index(refreshes[0]) + 1:] if refreshes else []
check("las peticiones siguientes usan el token nuevo", after and all("Bearer renovado" in l for l in after if l.startswith(("GET /v1", "POST /v1", "PUT /v1"))))
check("guardó el token nuevo en disco", token.get("access_token") == "renovado")
check("siguió funcionando (siguiente canción enviada)", any("POST /v1/me/player/next" in l for l in after))
check("sale con código 0", alive and code == 0)

print("== Spotify rechaza la renovación")
lines, text, alive, code, _ = run("refresh_fails")
attempts = sum(1 for l in lines if l.startswith("POST /api/token"))
print(f"     intentos de renovación en ~12 s: {attempts}")
check("intentó renovar", attempts >= 1)
check("no insiste: como mucho 2 intentos en ~12 s (espera entre reintentos)", attempts <= 2)
check("explica que hay que volver a iniciar sesión", "--logout" in text)
check("la app sigue viva y avisa", alive)
check("sale con código 0", code == 0)
print("== No se puede guardar el token en disco")
lines, text, alive, code, _ = run("normal", break_cache=True)
refreshes = [l for l in lines if l.startswith("POST /api/token")]
after = lines[lines.index(refreshes[0]) + 1:] if refreshes else []
check("renovó el token", len(refreshes) == 1)
check("sigue usando el token nuevo (en memoria)", after and all("Bearer renovado" in l for l in after if l.startswith(("GET /v1", "POST /v1", "PUT /v1"))))
check("no muestra un falso 'sesión caducada'", "caducó" not in text)
check("sale con código 0", alive and code == 0)
print(f"{sum(results)}/{len(results)} comprobaciones correctas")
sys.exit(0 if all(results) else 1)
