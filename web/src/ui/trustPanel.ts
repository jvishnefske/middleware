// Trust panel of the Device tab (requirement U13): this browser's signing
// key (create, back up, restore) and the connected device's pinned key
// (state, fingerprint, claim).
//
// Decisions come from the pure functions in `trust/wire.ts`
// (`relateKeys`, `canClaim`); this file only renders them and runs the
// dialogs. The key itself is handled by `trust/crypto.ts` and persisted by
// `trust/keyStore.ts`; the panel never sees private key bytes.

import type { DeviceClient } from "../device/client";
import {
  type BackupFile,
  MIN_PASSPHRASE,
  createSigningKey,
  fingerprint,
  groupFingerprint,
  parseBackupText,
  restoreBackup,
} from "../trust/crypto";
import { type KeyStorage, type StoredKey, storedKey } from "../trust/keyStore";
import { type KeyRelation, type TrustInfo, canClaim, relateKeys } from "../trust/wire";
import { errorText, h } from "./dom";

export interface TrustPanel {
  el: HTMLElement;
  /** The connected client, or null after disconnect. */
  setClient(c: DeviceClient | null): void;
  /** Re-reads `sys/trust/info`. */
  refresh(): Promise<void>;
  /** This browser's key, if it has one. */
  key(): StoredKey | null;
  /** Called whenever the key or the device state changes. */
  onChange(f: () => void): void;
  /** How the next upload will be sent ("" when unknown); `warn` when it
   * will be unsigned to an unclaimed device or refused. */
  uploadNote(): { text: string; warn: boolean };
}

const RELATION_TEXT: Record<KeyRelation, string> = {
  "no-trust": "no trust support",
  unclaimed: "unclaimed",
  match: "match",
  different: "different key",
  "no-local-key": "claimed (no key here)",
};

/** A modal <dialog>; resolves with the inputs' values, or null on cancel. */
export function modal(title: string, body: (Node | string)[], ok: string, inputs: HTMLInputElement[] = []): Promise<string[] | null> {
  const okBtn = h("button", { class: "primary", value: "ok" }, ok);
  const cancel = h("button", { value: "cancel", formnovalidate: true }, "Cancel");
  const form = h("form", { method: "dialog" }, h("h3", {}, title), ...body, ...inputs.map((i) => h("p", {}, i)), h("p", { class: "row" }, okBtn, cancel));
  const dlg = h("dialog", { class: "modal" }, form);
  document.body.append(dlg);
  return new Promise((resolve) => {
    dlg.addEventListener("close", () => {
      dlg.remove();
      resolve(dlg.returnValue === "ok" ? inputs.map((i) => i.value) : null);
    });
    dlg.showModal();
    (inputs[0] ?? okBtn).focus();
  });
}

function passwordInput(placeholder: string): HTMLInputElement {
  return h("input", { type: "password", placeholder, required: true, minlength: MIN_PASSPHRASE, autocomplete: "new-password" });
}

