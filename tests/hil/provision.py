"""Find every supported board and install the current firmware on those that
need it (silent, in BOOTSEL, or on another ABI), through `just provision-<chip>`.

    just provision               # list, then provision the boards that need it
    just provision --build       # build each chip's firmware first
    just provision --all         # every board, even ones already current
    just provision --list        # only list
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import uuid

import boards as B


def row(b: B.Board) -> str:
    i = b.info or {}
    where = b.address or f"BOOTSEL {b.serial or b.usb_path}"
    if b.info is None:
        state = "BOOTSEL" if b.kind and b.kind.bootsel else f"silent ({(b.error or '')[:60]})"
    else:
        state = f"{i.get('firmware')} ABI {i.get('ABI')} payload {i.get('payload status')}"
    return f"  {b.transport:<5} {where:<22} {b.chip or (b.kind.family if b.kind else '?'):<14} {state}"


NODE_ID = re.compile(r"[0-9a-f]{16}")


def _openssl(*args: str, data: str | None = None) -> str:
    """openssl's stdout for `args` (an argument list, never a shell line)."""
    try:
        r = subprocess.run(["openssl", *args], input=data, capture_output=True, text=True, timeout=60)
    except FileNotFoundError:
        raise B.InstallError("the openssl command is needed to check the certificate before installing it") from None
    if r.returncode != 0:
        raise B.InstallError(f"openssl {args[0]}: {(r.stderr or r.stdout).strip()}")
    return r.stdout


def _one_cert(pem: object, what: str) -> str:
    """`pem` if it is exactly one PEM certificate."""
    if not isinstance(pem, str) or pem.count("-----BEGIN CERTIFICATE-----") != 1 \
            or pem.count("-----END CERTIFICATE-----") != 1:
        raise B.InstallError(f"{what} is not one PEM certificate")
    return pem.strip() + "\n"


def _sha256_of_cert(pem: str) -> str:
    """The hex SHA-256 of the certificate's DER."""
    der = subprocess.run(["openssl", "x509", "-outform", "DER"], input=pem.encode(),
                         capture_output=True, timeout=60)
    if der.returncode != 0 or not der.stdout:
        raise B.InstallError(f"openssl x509: {der.stderr.decode(errors='replace').strip()}")
    return hashlib.sha256(der.stdout).hexdigest()


def pinned_ca_sha256() -> str | None:
    """The mesh CA the board must be given, as the hex SHA-256 of its DER:
    from MW_PKI_CA (a PEM file) when set, else MW_MESH_CA_SHA256; None when
    neither is set."""
    ca = os.environ.get("MW_PKI_CA", "")
    if ca:
        try:
            with open(ca) as f:
                return _sha256_of_cert(_one_cert(f.read(), f"MW_PKI_CA ({ca})"))
        except OSError as e:
            raise B.InstallError(f"MW_PKI_CA: {e}") from None
    pin = os.environ.get("MW_MESH_CA_SHA256", "").strip().lower().replace(":", "")
    if not pin:
        return None
    if not re.fullmatch(r"[0-9a-f]{64}", pin):
        raise B.InstallError("MW_MESH_CA_SHA256 must be the 64 hex digits of the CA certificate's SHA-256 (DER)")
    return pin


