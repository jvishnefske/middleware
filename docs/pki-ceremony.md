# Root of trust for the Zenoh mesh: provisioning and the M-of-N ceremony

**Status: the CI procedure is implemented and runs on every push
(`scripts/zenoh-e2e.sh`, job `zenoh-mtls`). The production ceremony in section 4
is a procedure for a production HSM; the hardware is not chosen in this
repository.**

This is the mesh's own certificate authority: the one that signs router and
device certificates for mutual TLS (`MESH.md` in `homelab-flux-mesh`). It is
separate from the release root (the `mw-release` key that signs software, design.md
UP1–UP3) and from the NATS edge CA. The keys do not share a token.

## 1. What is trusted, and by whom

| Key | Signs | Lives on | Who may use it |
|---|---|---|---|
| Mesh root CA (`mw-zenoh-ca` in CI, `mesh-ca` in production) | router and device certificates (`mw-pki`) | a PKCS#11 token: SoftHSM2 in CI, an HSM in production | the ceremony quorum (section 4) |
| Router certificate | the router's TLS identity | the router's host (`zenohd-tls` Secret) | `mw-pki server`, from a CSR |
| Device certificate (`kd240-<id>`) | the device's mutual-TLS identity | the device's own key, made on the device | `mw-pki client`, from a CSR |
| Operator certificate (`tool-<id>`) | an operator's mutual-TLS identity | the operator's machine or token | `mw-pki client`, from a CSR |

Rules that hold in CI and in production:

- A certificate is issued from a **CSR**. The private key is made where it will be
  used and never leaves that place; the CA never sees it.
- The CN is `kd240-<id>` or `tool-<id>` with an id of `[a-z0-9-]`. The ACL
  (`deploy/zenoh/mtls/zenohd.json5`; `homelab-flux-mesh` in production) maps the CN
  to a role. A certificate with a CN that has no role is denied by default.
- Client certificates carry clientAuth; the router's carries serverAuth and the
  name operators connect to in its SAN. Leaves are valid one year, the CA two.
- A client directory holds `ca` (the root), `cert` (the full chain: leaf, then CA)
  and `key`, with no extensions. That is the layout `mwctl --zenoh-tls-dir` and the
  KD240's `MW_ZENOH_TLS_DIR` read.

## 2. Why the CA key is on a token

`mw-pki` signs through PKCS#11 (`mwctl::pkcs11`, the signer `mwctl` uses for
releases), so the CA key is generated on the token and used only inside it. The
software path (`openssl req`/`x509` with a token key through OpenSSL 3's PKCS#11
provider) fails in `x509 -req` with "operation not supported for this keytype", so
`mw-pki` signs through rcgen's `SigningKey`, which uses the token's own ECDSA
mechanism. Its validity is set explicitly: rcgen's default (1975 to 4096) is refused
by the Zenoh client's TLS stack.

## 3. CI and development (SoftHSM2)

`scripts/zenoh-pki.sh token <dir>` creates a SoftHSM2 token in `<dir>` with a random
PIN (kept in `<dir>/env`, mode 0600) and an EC P-256 key labelled `mw-zenoh-ca`.
`scripts/zenoh-pki.sh ca <dir>` makes the root. CI runs the whole flow on every push.
**A SoftHSM token is a file, not a secure store**: it lives in the test directory and
is deleted after the run. It exercises the same code path as production; it has no
custodians.

## 4. Production: the M-of-N hardware ceremony

Proposed roles, for a quorum of **M of N** (the operator chooses the values; this
document proposes 3 of 5):

- **Custodians (N):** five people, each holding one authorization card for the HSM
  and its PIN. No custodian holds more than one card.
- **Witness:** not a custodian; signs the ceremony log.
- **Operator:** runs the commands on the ceremony machine.
- **Auditor:** reads the log afterwards and checks the hashes in section 5.

Before the day:

1. Build the ceremony machine from a read-only image of this repository at a tagged
   commit, with no network interface enabled. Record the image's SHA-256.
2. Write the commands as one script, reviewed by two custodians.
3. Factory-reset the HSM; record its firmware version.

On the day:

1. **Open.** Witness and auditor present. The log records the date, place,
   attendees, the image hash and the HSM serial.
2. **Initialise the HSM.** The custodians set the authorization so that signing
   needs **M of the N** cards, using the HSM's quorum or split-key policy as the
   vendor implements it. The security officer role is used once, then disabled.
   Record the policy.
3. **Generate the root key on the HSM.** `pkcs11-tool --keypairgen --key-type
   EC:prime256v1 --label mesh-ca --id 01`, with the key non-extractable and
   sensitive. Record its public key's SHA-256.
4. **Self-sign the root.** `mw-pki ca --key mesh-ca <out> <CN>`, with the validity
   term the ceremony chooses (the tool's default is two years; change the `validity`
   call for a production root). Record the certificate's SHA-256.
5. **Issue the router certificate.** From a CSR made on the router's host, signed
   with `mw-pki server` under quorum. Its SAN is the endpoint name.
6. **Check.** `openssl verify -CAfile ca.crt router/cert` and `openssl x509 -noout
   -ext basicConstraints,keyUsage` on the root. Record the output.
7. **Close.** Remove the cards from the readers, power the ceremony machine down,
   seal each custodian's card in its own bag. Each attendee initials the log.

Later issuing (devices and operators) uses the same commands, with a quorum present
for each batch. The log records each batch's CSR hashes and certificate serials.

## 5. Records, backup and revocation

- **Log.** Each ceremony produces a log: the image hash, the HSM serial and firmware,
  the attendees, each command and its output, and the published hashes. The witness
  signs it; it is kept with the cards' custody record.
- **Backup.** A second HSM under the same policy, or a key backup wrapped to the
  quorum. The restore is rehearsed, not only written down.
- **Revocation.** Until a CRL exists, revoke by removing the CN from the ACL (a
  reviewed change in `homelab-flux-mesh`) and reissue short-lived leaves.
  `close_link_on_expiration` drops a link when its certificate expires.

## 6. Not done

- **The quorum is the HSM's policy, not `mw-pki`'s.** `mw-pki` calls `C_Sign` after
  login and cannot enforce M of N itself. The HSM is not yet chosen.
- **No CRL or OCSP.** Revocation is by ACL and expiry, as in section 5.
- **No intermediate CA.** Production may want an offline root signing an online
  issuing CA. This procedure uses one CA, as `MESH.md` does on the homelab.
- **CI has no custodians.** It proves the code path, not the custody.
