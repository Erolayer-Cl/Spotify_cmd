import atexit, datetime, fcntl, json, os, pty, select, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, HERE + "/pylib")  # opcional: pyte instalado aquí
import pyte
BIN = sys.argv[1]
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_"); log = f"{work}/requests.log"; open(log, "w").close()
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
mock = subprocess.Popen([sys.executable, "-I", HERE + "/mock_spotify.py", str(port), log, "big"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); time.sleep(0.5)
atexit.register(mock.terminate)
os.makedirs(f"{work}/spotify_cmd"); open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\n')
scopes = ("user-read-playback-state user-modify-playback-state user-read-currently-playing "
          "playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played")
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)).isoformat()
json.dump({"access_token": "f", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": scopes}, open(f"{work}/spotify_cmd/token.json", "w"))
screen = pyte.Screen(130, 36); stream = pyte.ByteStream(screen)
t0 = time.time()
pid, fd = pty.fork()
if pid == 0:
    os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1",
                      SPOTIFY_CMD_API_URL=f"http://127.0.0.1:{port}/v1/")
    os.execv(BIN, [BIN, "--halfblocks"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 130, 0, 0))
first_ui = loaded = None
while time.time() - t0 < 20 and loaded is None:
    r, _, _ = select.select([fd], [], [], 0.02)
    if r: stream.feed(os.read(fd, 65536))
    t = "\n".join(screen.display)
    if first_ui is None and "Biblioteca" in t: first_ui = time.time() - t0
    if "Tus me gusta · 1000" in t: loaded = time.time() - t0
# navegación por la lista de 1000: 200 teclas, medir respuesta
os.write(fd, b"\t"); time.sleep(0.2)
t1 = time.time()
for _ in range(200):
    os.write(fd, b"\x1b[6~" if _ % 2 else b"j")
end = time.time() + 3
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.02)
    if r: stream.feed(os.read(fd, 65536))
    if "Canción 999" in "\n".join(screen.display): break
nav = time.time() - t1
os.write(fd, b"q"); os.waitpid(pid, 0); mock.terminate()
pages = sum(1 for l in open(log) if l.startswith("GET /v1/me/tracks"))
print(f"interfaz visible en: {first_ui:.2f} s")
print(f"'Tus me gusta' (1000 de 5000, {pages} páginas, 80 ms c/u) cargada en: {loaded:.2f} s  (en serie serían ≥ {pages*0.08:.2f} s solo de red)")
print(f"200 teclas de navegación procesadas y dibujadas en: {nav:.2f} s")