def check_chain(leaf: object, ca: object, csr: str, cn: str, pin: str) -> str:
    """The chain (leaf, then CA) once it is what the board should get: the CA
    is the pinned one (`pin`, hex SHA-256 of its DER), the leaf is signed by
    it for client use, holds the CSR's key, and its subject is exactly
    CN=`cn`. The board itself checks only that the leaf matches its key and
    the CA that came with it, and then trusts that CA, so this is where the
    trust root is checked."""
    leaf = _one_cert(leaf, "the certificate")
    ca = _one_cert(ca, "the CA certificate")
    if _sha256_of_cert(ca) != pin:
        raise B.InstallError("the CA returned is not the pinned mesh CA (MW_PKI_CA / MW_MESH_CA_SHA256)")
    with tempfile.TemporaryDirectory() as d:
        ca_path, leaf_path = os.path.join(d, "ca.pem"), os.path.join(d, "leaf.pem")
        with open(ca_path, "w") as f:
            f.write(ca)
        with open(leaf_path, "w") as f:
            f.write(leaf)
        _openssl("verify", "-purpose", "sslclient", "-CAfile", ca_path, leaf_path)
    leaf_key = _openssl("x509", "-noout", "-pubkey", data=leaf).split()
    csr_key = _openssl("req", "-noout", "-pubkey", data=csr).split()
    if leaf_key != csr_key:
        raise B.InstallError("the certificate is not for the board's key (it differs from the CSR's)")
    subject = _openssl("x509", "-noout", "-subject", "-nameopt", "RFC2253", data=leaf).strip()
    if subject != f"subject=CN={cn}":
        raise B.InstallError(f"the certificate's subject is '{subject}', not CN={cn}")
    return leaf + ca


def sign_locally(csr: str, cn: str) -> str:
    """The chain (leaf, then CA) from the operator's own token: mw-pki signs
    with the CA key on it (MW_PKI_CA, MW_PKI_KEY, the PIN in MW_PKI_PIN).
    The result is checked against MW_PKI_CA (check_chain) before use."""
    ca = os.environ.get("MW_PKI_CA", "")
    label = os.environ.get("MW_PKI_KEY", "")
    if not (ca and label):
        raise B.NotInstalled("signing here needs MW_PKI_CA and MW_PKI_KEY (or set MW_ADMIN_URL)")
    pin = pinned_ca_sha256()
    assert pin is not None  # MW_PKI_CA is set
    with tempfile.TemporaryDirectory() as d:
        csr_path = os.path.join(d, "board.csr")
        cert_path = os.path.join(d, "board.crt")
        with open(csr_path, "w") as f:
            f.write(csr)
        mw_pki = os.environ.get("MW_PKI_BIN", "mw-pki")
        cmd = [mw_pki, "client", "--key", label, ca, csr_path, cn, cert_path]
        sign = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if sign.returncode != 0:
            raise B.InstallError(f"mw-pki: {(sign.stderr or sign.stdout).strip()}")
        with open(cert_path) as f:
            leaf = f.read()
        with open(ca) as f:
            ca_pem = f.read()
    return check_chain(leaf, ca_pem, csr, cn, pin)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Never follows a redirect: it would carry the bearer token elsewhere.
    Any 3xx then surfaces as an HTTPError."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        return None


def admin_url_ok(admin: str) -> bool:
    """`https://<host>`, or plain `http://` to localhost or 127.0.0.1 only;
    never with userinfo (`user@`). Raises ValueError for a malformed URL."""
    u = urllib.parse.urlsplit(admin)
    _ = u.port  # raises ValueError for a bad port
    if "@" in u.netloc or u.username is not None or u.password is not None or not u.hostname:
        return False
    if u.scheme == "https":
        return True
    return u.scheme == "http" and u.hostname in ("localhost", "127.0.0.1")