function download(file: BackupFile): void {
  const blob = new Blob([JSON.stringify(file, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  h("a", { href: url, download: `middleware-signing-key-${file.fingerprint}.json` }).click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function trustPanel(storage: KeyStorage): TrustPanel {
  let client: DeviceClient | null = null;
  let mine: StoredKey | null = null;
  // undefined: not read yet (or disconnected); null: firmware predates trust.
  let info: TrustInfo | null | undefined;
  let deviceFp = "";
  const listeners: (() => void)[] = [];

  const stateEl = h("dd", {}, "not connected");
  const deviceKeyEl = h("dd", { class: "mono" }, "—");
  const minEl = h("dd", {}, "—");
  const mineEl = h("dd", { class: "mono" }, "loading …");
  const verdict = h("span", { class: "badge" }, "—");
  const msg = h("p", { class: "muted" });

  const createBtn = h("button", {}, "Create key");
  const backupBtn = h("button", { disabled: true, title: "Download the encrypted backup made when the key was created" }, "Back up key");
  const restoreFile = h("input", { type: "file", accept: ".json,application/json", hidden: true });
  const restoreBtn = h("button", {}, "Restore key");
  const claimBtn = h("button", { disabled: true, class: "primary" }, "Claim device");
  const refreshBtn = h("button", { disabled: true }, "Refresh");

  const say = (s: string) => (msg.textContent = s);

  function render() {
    mineEl.textContent = mine ? groupFingerprint(mine.fingerprint) : "no key in this browser";
    backupBtn.disabled = !mine;
    refreshBtn.disabled = !client;
    if (!client || info === undefined) {
      stateEl.textContent = client ? "reading …" : "not connected";
      deviceKeyEl.textContent = "—";
      minEl.textContent = "—";
      verdict.textContent = "—";
      delete verdict.dataset.state;
      claimBtn.disabled = true;
    } else {
      stateEl.textContent =
        info === null
          ? "firmware predates signed payloads"
          : info.state === "unclaimed"
            ? "unclaimed"
            : info.state === "claimed"
              ? "claimed"
              : "claimed, re-key window open";
      deviceKeyEl.textContent = info?.key ? groupFingerprint(deviceFp) : "—";
      minEl.textContent = info ? String(info.minVersion) : "—";
      const rel = relateKeys(info, mine?.publicKey ?? null);
      const rekey = info?.state === "rekey-window";
      verdict.textContent = rekey ? `${RELATION_TEXT[rel]} · re-key window open` : RELATION_TEXT[rel];
      verdict.dataset.state = rekey ? "rekey" : rel;
      claimBtn.disabled = !mine || !canClaim(info) || rel === "match";
      claimBtn.title = !mine ? "Create or restore a key first" : canClaim(info) ? "" : "Claimed: hold BOOT ≥3 s after boot to open the re-key window";
    }
    for (const f of listeners) f();
  }

  async function setInfo(i: TrustInfo | null | undefined) {
    info = i;
    deviceFp = i?.key ? await fingerprint(i.key) : "";
    render();
  }

  async function refresh() {
    if (!client) return setInfo(undefined);
    try {
      await setInfo(await client.trustInfo());
    } catch (e) {
      say(`sys/trust/info: ${errorText(e)}`);
      await setInfo(undefined);
    }
  }

  async function confirmReplace(): Promise<boolean> {
    if (!mine) return true;
    const r = await modal(
      "Replace this browser's key?",
      [
        h("p", {}, `This browser forgets key ${groupFingerprint(mine.fingerprint)}. Devices that pin it accept payloads only from its backup file, or after re-keying with BOOT.`),
        h("p", {}, "Make sure you have that key's backup before continuing."),
      ],
      "Replace",
    );
    return r !== null;
  }

  async function save(k: StoredKey) {
    await storage.save(k);
    mine = k;
    render();
  }

  createBtn.addEventListener("click", async () => {
    if (!(await confirmReplace())) return;
    const p1 = passwordInput("passphrase");
    const p2 = passwordInput("repeat passphrase");
    const r = await modal(
      "Create signing key",
      [
        h("p", {}, "A new Ed25519 key is generated in this browser. The private key is stored non-extractable: this page can sign with it but nobody can read it out."),
        h("p", {}, `Its only copy outside the browser is the backup file downloaded next, encrypted with this passphrase (at least ${MIN_PASSPHRASE} characters). Without the backup, losing this browser profile means re-keying every device with BOOT.`),
      ],
      "Create and download backup",
      [p1, p2],
    );
    if (!r) return;
    if (r[0] !== r[1]) return say("Passphrases differ; nothing was created.");
    try {
      say("Generating key and encrypting the backup …");
      const k = await createSigningKey(r[0]);
      await save(await storedKey(k.privateKey, k.publicKey, k.backup));
      download(k.backup);
      say(`Created key ${groupFingerprint(k.backup.fingerprint)}; backup downloaded. Keep the file and the passphrase safe.`);
    } catch (e) {
      say(`Create key failed: ${errorText(e)}`);
    }
  });

  backupBtn.addEventListener("click", () => {
    if (!mine) return;
    download(mine.backup);
    say("Downloaded the encrypted backup (same passphrase as when the key was created or restored).");
  });

  restoreBtn.addEventListener("click", () => restoreFile.click());
  restoreFile.addEventListener("change", async () => {
    const f = restoreFile.files?.[0];
    restoreFile.value = "";
    if (!f) return;
    try {
      const file = parseBackupText(await f.text());
      if (mine && mine.fingerprint !== file.fingerprint && !(await confirmReplace())) return;
      const pass = h("input", { type: "password", placeholder: "passphrase", required: true, autocomplete: "current-password" });
      const r = await modal("Restore signing key", [h("p", {}, `Key ${groupFingerprint(file.fingerprint)} from ${f.name}.`)], "Restore", [pass]);
      if (!r) return;
      say("Decrypting …");
      const k = await restoreBackup(file, r[0]);
      await save(await storedKey(k.privateKey, k.publicKey, file));
      say(`Restored key ${groupFingerprint(mine?.fingerprint ?? "")}.`);
    } catch (e) {
      say(`Restore failed: ${errorText(e)}`);
    }
  });

  claimBtn.addEventListener("click", async () => {
    if (!client || !mine || !canClaim(info ?? null)) return;
    const rekey = info?.state === "rekey-window";
    const r = await modal(
      rekey ? "Re-key device" : "Claim device",
      [
        h("p", {}, `The device will pin this browser's public key ${groupFingerprint(mine.fingerprint)} and from then on install only payloads signed with it.`),
        rekey
          ? h("p", {}, `This replaces the pinned key ${groupFingerprint(deviceFp)}, which will no longer be accepted.`)
          : h("p", {}, "This is trust on first use: whoever claims an unclaimed device first owns it. Claim only a device you hold, over a cable you control."),
        h("p", {}, "To change the key later, hold BOOT for 3 s or more after the board has booted; that opens a 60 s window in which it can be claimed again."),
      ],
      rekey ? "Re-key" : "Claim",
    );
    if (!r) return;
    try {
      await client.trustClaim(mine.publicKey);
      say(`Device now trusts ${groupFingerprint(mine.fingerprint)}.`);
    } catch (e) {
      say(`Claim failed: ${errorText(e)}`);
    }
    await refresh();
  });

  refreshBtn.addEventListener("click", () => void refresh());

  void (async () => {
    try {
      mine = await storage.load();
    } catch (e) {
      say(errorText(e));
      mineEl.textContent = "key storage unavailable";
      createBtn.disabled = true;
      restoreBtn.disabled = true;
      return;
    }
    render();
  })();

  const el = h(
    "div",
    { class: "trust" },
    h(
      "dl",
      { class: "info" },
      h("dt", {}, "Device"),
      stateEl,
      h("dt", {}, "Device key"),
      deviceKeyEl,
      h("dt", {}, "Last version"),
      minEl,
      h("dt", {}, "This browser"),
      mineEl,
      h("dt", {}, "Keys"),
      h("dd", {}, verdict),
    ),
    h("p", { class: "row" }, createBtn, backupBtn, restoreBtn, restoreFile, claimBtn, refreshBtn),
    msg,
  );

  return {
    el,
    setClient(c) {
      client = c;
      void refresh();
    },
    refresh,
    key: () => mine,
    onChange: (f) => listeners.push(f),
    uploadNote() {
      if (!client || info === undefined) return { text: "", warn: false };
      switch (relateKeys(info, mine?.publicKey ?? null)) {
        case "no-trust":
          return { text: "This firmware predates signed payloads: uploads are unsigned.", warn: false };
        case "unclaimed":
          return {
            text: "This device is unclaimed: uploads are unsigned and anyone with a cable can replace the payload. Claim it to pin your key.",
            warn: true,
          };
        case "match":
          return { text: `Uploads are signed with ${groupFingerprint(mine?.fingerprint ?? "")}.`, warn: false };
        case "different":
          return { text: "The device trusts a different key: uploads will be refused. Restore that key, or re-key the device.", warn: true };
        case "no-local-key":
          return { text: "The device is claimed and this browser has no key: restore the key from its backup.", warn: true };
      }
    },
  };
}
