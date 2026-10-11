"""Servidor que imita la Web API de Spotify (lo justo para spotify_cmd)."""
import json, struct, sys, threading, time, zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

PORT = int(sys.argv[1]); LOG = sys.argv[2]
CHAOS = len(sys.argv) > 3 and sys.argv[3] == "chaos"
BIG = len(sys.argv) > 3 and sys.argv[3] == "big"
SAVED_TOTAL = 5000 if BIG else 120
import random; rng = random.Random(7)
BASE = f"http://127.0.0.1:{PORT}"
lock = threading.Lock()

def png(w=64, h=64):
    raw = b"".join(b"\x00" + bytes(c for x in range(w) for c in (x * 4, y * 4, 120)) for y in range(h))
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
COVER = png()

def sid(n):  # id base62 de 22 caracteres
    return (n + "0" * 22)[:22]

def artist(name):
    i = sid("art" + name.replace(" ", "")[:10])
    return {"external_urls": {}, "href": None, "id": i, "name": name, "type": "artist", "uri": f"spotify:artist:{i}"}

def album(name, art):
    i = sid("alb" + name.replace(" ", "")[:10])
    return {"album_type": "album", "artists": [artist(art)], "external_urls": {}, "href": None, "id": i,
            "images": [{"url": f"{BASE}/img/{i}.png", "width": 300, "height": 300}], "name": name,
            "release_date": "1975-10-31", "release_date_precision": "day", "type": "album", "uri": f"spotify:album:{i}"}

def track(n, name=None, art="Queen", alb="A Night at the Opera"):
    name = name or f"Canción {n}"
    i = sid(f"trk{n}")
    return {"album": album(alb, art), "artists": [artist(art)], "disc_number": 1, "duration_ms": 200000 + n * 1000,
            "explicit": False, "external_ids": {}, "external_urls": {}, "href": None, "id": i, "is_local": False,
            "name": name, "popularity": 50, "preview_url": None, "track_number": 1, "type": "track", "uri": f"spotify:track:{i}"}

def page(items, total, offset=0, limit=50):
    return {"href": "x", "items": items, "limit": limit, "next": "x" if offset + limit < total else None,
            "offset": offset, "previous": None, "total": total}

def playlist(n, name, total):
    i = sid(f"pl{n}")
    return {"collaborative": False, "external_urls": {}, "href": "x", "id": i, "images": [], "name": name,
            "owner": {"display_name": "Ignacio", "external_urls": {}, "href": "x", "id": "ignacio", "type": "user", "uri": "spotify:user:ignacio"},
            "public": True, "snapshot_id": "s", "items": {"href": "x", "total": total}, "type": "playlist", "uri": f"spotify:playlist:{i}"}

PLAYLISTS = [playlist(1, "Rock en español", 3), playlist(2, "Para estudiar", 2)]
state = {"playing": True, "track": 1, "shuffle": True, "repeat": "off", "volume": 65, "progress": 83000}

def device(n, name, active):
    return {"id": f"dev{n}", "is_active": active, "is_private_session": False, "is_restricted": False,
            "name": name, "type": "Computer" if n == 1 else "Smartphone", "volume_percent": state["volume"]}

def playback():
    t = track(state["track"], "Bohemian Rhapsody" if state["track"] == 1 else None)
    return {"device": device(1, "Mi portátil", True), "repeat_state": state["repeat"], "shuffle_state": state["shuffle"],
            "context": None, "timestamp": int(time.time() * 1000), "progress_ms": state["progress"],
            "is_playing": state["playing"], "item": t, "currently_playing_type": "track", "actions": {"disallows": {}}}

class Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass  # la app cortó la conexión (timeout): no es un error del servidor

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send_json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def no_content(self):
        self.send_response(204); self.send_header("content-length", "0"); self.end_headers()
    def record(self, body=b""):
        with lock, open(LOG, "a") as f:
            f.write(f"{self.command} {self.path} {body.decode(errors='replace')}\n")
    def chaos(self):
        """Devuelve True si ya respondió con un fallo simulado."""
        if not CHAOS or self.path.startswith("/img/x"):
            return False
        r = rng.random()
        if r < 0.15:
            self.send_json({"error": {"status": 500, "message": "boom"}}, 500)
        elif r < 0.25:
            body = b'{"error":{"status":429}}'
            self.send_response(429); self.send_header("retry-after", "1")
            self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
        elif r < 0.35:
            body = b'{"esto no es": json valido'
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
        elif r < 0.40:
            self.send_response(200); self.send_header("content-length", "0"); self.end_headers()
        elif r < 0.44:
            time.sleep(11)  # más que el timeout de 10 s
            return False
        elif r < 0.50:
            self.connection.shutdown(2)  # corta la conexión sin responder
        elif r < 0.55:
            body = json.dumps({"items": None, "total": "muchos"}).encode()  # tipos incorrectos
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
        else:
            return False
        return True

    def do_GET(self):
        self.record()
        if self.chaos(): return
        u = urlparse(self.path); q = {k: v[0] for k, v in parse_qs(u.query).items()}
        off = int(q.get("offset", 0)); lim = int(q.get("limit", 50))
        p = u.path
        if p.startswith("/img/"):
            self.send_response(200); self.send_header("content-type", "image/png")
            self.send_header("content-length", str(len(COVER))); self.end_headers(); self.wfile.write(COVER)
        elif p == "/v1/me/player": self.send_json(playback())
        elif p == "/v1/me/playlists": self.send_json(page(PLAYLISTS[off:off + lim], len(PLAYLISTS), off, lim))
        elif p == "/v1/me/tracks":
            if BIG: time.sleep(0.08)  # latencia de red simulada
            items = [{"added_at": "2024-01-01T00:00:00Z", "track": track(n)} for n in range(off, min(off + lim, SAVED_TOTAL))]
            self.send_json(page(items, SAVED_TOTAL, off, lim))
        elif p == "/v1/me/player/recently-played":
            self.send_json({"href": "x", "items": [{"track": track(n), "played_at": "2024-01-01T00:00:00Z", "context": None} for n in range(5)],
                            "limit": 50, "next": None, "cursors": None, "total": None})
        elif p == "/v1/me/player/queue": self.send_json({"currently_playing": track(1), "queue": [track(n) for n in range(2, 6)]})
        elif p.startswith("/v1/playlists/") and p.endswith("/items"):
            items = [{"added_at": "2024-01-01T00:00:00Z", "added_by": None, "is_local": False, "item": track(200 + n, f"Pista de playlist {n}", "Soda Stereo", "Canción Animal")} for n in range(3)]
            self.send_json(page(items, 3, off, lim))
        elif p == "/v1/search":
            if q.get("type") == "album":
                self.send_json({"albums": page([album("Innuendo", "Queen"), album("Jazz", "Queen")], 2)})
            else:
                self.send_json({"tracks": page([track(300, "Don't Stop Me Now"), track(301, "Somebody to Love")], 2)})
        elif p == "/v1/me/player/devices": self.send_json({"devices": [device(1, "Mi portátil", True), device(2, "Mi móvil", False)]})
        else: self.send_json({"error": {"status": 404, "message": "not found"}}, 404)
    def do_PUT(self): self.mutate()
    def do_POST(self): self.mutate()
    def mutate(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0) or 0))
        self.record(body)
        if self.chaos(): return
        p = urlparse(self.path).path
        if p == "/v1/me/player/pause": state["playing"] = False
        elif p == "/v1/me/player/play": state["playing"] = True
        elif p == "/v1/me/player/next": state["track"] += 1
        elif p == "/v1/me/player/previous": state["track"] = max(1, state["track"] - 1)
        self.no_content()

Server(("127.0.0.1", PORT), H).serve_forever()
