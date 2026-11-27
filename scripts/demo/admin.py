#!/usr/bin/env python3
"""The enrolment server for the end-to-end demo (docs/demo.md): the
`MW_ADMIN_URL` that `just provision` (tests/hil/provision.py, `sign_by_admin`)
posts a node's certificate request to.

  POST /api/enroll   {"cn": "kd240-<16 hex>", "csr": "<PEM>"}
                     Authorization: Bearer <access token from `mwctl auth token`>
  GET  /healthz

For each request it
  1. asks the OAuth provider whether the bearer token is active (RFC 7662
     introspection, as the `admin-api` client), and takes the user's email;
  2. requires that email to be on the allow list (403 otherwise; 401 without
     a valid token);
  3. checks the CN is a node name, `kd240-` and 16 lowercase hex digits;
  4. has `mw-pki` sign the CSR as that CN with the CA key on the PKCS#11
     token (SoftHSM here, an HSM or TPM in production): the CA key never
     leaves the token and this process never sees it.
A request may carry a `request_id`: a repeat of it (a client retrying after a
dropped connection) is answered with the certificate already issued, not a
second signature; the same id with another CN or CSR is refused (409).
It answers {"certificate": PEM, "ca": PEM}. Each decision is logged with the
email and the CN, never a token, a PIN or a key.

    admin.py --port 18081 --idp http://127.0.0.1:18080/realms/demo \
             --pki-dir W/pki --mw-pki target/release/mw-pki --allow operator@demo.local
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CN = re.compile(r"kd240-[0-9a-f]{16}")
REQUEST_ID = re.compile(r"[A-Za-z0-9-]{8,64}")
SEEN_MAX = 256
SEEN: dict[str, tuple[str, str, dict]] = {}  # request_id -> (cn, csr, the answer given)
MAX_BODY = 16384
SIGN_LOCK = threading.Lock()  # one token session at a time
SEEN_LOCK = threading.Lock()

CFG: dict = {}


def log(msg: str) -> None:
    print(f"admin: {msg}", file=sys.stderr, flush=True)


def read_env(path: Path) -> dict[str, str]:
    """KEY=VALUE lines of the token's env file (scripts/zenoh-pki.sh)."""
    out = {}
    for line in path.read_text().splitlines():
        k, sep, v = line.partition("=")
        if sep and re.fullmatch(r"[A-Z0-9_]+", k):
            out[k] = v
    return out


