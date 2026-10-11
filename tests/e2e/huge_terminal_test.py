"""Ventanas absurdamente grandes (hasta 65535×65535): la memoria debe quedar acotada."""
import fcntl, os, pty, select, signal, struct, sys, termios, time, json, datetime, tempfile
BIN = sys.argv[1]; ROWS, COLS = (int(sys.argv[2]), int(sys.argv[3])) if len(sys.argv) > 3 else (65535, 65535)
MAX_RSS_KB = 200_000
work = tempfile.mkdtemp(prefix="huge_"); os.makedirs(f"{work}/spotify_cmd")
open(f"{work}/spotify_cmd/config.toml", "w").write('client_id = "dummy"\n')
exp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=1)).isoformat()
json.dump({"access_token": "f", "expires_in": 3600, "expires_at": exp, "refresh_token": "r", "scope": "user-read-playback-state user-modify-playback-state user-read-currently-playing playlist-read-private playlist-read-collaborative user-library-read user-read-recently-played"}, open(f"{work}/spotify_cmd/token.json", "w"))
pid, fd = pty.fork()
if pid == 0:
    os.environ.update(XDG_CONFIG_HOME=work, TERM="xterm-256color", SPOTIFY_CMD_API_URL="http://127.0.0.1:9/v1/")
    os.execv(BIN, [BIN, "--halfblocks"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
def pump(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: os.read(fd, 1 << 20)
            except OSError: return
pump(3)
def rss():
    try:
        return int([l for l in open(f"/proc/{pid}/status") if l.startswith("VmRSS")][0].split()[1])
    except Exception: return None
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0)); os.kill(pid, signal.SIGWINCH)
peak = 0; t0 = time.time()
while time.time() - t0 < 8:
    pump(0.25); r = rss(); peak = max(peak, r or 0)
    if os.waitpid(pid, os.WNOHANG)[0]: print(f"{ROWS}x{COLS}: LA APP MURIÓ, pico RSS {peak} KB"); sys.exit(1)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0)); os.kill(pid, signal.SIGWINCH); pump(1)
os.write(fd, b"q"); pump(2)
w, st = os.waitpid(pid, os.WNOHANG)
ok = w and os.waitstatus_to_exitcode(st) == 0 and peak < MAX_RSS_KB
print(f"{ROWS}x{COLS}: salida={os.waitstatus_to_exitcode(st) if w else 'aún corriendo'} pico de memoria {peak} KB (tope {MAX_RSS_KB} KB) -> {'OK' if ok else 'FALLO'}")
sys.exit(0 if ok else 1)
