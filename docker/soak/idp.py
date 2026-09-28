#!/usr/bin/env python3
"""A fake OpenID Connect IdP for the soak, plus a webhook sink.

The apps discover it at `http://idp:9000` and talk to `/token` and
`/userinfo` from inside the network. Browsers reach `/authorize` through
the published port (`SOAK_IDP_PUBLIC_URL`).

`/authorize` takes the identity it should assert as query params
(`sub`, `email`, `email_verified`), so a test chooses who signs in.
Without them it shows a form, for a browser. Nothing here checks client
secrets or PKCE: the apps under test are the thing being checked.

Webhook sink: `POST /hook/<nonce>` counts, `GET /hooks/<nonce>` reads the
count, `POST /redirect?to=<path>` answers 302, `POST /status/<code>`
answers that status with a body that must never be stored.
"""

from __future__ import annotations

import html
import json
import os
import secrets
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse

INTERNAL = os.environ.get("SOAK_IDP_ISSUER", "http://idp:9000")
PUBLIC = os.environ.get("SOAK_IDP_PUBLIC_URL", "http://localhost:19000")

LOCK = threading.Lock()
CODES: dict[str, dict] = {}
TOKENS: dict[str, dict] = {}
HOOKS: dict[str, int] = {}


class Handler(BaseHTTPRequestHandler):
    server_version = "soak-idp/1"

    def log_message(self, fmt, *args):  # quiet: the soak greps logs for secrets
        pass

    def _send(self, status, body=b"", ctype="application/json", headers=None):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _json(self, obj, status=200):
        self._send(status, json.dumps(obj).encode())

    def _body(self) -> bytes:
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self):
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        if u.path == "/.well-known/openid-configuration":
            return self._json({
                "issuer": INTERNAL,
                "authorization_endpoint": f"{PUBLIC}/authorize",
                "token_endpoint": f"{INTERNAL}/token",
                "userinfo_endpoint": f"{INTERNAL}/userinfo",
            })
        if u.path == "/authorize":
            return self._authorize(q)
        if u.path == "/userinfo":
            tok = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
            with LOCK:
                prof = TOKENS.get(tok)
            return self._json(prof) if prof else self._json({"error": "invalid_token"}, 401)
        if u.path.startswith("/hooks/"):
            with LOCK:
                return self._json({"hits": HOOKS.get(u.path[len("/hooks/"):], 0)})
        if u.path == "/health":
            return self._json({"ok": True})
        self._json({"error": "not_found"}, 404)

    def _authorize(self, q):
        redirect_uri, state = q.get("redirect_uri", ""), q.get("state", "")
        if "sub" not in q:
            fields = "".join(
                f'<input type="hidden" name="{html.escape(k)}" value="{html.escape(v)}">'
                for k, v in q.items())
            page = (
                "<!doctype html><title>Soak IdP</title><h1>Soak IdP</h1>"
                f'<form method="get" action="/authorize">{fields}'
                '<label>sub <input name="sub"></label>'
                '<label>email <input name="email"></label>'
                '<label><input type="checkbox" name="email_verified" value="true" checked>'
                " verified</label><button>Sign in</button></form>")
            return self._send(200, page.encode(), "text/html; charset=utf-8")
        code = secrets.token_urlsafe(16)
        with LOCK:
            CODES[code] = {
                "sub": q["sub"],
                "email": q.get("email") or None,
                "email_verified": q.get("email_verified", "true") == "true",
                "name": q.get("name", q["sub"]),
            }
        # The apps build an https redirect_uri from Host; the fleet is
        # plain HTTP, so send a browser back over http.
        if os.environ.get("SOAK_IDP_DOWNGRADE_REDIRECT") == "1":
            redirect_uri = redirect_uri.replace("https://", "http://", 1)
        sep = "&" if "?" in redirect_uri else "?"
        loc = f"{redirect_uri}{sep}{urlencode({'code': code, 'state': state})}"
        self._send(302, b"", headers={"Location": loc})

    def do_POST(self):
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        body = self._body()
        if u.path == "/token":
            form = {k: v[0] for k, v in parse_qs(body.decode()).items()}
            with LOCK:
                prof = CODES.pop(form.get("code", ""), None)
                if prof is None:
                    return self._json({"error": "invalid_grant"}, 400)
                tok = secrets.token_urlsafe(24)
                TOKENS[tok] = prof
            return self._json({"access_token": tok, "token_type": "Bearer", "expires_in": 300})
        if u.path.startswith("/hook/"):
            with LOCK:
                key = u.path[len("/hook/"):]
                HOOKS[key] = HOOKS.get(key, 0) + 1
            return self._json({"ok": True})
        if u.path == "/redirect":
            return self._send(302, b"", headers={"Location": q.get("to", "/")})
        if u.path.startswith("/status/"):
            code = int(u.path[len("/status/"):] or 500)
            return self._send(code, f"BODY-SECRET-{q.get('n', '')}".encode(), "text/plain")
        self._json({"error": "not_found"}, 404)


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 9000), Handler).serve_forever()