def sign_by_admin(csr: str, cn: str, mw: B.Mwctl, admin: str) -> str:
    """The chain (leaf, then CA) from the admin server at `admin`, which signs
    with the CA key on its TPM (through mw-pki, the SecureChip trait). The
    request carries the operator's bearer token from `mwctl auth token`
    (MW_AUTH_ISSUER); the token is never printed or logged. Redirects are
    refused. The answer must be the pinned mesh CA (MW_PKI_CA or
    MW_MESH_CA_SHA256; without either nothing is enrolled) and a leaf for the
    CSR's key named `cn` (check_chain).

    Each call sends one random request_id: a connection error is retried
    once with the same id, so the server answers with what it already issued
    instead of signing twice."""
    admin = admin.rstrip("/")
    try:
        ok = admin_url_ok(admin)
    except ValueError as e:
        raise B.InstallError(f"MW_ADMIN_URL is not a valid URL: {e}") from None
    if not ok:
        raise B.InstallError(
            f"MW_ADMIN_URL must be https://<host> (plain http only to localhost or 127.0.0.1, no user@), not {admin}")
    pin = pinned_ca_sha256()
    if pin is None:
        raise B.InstallError(
            "refusing to enrol through the admin server without a pinned mesh CA: "
            "set MW_PKI_CA (the CA certificate) or MW_MESH_CA_SHA256 (its SHA-256, DER)")
    tok = subprocess.run([mw.exe, "auth", "token"], capture_output=True, text=True, timeout=60)
    if tok.returncode != 0:
        raise B.InstallError(f"no sign-in ({tok.stderr.strip()}); run: {mw.exe} auth login")
    body = json.dumps({"cn": cn, "csr": csr, "request_id": str(uuid.uuid4())}).encode()
    opener = urllib.request.build_opener(_NoRedirect)
    for attempt in (1, 2):
        try:
            req = urllib.request.Request(
                f"{admin}/api/enroll",
                data=body,
                method="POST",
                headers={"Authorization": f"Bearer {tok.stdout.strip()}", "Content-Type": "application/json"},
            )
            with opener.open(req, timeout=120) as resp:
                raw = resp.read()
            break
        except urllib.error.HTTPError as e:
            if 300 <= e.code < 400:
                raise B.InstallError(
                    f"the admin server answered a redirect (HTTP {e.code}); not followed: "
                    "set MW_ADMIN_URL to the final address") from None
            try:
                why = json.load(e).get("error", "")
            except (ValueError, AttributeError, OSError):
                why = ""
            raise B.InstallError(f"the admin server refused the enrolment: HTTP {e.code} {why}".rstrip()) from None
        except ValueError as e:  # http.client.InvalidURL among them
            raise B.InstallError(f"MW_ADMIN_URL is not a valid URL: {e}") from None
        except (urllib.error.URLError, OSError) as e:
            if attempt == 1:
                print(f"  the admin server did not answer ({e}); retrying once")
                continue
            raise B.InstallError(f"the admin server at {admin} did not answer: {e}") from None
    try:
        out = json.loads(raw)
    except ValueError:
        out = None
    if not isinstance(out, dict):
        raise B.InstallError("the admin server's answer is not a JSON object")
    return check_chain(out.get("certificate"), out.get("ca"), csr, cn, pin)


