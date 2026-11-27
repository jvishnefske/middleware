#!/usr/bin/env python3
"""A small OAuth 2.0 / OpenID Connect provider for the end-to-end demo
(docs/demo.md). It is a stand-in for Keycloak: the same endpoints `mwctl auth`
uses, nothing more, and it is not for production.

  GET  /realms/demo/.well-known/openid-configuration   discovery
  POST /realms/demo/protocol/openid-connect/auth/device  RFC 8628 device authorization
  GET  /realms/demo/device                             where the user types the code
  POST /realms/demo/device                             user_code + username + password
  POST /realms/demo/protocol/openid-connect/token      device_code and refresh_token grants
  POST /realms/demo/protocol/openid-connect/revoke     RFC 7009
  POST /realms/demo/protocol/openid-connect/token/introspect   RFC 7662, for the resource
                                                       server (HTTP basic: admin-api)

Access and refresh tokens are opaque random strings, checked by introspection.
Tokens never appear in the log. Users and clients are fixed below.

    idp.py --port 18080
"""

from __future__ import annotations

import argparse
import base64
import hmac
import json
import secrets
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

REALM = "/realms/demo"
OIDC = REALM + "/protocol/openid-connect"
USERS = {  # email -> password
    "operator@demo.local": "operator-pass",
    "intruder@demo.local": "intruder-pass",
}
PUBLIC_CLIENTS = {"mwctl"}  # device login, no secret
RESOURCE_SERVERS = {"admin-api": "admin-secret"}  # may introspect
SCOPE = "openid email"
DEVICE_TTL = 300
INTERVAL = 1


class State:
    def __init__(self, access_ttl: int):
        self.lock = threading.Lock()
        self.access_ttl = access_ttl
        self.devices: dict[str, dict] = {}  # device_code -> record
        self.access: dict[str, dict] = {}
        self.refresh: dict[str, dict] = {}

    def issue(self, email: str, client: str) -> dict:
        now = time.time()
        access, refresh = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        self.access[access] = {"email": email, "client_id": client, "exp": now + self.access_ttl}
        self.refresh[refresh] = {"email": email, "client_id": client, "exp": now + 3600}
        return {
            "access_token": access,
            "refresh_token": refresh,
            "token_type": "Bearer",
            "expires_in": self.access_ttl,
            "scope": SCOPE,
        }


STATE: State
ISSUER = ""


def log(msg: str) -> None:
    print(f"idp: {msg}", file=sys.stderr, flush=True)


