"""Caos: la API falla al azar y la app recibe miles de teclas. No debe caerse."""
import datetime, fcntl, json, os, pty, random, select, signal, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1]; SECONDS = float(sys.argv[2]) if len(sys.argv) > 2 else 60
SEED = int(sys.argv[3]) if len(sys.argv) > 3 else 1
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_"); log = f"{work}/requests.log"; open(log, "w").close()
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
mock = subprocess.Popen([sys.executable, "-I", HERE + "/mock_spotify.py", str(port), log, "chaos"])
time.sleep(0.5)
os.makedirs(f"{work}/spotify_cmd")
open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\npoll_secs = 1\n')
scopes = ("user-read-playback-state user-modify-playback-state user-read-currently-playing "
          "playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played")
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)).isoformat()
json.dump({"access_token": "fake", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": scopes},
          open(f"{work}/spotify_cmd/token.json", "w"))
pid, fd = pty.fork()
if pid == 0:
    errf = os.open(f"{work}/stderr.txt", os.O_WRONLY | os.O_CREAT); os.dup2(errf, 2)
    os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", NO_PROXY="127.0.0.1", no_proxy="127.0.0.1",
                      SPOTIFY_CMD_API_URL=f"http://127.0.0.1:{port}/v1/")
    os.execv(BIN, [BIN, "--halfblocks"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
out = bytearray()
def pump(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.02)
        if r:
            try: out.extend(os.read(fd, 65536))
            except OSError: return
rng = random.Random(SEED)

def rss_kb():
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"): return int(line.split()[1])
    except OSError:
        return None
keys = [" ", "n", "p", "s", "r", "+", "-", "/", "d", "c", "?", "R", "a", "o", "j", "k", "g", "G", "\t", "\r", "\x1b",
        "\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D", "\x1b[5~", "\x1b[6~", "x", "/abba\r", "\x7f"]
sizes = [(30, 100), (8, 20), (2, 2), (50, 200), (30, 100)]
pump(3); rss_start = rss_kb()
end = time.time() + SECONDS; sent = 0
while time.time() < end:
    k = rng.choice(keys)
    if k == "q": continue
    os.write(fd, k.encode()); sent += 1
    if rng.random() < 0.01:
        r, c = rng.choice(sizes); fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", r, c, 0, 0)); os.kill(pid, signal.SIGWINCH)
    pump(rng.choice([0.0, 0.01, 0.05, 0.2]))
    wpid, st = os.waitpid(pid, os.WNOHANG)
    if wpid:
        print("FALLO: la app terminó sola durante el caos, código", os.waitstatus_to_exitcode(st), "última tecla", repr(k))
        print("stderr:", open(f"{work}/stderr.txt").read()[:800]); sys.exit(1)
rss_end = rss_kb()
# salir: cerrar cualquier popup y pulsar q
for k in ["\x1b", "\x1b", "q"]:
    os.write(fd, k.encode()); pump(0.5)
deadline = time.time() + 15; code = None
while time.time() < deadline:
    pump(0.1)
    wpid, st = os.waitpid(pid, os.WNOHANG)
    if wpid: code = os.waitstatus_to_exitcode(st); break
mock.terminate()
reqs = open(log).read().splitlines()
stderr = open(f"{work}/stderr.txt").read()
print(f"semilla {SEED}, {SECONDS:.0f} s: teclas enviadas: {sent}, peticiones a la API: {len(reqs)}, código de salida: {code}")
print(f"memoria (RSS): {rss_start} KB al empezar -> {rss_end} KB al terminar")
print(f"panic en stderr: {'panicked' in stderr}  restauró la terminal: {chr(27) + '[?1049l' in out.decode('utf-8', 'replace')}")
if stderr.strip(): print("stderr:", stderr[:500])
sys.exit(0 if code == 0 and "panicked" not in stderr else 1)
