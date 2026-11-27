# The end-to-end demo

`just demo` runs the whole chain on one machine, from nothing to a client that
is on an mTLS hub with a certificate it asked for itself, and then checks what
must be refused. The same command runs in CI (`.github/workflows/demo.yml`).

```
                       OAuth device login (RFC 8628)
      operator ───────────────────────────────────────────▶ identity provider
         │  bearer token                                     (scripts/demo/idp.py)
         ▼                                                          ▲ introspect
   just provision ── CSR ◀── the node ──── ▶ enrolment server ──────┘
         │                (its own key)      (scripts/demo/admin.py)
         │ install                                  │ mw-pki: sign
         ▼                                          ▼
   QEMU client ◀──── chain ────────────────  CA key on a SoftHSM token
   (x86_64 base image)                         (never leaves the token)
         │ mTLS, CN kd240-<id>
         ▼
   Zenoh hub (zenohd, default-deny rules by CN) ◀── mTLS, CN tool-demo ── operator
```

## Run it

```sh
just demo           # everything, then tears down; exit status 0 = every check passed
just demo-up        # or step by step, leaving things up to look at:
just demo-provision
just demo-verify
just demo-down
```

Needs `docker` (the hub, `eclipse/zenoh:1.10.1`), `softhsm2-util` and `openssl`
(the root of trust), `nix` (builds the client VM, `.#x86-demo-vm`), `python3`,
`curl`, and `/dev/kvm` for speed (QEMU falls back to emulation without it, and
the boot takes minutes instead of seconds). The first run builds the VM, a Rust
build: tens of minutes, then cached. Everything lives in `target/demo`
(`DEMO_DIR` moves it); the ports are `DEMO_HUB_PORT`, `DEMO_NODE_PORT`,
`DEMO_IDP_PORT` and `DEMO_ADMIN_PORT`. `DEMO_KEEP=1 just demo` leaves it up.

## What happens

1. **Root of trust.** `scripts/zenoh-pki.sh` makes a SoftHSM token, generates
   the CA key *on it*, and issues the hub's and the operator's certificates
   through `mw-pki`, which signs through the token. The CA key is never in a file.
2. **The hub.** `zenohd` in docker with mutual TLS and the access rules of
   `deploy/zenoh/mtls/zenohd.json5` (default deny, a role per certificate CN),
   with this demo's CNs filled in.
3. **OAuth.** `idp.py` is a small stand-in for Keycloak: discovery, the device
   grant, refresh, revocation and introspection. `admin.py` is the enrolment
   server (`MW_ADMIN_URL`): it asks the provider whether the bearer token is
   active, requires the user to be an allowed enroller, checks the CN is a node
   name, and has `mw-pki` sign the CSR with the token-held CA key. A repeated
   `request_id` gets the certificate already issued, not a second one.
4. **The client.** `nix/demo-vm.nix`: the x86_64 base image (`nix/x86-base.nix`)
   as a QEMU VM. It boots with a node and no certificate, on its own loopback
   router, reachable only through QEMU's forwarded port.
5. **Provisioning.** The operator signs in with `mwctl auth login` (the code is
   approved the way a browser would, as `operator@demo.local`). Then
   `just provision --env-only --enroll-lan` asks the node for a CSR
   (`sys/cert/csr`; the node made the key itself and keeps it), gets a signature
   from the enrolment server, and installs the chain (`sys/cert/install`). The
   node validates the chain against its own key and CN, writes it, restarts into
   the hub's address, and connects with its certificate.
6. **Verification.** The operator, with its own certificate, reads the node's
   `sys/info` through the hub: the hub's rules let a tool query a node's system
   keys, and the node answers the ones its gateway allows.

## What is checked, and what is refused

| Check | Why it matters |
|---|---|
| the node is not on the hub before enrolment | no certificate, no access |
| the node's enrolled CN is `kd240-<id>`, issued by the token's CA | the hub's rules key on it |
| the operator reaches the node through the hub | mTLS end to end, both ways |
| plain TCP to the hub's TLS port is refused | no anonymous access |
| a certificate from another CA with the right CN is refused | the CA, not the name, is the root of trust |
| `sys/trust/claim` through the hub gets no reply | the hub's `deny-claim` rule, even for the operator |
| enrolment with no sign-in is `401` | the enrolment server needs a token |
| enrolment by a signed-in user who may not enrol is `403` | authorisation, not only authentication |
| a wrong password at the provider is refused | the provider checks the user |

## How it maps to production

| Demo | Production |
|---|---|
| SoftHSM token | an HSM or a TPM (`docs/pki-ceremony.md`) |
| `idp.py` | Keycloak or another OpenID Connect provider (`MW_AUTH_ISSUER`) |
| `admin.py` | the admin server behind `MW_ADMIN_URL` (same `/api/enroll`) |
| QEMU VM, file key | a board; on x86 with `mw.x86.tpm.enable` the key is in the TPM |
| the CA pinned by `MW_PKI_CA` | the same, or `MW_MESH_CA_SHA256` |

## Limits, stated plainly

- The identity provider and the enrolment server are for the demo: users and
  secrets are fixed in the source, tokens are in memory, plain `http` on
  localhost. They exist to exercise the real client side (`mwctl auth`,
  `provision.py`, `mw-pki`), not to run a service.
- `--enroll-lan`: a node's id is its own claim, so `just provision` enrols only
  USB-attached boards unless told otherwise. The demo names its one node
  explicitly and reaches it over QEMU's forwarded port on localhost.
- A key in a TPM can enrol a node but cannot yet authenticate its own mTLS
  session (the Zenoh client reads its key from a file), so the demo's node uses a
  file key.
- The hub forwards between a client and a peer, not between two peers (Zenoh
  1.10 regions), so the operator's session is a client (`mwctl --zenoh tls/...`).

## Files

`scripts/demo/demo.sh` (the orchestration), `idp.py`, `admin.py`;
`nix/demo-vm.nix`, `nix/x86-base.nix`; `scripts/zenoh-pki.sh`;
`deploy/zenoh/mtls/zenohd.json5`; `tests/hil/provision.py` (`--env-only`);
`.github/workflows/demo.yml`. Logs: `target/demo/log/` (`vm.log` is the node's
own console, `provision.out` the provisioning run, `admin.log` who enrolled whom).