class Handler(BaseHTTPRequestHandler):
    server_version = "demo-idp"

    def log_message(self, format, *args):  # noqa: A002  (our own audit lines only)
        pass

    # ------------------------------------------------------------ plumbing
    def send_json(self, status: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_html(self, status: int, html: str) -> None:
        data = html.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def form(self) -> dict[str, str]:
        n = int(self.headers.get("Content-Length") or 0)
        if n > 8192:
            return {}
        raw = self.rfile.read(n).decode("utf-8", "replace")
        return {k: v[0] for k, v in parse_qs(raw).items()}

    def basic(self) -> tuple[str, str] | None:
        h = self.headers.get("Authorization", "")
        if not h.lower().startswith("basic "):
            return None
        try:
            user, _, pw = base64.b64decode(h[6:]).decode().partition(":")
        except Exception:  # noqa: BLE001
            return None
        return user, pw

    # ---------------------------------------------------------------- GET
    def do_GET(self):  # noqa: N802
        url = urlparse(self.path)
        if url.path == "/healthz":
            return self.send_json(200, {"ok": True})
        if url.path == REALM + "/.well-known/openid-configuration":
            return self.send_json(
                200,
                {
                    "issuer": ISSUER,
                    "device_authorization_endpoint": ISSUER + "/protocol/openid-connect/auth/device",
                    "token_endpoint": ISSUER + "/protocol/openid-connect/token",
                    "revocation_endpoint": ISSUER + "/protocol/openid-connect/revoke",
                    "introspection_endpoint": ISSUER + "/protocol/openid-connect/token/introspect",
                    "grant_types_supported": [
                        "urn:ietf:params:oauth:grant-type:device_code",
                        "refresh_token",
                    ],
                    "scopes_supported": SCOPE.split(),
                },
            )
        if url.path == REALM + "/device":
            code = parse_qs(url.query).get("user_code", [""])[0]
            return self.send_html(
                200,
                "<h1>demo sign-in</h1><form method=post>"
                f"code <input name=user_code value='{code[:16]}'><br>"
                "user <input name=username><br>password <input name=password type=password><br>"
                "<button>approve</button></form>",
            )
        self.send_json(404, {"error": "not_found"})

    # --------------------------------------------------------------- POST
    def do_POST(self):  # noqa: N802
        path = urlparse(self.path).path
        if path == OIDC + "/auth/device":
            return self.device_authorization()
        if path == REALM + "/device":
            return self.device_approval()
        if path == OIDC + "/token":
            return self.token()
        if path == OIDC + "/revoke":
            return self.revoke()
        if path == OIDC + "/token/introspect":
            return self.introspect()
        self.send_json(404, {"error": "not_found"})

    def device_authorization(self):
        f = self.form()
        client = f.get("client_id", "")
        if client not in PUBLIC_CLIENTS:
            return self.send_json(400, {"error": "invalid_client"})
        device = secrets.token_urlsafe(32)
        user = "-".join(secrets.token_hex(2).upper() for _ in range(2))
        with STATE.lock:
            STATE.devices[device] = {
                "user_code": user,
                "client_id": client,
                "exp": time.time() + DEVICE_TTL,
                "user": None,
                "denied": False,
            }
        base = ISSUER + "/device"
        log(f"device login started for client {client}, code {user}")
        self.send_json(
            200,
            {
                "device_code": device,
                "user_code": user,
                "verification_uri": base,
                "verification_uri_complete": f"{base}?user_code={user}",
                "expires_in": DEVICE_TTL,
                "interval": INTERVAL,
            },
        )

    def device_approval(self):
        f = self.form()
        code = f.get("user_code", "").strip().upper()
        email, password = f.get("username", ""), f.get("password", "")
        with STATE.lock:
            rec = next((r for r in STATE.devices.values() if r["user_code"] == code), None)
            if rec is None or rec["exp"] < time.time():
                return self.send_html(400, "unknown or expired code")
            expected = USERS.get(email)
            if expected is None or not hmac.compare_digest(expected, password):
                rec["denied"] = True
                log(f"sign-in refused for {email or '(none)'}")
                return self.send_html(401, "wrong user or password")
            rec["user"] = email
        log(f"code {code} approved by {email}")
        self.send_html(200, "approved: you can close this page")

    def token(self):
        f = self.form()
        grant = f.get("grant_type", "")
        client = f.get("client_id", "")
        if client not in PUBLIC_CLIENTS:
            return self.send_json(400, {"error": "invalid_client"})
        now = time.time()
        with STATE.lock:
            if grant == "urn:ietf:params:oauth:grant-type:device_code":
                rec = STATE.devices.get(f.get("device_code", ""))
                if rec is None or rec["client_id"] != client:
                    return self.send_json(400, {"error": "invalid_grant"})
                if rec["exp"] < now:
                    STATE.devices.pop(f["device_code"], None)
                    return self.send_json(400, {"error": "expired_token"})
                if rec["denied"]:
                    STATE.devices.pop(f["device_code"], None)
                    return self.send_json(400, {"error": "access_denied"})
                if rec["user"] is None:
                    return self.send_json(400, {"error": "authorization_pending"})
                STATE.devices.pop(f["device_code"], None)
                tokens = STATE.issue(rec["user"], client)
                log(f"tokens issued to {rec['user']}")
                return self.send_json(200, tokens)
            if grant == "refresh_token":
                rec = STATE.refresh.get(f.get("refresh_token", ""))
                if rec is None or rec["exp"] < now or rec["client_id"] != client:
                    return self.send_json(400, {"error": "invalid_grant"})
                tokens = STATE.issue(rec["email"], client)
                tokens.pop("refresh_token")  # the same refresh token stays valid
                log(f"access token refreshed for {rec['email']}")
                return self.send_json(200, tokens)
        self.send_json(400, {"error": "unsupported_grant_type"})

    def revoke(self):
        f = self.form()
        token = f.get("token", "")
        with STATE.lock:
            gone = STATE.refresh.pop(token, None) or STATE.access.pop(token, None)
        if gone:
            log(f"token revoked for {gone['email']}")
        self.send_json(200, {})  # RFC 7009: always 200

    def introspect(self):
        creds = self.basic()
        if creds is None or RESOURCE_SERVERS.get(creds[0]) is None or not hmac.compare_digest(
            RESOURCE_SERVERS[creds[0]], creds[1]
        ):
            return self.send_json(401, {"error": "invalid_client"})
        token = self.form().get("token", "")
        with STATE.lock:
            rec = STATE.access.get(token)
        if rec is None or rec["exp"] < time.time():
            return self.send_json(200, {"active": False})
        self.send_json(
            200,
            {
                "active": True,
                "iss": ISSUER,
                "client_id": rec["client_id"],
                "email": rec["email"],
                "scope": SCOPE,
                "exp": int(rec["exp"]),
                "token_type": "Bearer",
            },
        )


def main() -> int:
    global STATE, ISSUER
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--access-ttl", type=int, default=300, help="seconds an access token lives")
    a = ap.parse_args()
    STATE = State(a.access_ttl)
    ISSUER = f"http://{a.host}:{a.port}{REALM}"
    srv = ThreadingHTTPServer((a.host, a.port), Handler)
    log(f"issuer {ISSUER}")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
