"""La interfaz se adapta al agrandar y encoger la ventana (incluido el tope)."""
import atexit, datetime, fcntl, json, os, pty, select, signal, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, HERE + "/pylib")
import pyte
BIN = sys.argv[1]
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_"); log = f"{work}/r.log"; open(log, "w").close()
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
mock = subprocess.Popen([sys.executable, HERE + "/mock_spotify.py", str(port), log], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
atexit.register(mock.terminate); time.sleep(0.5)
os.makedirs(f"{work}/spotify_cmd"); open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "d"\n')
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)).isoformat()
json.dump({"access_token": "f", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": "user-read-playback-state user-modify-playback-state user-read-currently-playing playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played"}, open(f"{work}/spotify_cmd/token.json", "w"))
screen = pyte.Screen(120, 34); stream = pyte.ByteStream(screen)
pid, fd = pty.fork()
if pid == 0:
    os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1", SPOTIFY_CMD_API_URL=f"http://127.0.0.1:{port}/v1/")
    os.execv(BIN, [BIN, "--halfblocks"])
def resize(rows, cols):
    screen.resize(rows, cols); fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)); os.kill(pid, signal.SIGWINCH)
def pump(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: stream.feed(os.read(fd, 1 << 20))
            except OSError: return
results = []
def check(name, cond):
    results.append(bool(cond)); print(("  OK   " if cond else "  FALLO") + " " + name)
resize(34, 120); pump(3)
for rows, cols in [(34, 120), (20, 70), (50, 200), (24, 80)]:
    resize(rows, cols); pump(1.2)
    top, bottom_box = screen.display[0], screen.display[rows - 2]
    check(f"{cols}x{rows}: el marco ocupa todo el ancho", top.startswith("╭") and top.rstrip().endswith("╮") and len(top.rstrip()) == cols)
    check(f"{cols}x{rows}: el reproductor está abajo", bottom_box.startswith("╰") and bottom_box.rstrip().endswith("╯"))
    check(f"{cols}x{rows}: se ve la canción", "Bohemian Rhapsody" in "\n".join(screen.display))
os.write(fd, b"q"); pump(1); _, st = os.waitpid(pid, 0)
check("sale con código 0", os.waitstatus_to_exitcode(st) == 0)
print(f"{sum(results)}/{len(results)} comprobaciones correctas"); sys.exit(0 if all(results) else 1)
