"""Prueba de resistencia: uso continuo durante varios minutos midiendo la
memoria. Si hubiera una fuga, la memoria crecería sin parar."""
import datetime, fcntl, json, os, pty, random, select, socket, struct, subprocess, sys, tempfile, termios, time
HERE = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1]; MINUTES = float(sys.argv[2]) if len(sys.argv) > 2 else 4
work = tempfile.mkdtemp(prefix="spotify_cmd_e2e_"); log = f"{work}/requests.log"; open(log, "w").close()
s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
mock = subprocess.Popen([sys.executable, HERE + "/mock_spotify.py", str(port), log]); time.sleep(0.5)
os.makedirs(f"{work}/spotify_cmd")
open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\npoll_secs = 1\n')
scopes = ("user-read-playback-state user-modify-playback-state user-read-currently-playing "
          "playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played")
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=2)).isoformat()
json.dump({"access_token": "f", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": scopes},
          open(f"{work}/spotify_cmd/token.json", "w"))
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
            try: os.read(fd, 65536)
            except OSError: return
def rss():
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"): return int(line.split()[1])
# Ciclo de uso realista: abrir cada lista, buscar, reproducir, cambiar carátula.
cycle = ["\t", "g", "\r", "\t", "j", "\r", "\t", "j", "\r", "\t", "j", "\r", "\t", "j", "\r",
         "/", "abba", "\r", "\r", "n", "c", "c", "d", "\x1b", "+", "-", "\x1b[C"]
rng = random.Random(3)
samples = []; t0 = time.time(); next_sample = t0 + 10
while time.time() - t0 < MINUTES * 60:
    for k in cycle:
        os.write(fd, k.encode()); pump(0.15)
    if time.time() >= next_sample:
        samples.append((int(time.time() - t0), rss())); next_sample += 20
os.write(fd, b"\x1b"); pump(0.3); os.write(fd, b"q"); pump(1)
_, st = os.waitpid(pid, 0); mock.terminate()
reqs = sum(1 for _ in open(log))
print("memoria (s, KB):", " ".join(f"{t}s={kb}" for t, kb in samples))
first_half = max(kb for _, kb in samples[: len(samples) // 2])
second_half = max(kb for _, kb in samples[len(samples) // 2:])
print(f"peticiones: {reqs}, máximo 1.ª mitad: {first_half} KB, máximo 2.ª mitad: {second_half} KB, código: {os.waitstatus_to_exitcode(st)}")
stable = second_half <= first_half * 1.15
print("memoria estable (sin fugas)" if stable else "AVISO: la memoria sigue creciendo")
sys.exit(0 if stable and os.waitstatus_to_exitcode(st) == 0 else 1)