def introspect(token: str) -> dict | None:
    """The provider's answer for `token`, or None when it cannot be asked."""
    user, pw = CFG["client"].split(":", 1)
    req = urllib.request.Request(
        CFG["idp"] + "/protocol/openid-connect/token/introspect",
        data=urllib.parse.urlencode({"token": token}).encode(),
        method="POST",
        headers={
            "Authorization": "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode(),
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.load(r)
    except (urllib.error.URLError, OSError, ValueError) as e:
        log(f"introspection failed: {e}")
        return None


def sign(csr: str, cn: str) -> tuple[str, str]:
    """The certificate for `csr` as `cn`, signed through the token; and the CA."""
    pki = Path(CFG["pki_dir"])
    env = {**os.environ, **read_env(pki / "env")}
    ca = pki / "ca" / "ca.crt"
    with tempfile.TemporaryDirectory() as d:
        csr_path, out = Path(d) / "req.csr", Path(d) / "node.crt"
        csr_path.write_text(csr)
        cmd = [CFG["mw_pki"], "client", "--key", CFG["key"], str(ca), str(csr_path), cn, str(out)]
        with SIGN_LOCK:
            r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)
        if r.returncode != 0 or not out.exists():
            raise RuntimeError((r.stderr or r.stdout).strip()[-300:])
        return out.read_text(), ca.read_text()


class Handler(BaseHTTPRequestHandler):
    server_version = "demo-admin"

    def log_message(self, format, *args):  # noqa: A002
        pass

    def reply(self, status: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):  # noqa: N802
        if self.path == "/healthz":
            return self.reply(200, {"ok": True})
        self.reply(404, {"error": "not found"})

    def do_POST(self):  # noqa: N802
        if self.path != "/api/enroll":
            return self.reply(404, {"error": "not found"})
        auth = self.headers.get("Authorization", "")
        if not auth.lower().startswith("bearer ") or len(auth) < 16:
            log("refused: no bearer token")
            return self.reply(401, {"error": "a bearer token is required"})
        info = introspect(auth[7:].strip())
        if info is None:
            return self.reply(502, {"error": "the identity provider did not answer"})
        if not info.get("active") or info.get("iss") != CFG["issuer"]:
            log("refused: token not active")
            return self.reply(401, {"error": "the token is not valid"})
        email = str(info.get("email", ""))
        if email not in CFG["allow"]:
            log(f"refused: {email} may not enrol nodes")
            return self.reply(403, {"error": "this user may not enrol nodes"})
        n = int(self.headers.get("Content-Length") or 0)
        if n <= 0 or n > MAX_BODY:
            return self.reply(400, {"error": "bad request size"})
        try:
            req = json.loads(self.rfile.read(n))
            cn, csr = str(req["cn"]), str(req["csr"])
            rid = req.get("request_id")
        except (ValueError, KeyError, AttributeError):
            return self.reply(400, {"error": "expected {cn, csr}"})
        if rid is not None and not (isinstance(rid, str) and REQUEST_ID.fullmatch(rid)):
            return self.reply(400, {"error": "request_id must be 8-64 of [A-Za-z0-9-]"})
        if not CN.fullmatch(cn):
            log(f"refused: {email} asked for CN {cn[:40]!r}")
            return self.reply(400, {"error": "the CN must be kd240-<16 lowercase hex digits>"})
        if "BEGIN CERTIFICATE REQUEST" not in csr:
            return self.reply(400, {"error": "csr is not a PEM certificate request"})
        with SEEN_LOCK:
            prior = SEEN.get(rid) if rid else None
            if prior is not None:
                if prior[:2] != (cn, csr):
                    log(f"refused: request_id reused by {email} for another request")
                    return self.reply(409, {"error": "request_id was used for another request"})
                log(f"{email} repeated the enrolment of {cn}: the first answer again")
                return self.reply(200, prior[2])
            try:
                cert, ca = sign(csr, cn)
            except (RuntimeError, subprocess.SubprocessError, OSError) as e:
                log(f"signing {cn} failed: {e}")
                return self.reply(500, {"error": "signing failed"})
            answer = {"certificate": cert, "ca": ca}
            if rid:
                if len(SEEN) >= SEEN_MAX:
                    SEEN.pop(next(iter(SEEN)))
                SEEN[rid] = (cn, csr, answer)
        log(f"{email} enrolled {cn}")
        self.reply(200, answer)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=18081)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--idp", required=True, help="the issuer URL, http://host/realms/demo")
    ap.add_argument("--client", default="admin-api:admin-secret", help="id:secret that may introspect")
    ap.add_argument("--pki-dir", required=True, help="the dir of scripts/zenoh-pki.sh (env, ca/ca.crt)")
    ap.add_argument("--mw-pki", required=True)
    ap.add_argument("--key", default="mw-zenoh-ca", help="the CA key's label on the token")
    ap.add_argument("--allow", action="append", required=True, help="an email that may enrol (repeat)")
    a = ap.parse_args()
    CFG.update(
        idp=a.idp.rstrip("/"),
        issuer=a.idp.rstrip("/"),
        client=a.client,
        pki_dir=a.pki_dir,
        mw_pki=a.mw_pki,
        key=a.key,
        allow=set(a.allow),
    )
    srv = ThreadingHTTPServer((a.host, a.port), Handler)
    log(f"listening on {a.host}:{a.port}; may enrol: {', '.join(sorted(CFG['allow']))}")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