def enroll(b: B.Board, mw: B.Mwctl, enroll_lan: bool = False) -> str:
    """Gives a Linux board its mesh identity: it makes its key on its secure
    chip (SecureChip: a file, or a TPM through PKCS#11) and returns a CSR, a CA
    signs the CSR as kd240-<node id>, and the board takes back the chain and
    the router URL. The key never leaves the board (MESH.md).

    Needs MW_ZENOH_ROUTER (the router's locator, e.g. tls/mesh.example:443).
    With MW_ADMIN_URL set, the admin server signs (sign_by_admin: needs
    `mwctl auth login` against MW_AUTH_ISSUER first, and the mesh CA pinned
    by MW_PKI_CA or MW_MESH_CA_SHA256); otherwise the operator's token does
    (sign_locally). The node id (the CN) is the board's own claim, so only a
    board on a local link (USB) is enrolled unless `enroll_lan`
    (--enroll-lan); otherwise it is left without one, which the returned
    line says. Returns what was done; raises InstallError on failure.
    """
    router = os.environ.get("MW_ZENOH_ROUTER", "")
    admin = os.environ.get("MW_ADMIN_URL", "")
    if not router:
        raise B.NotInstalled("the mesh identity needs MW_ZENOH_ROUTER")
    info = b.info or {}
    node = info.get("node", "")
    if not node:
        raise B.InstallError("the board does not report its node id")
    if not NODE_ID.fullmatch(node):
        raise B.InstallError(f"the board's node id {node!r} is not 16 lowercase hex digits")
    if b.transport != "usb":
        if not enroll_lan:
            # Not an exception: the firmware install that follows still runs.
            return (f"not enrolled: reached over {b.transport}, and a LAN-discovered identity can be "
                    "spoofed; enrol it over USB, or pass --enroll-lan")
        print(f"  warning: enrolling kd240-{node} reached over {b.transport} (--enroll-lan): "
              "LAN-discovered identities can be spoofed")
    r = mw.run(b, "request", "sys/cert/csr", timeout=60)
    if r.returncode != 0:
        raise B.InstallError(f"no CSR from the board: {(r.stderr or r.stdout).strip()}")
    cn = f"kd240-{node}"
    chain = sign_by_admin(r.stdout, cn, mw, admin) if admin else sign_locally(r.stdout, cn)
    body = f"router={router}\n{chain}"
    r = mw.run(b, "request", "sys/cert/install", body, timeout=60)
    if r.returncode != 0:
        raise B.InstallError(f"install refused: {(r.stderr or r.stdout).strip()}")
    return f"enrolled as kd240-{node}; the node uses it after its next start"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--build", action="store_true", help="just fw-build <chip> before installing")
    ap.add_argument("--all", action="store_true", help="also boards already on this ABI")
    ap.add_argument("--list", action="store_true", help="list only")
    ap.add_argument("--no-lan", action="store_true")
    ap.add_argument("--no-mdns", action="store_true", help="LAN without DNS-SD")
    ap.add_argument("--scout-seconds", type=float, default=1.0,
                    help="how long the LAN is listened to (Zenoh scouting and DNS-SD together)")
    ap.add_argument("--env-only", action="store_true",
                    help="only the boards MW_HIL_LAN names: no USB, no scouting, no DNS-SD "
                         "(a demo or a CI run that must not touch the boards on this machine's bench or LAN)")
    ap.add_argument("--mwctl", help="mwctl binary [default: build it]")
    ap.add_argument("--enroll-lan", action="store_true",
                    help="also give mesh identities to Linux boards found on the LAN "
                         "(their node ids can be spoofed; by default only USB boards are enrolled)")
    a = ap.parse_args()
    sys.stdout.reconfigure(line_buffering=True)  # keep our lines in order with the recipes'

    mw = B.Mwctl(a.mwctl) if a.mwctl else B.build_mwctl()
    boards = B.discover(
        mw,
        usb=not a.env_only,
        lan=not a.no_lan,
        mdns=not a.no_mdns,
        scout_seconds=a.scout_seconds,
        scout=not a.env_only,
    )
    print(f"{len(boards)} board(s) found:")
    for b in boards:
        print(row(b))
    if a.list:
        return 0
    todo = [b for b in boards if a.all or B.needs_firmware(b)]
    if not todo:
        print("nothing to provision")
        return 0
    failed = 0
    built: set[str] = set()
    for b in todo:
        print(f"\n== provisioning {b.address or b.serial}")
        try:
            chip = B.chip_to_install(b)
            if (b.chip or "").startswith("linux-"):
                print(f"mesh identity: {enroll(b, mw, enroll_lan=a.enroll_lan)}")
            B.install(b, mw, build=a.build and chip not in built)
            built.add(chip or "")
            print(f"ok: {row(b).strip()}")
            if B.needs_firmware(b):
                print("  still not answering on this ABI after install")
                failed += 1
        except B.NotInstalled as e:
            print(f"skipped: {e}")
        except B.InstallError as e:
            print(f"FAILED: {e}\n  (the lines marked !!! or >>> above say what to do)")
            failed += 1
    if failed:
        print(f"\n{failed} board(s) not provisioned; after fixing, rerun: just provision")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
