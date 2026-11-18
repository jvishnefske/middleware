// The Device page's channel binding panel (BD5): for the payload chosen
// for upload, list the channels it declares, bind each to a key or to one
// of the connected board's sources (as its manifest offers them), show
// `checkBinding`'s issues with one-click fixes, bind an input to a key on
// another node the board has heard (`sys/peers`, PR4), and install the binding
// on the board (signed when the board is claimed by this browser's key).
// The logic is `device/binding.ts`; this file only renders it.

import { applyFix, bindingIssues, describeChannel, describeTarget, emptyBinding, installBinding, setTarget, suggestBinding } from "../device/binding";
import type { DeviceClient } from "../device/client";
import type { SigningKey } from "../device/upload";
import type { BindingRecord, ChannelDecl, Protocol } from "../protocol/wasm";
import { sha256 } from "../trust/crypto";
import { errorText, h } from "./dom";

export interface BindingPanelDeps {
  /** The connected board, if any. */
  client(): DeviceClient | null;
  /** This browser's signing key, if any. */
  key(): SigningKey | null;
  /** The payload chosen for upload. */
  payload(): Promise<{ name: string; wasm: Uint8Array }>;
}

export function bindingPanel(proto: Protocol, deps: BindingPanelDeps): { el: HTMLElement } {
  const loadBtn = h("button", {}, "Edit channel binding");
  const installBtn = h("button", { disabled: true, class: "primary" }, "Install binding");
  const rows = h("tbody");
  const issuesEl = h("ul", { class: "binding-issues" });
  const msg = h("p", { class: "muted", role: "status", "aria-live": "polite" });
  let wasm: Uint8Array | null = null;
  let manifest: Uint8Array | null = null;
  let decls: ChannelDecl[] = [];
  let record: BindingRecord | null = null;
  let peers: string[] = [];

  function render() {
    rows.replaceChildren();
    issuesEl.replaceChildren();
    installBtn.disabled = !record || !deps.client();
    if (!record || !wasm) return;
    const r = record;
    for (const c of decls) {
      const target = r.entries.find((e) => e.name === c.name)?.target;
      const keyIn = h("input", { type: "text", placeholder: "key", value: target?.kind === "key" ? target.key : "" });
      const setKey = h("button", {}, "Use key");
      setKey.addEventListener("click", () => {
        const k = keyIn.value.trim();
        update(setTarget(r, c.name, k ? { kind: "key", key: k } : null));
      });
      const clear = h("button", {}, "Unbind");
      clear.addEventListener("click", () => update(setTarget(r, c.name, null)));
      const peerSel = h("select", { title: "A node the board has heard" }, ...peers.map((p) => h("option", { value: p }, p)));
      const setRemote = h("button", { disabled: !peers.length || c.dir !== "in" }, "Use key from node");
      setRemote.addEventListener("click", () => {
        const k = keyIn.value.trim();
        if (k) update(setTarget(r, c.name, { kind: "remote", node: (peerSel as HTMLSelectElement).value, key: k }));
      });
      rows.append(
        h(
          "tr",
          {},
          h("td", {}, c.name),
          h("td", {}, describeChannel(c)),
          h("td", {}, describeTarget(target)),
          h("td", {}, keyIn, setKey, clear, ...(peers.length ? [peerSel, setRemote] : [])),
        ),
      );
    }
    if (!manifest) {
      issuesEl.append(h("li", { class: "muted" }, "The board reports no manifest: the binding is not checked."));
      return;
    }
    let issues;
    try {
      issues = bindingIssues(proto, manifest, wasm, r, peers);
    } catch (e) {
      issuesEl.append(h("li", { class: "note" }, errorText(e)));
      return;
    }
    if (!issues.length) issuesEl.append(h("li", { class: "muted" }, "No issues."));
    for (const i of issues) {
      const li = h("li", { class: "note" }, i.message, " ");
      if (i.fix) {
        const fix = h("button", {}, i.fix.kind === "remove" ? "Remove" : i.fix.kind === "keyNamed" ? `Bind to key ${i.channel}` : `Use ${describeTarget(i.fix)}`);
        fix.addEventListener("click", () => update(applyFix(r, i)));
        li.append(fix);
      }
      issuesEl.append(li);
    }
  }

  function update(next: BindingRecord) {
    try {
      proto.encodeBinding(next);
      record = next;
      msg.textContent = "";
    } catch (e) {
      msg.textContent = errorText(e);
    }
    render();
  }

  loadBtn.addEventListener("click", async () => {
    try {
      const p = await deps.payload();
      decls = proto.readChannels(p.wasm);
      if (!decls.length) {
        record = null;
        msg.textContent = `${p.name} declares no channels (ABI 8): nothing to bind.`;
        render();
        return;
      }
      wasm = p.wasm;
      const digest = await sha256(p.wasm);
      const client = deps.client();
      manifest = client ? await client.manifestBody() : null;
      peers = client ? await client.peers() : [];
      record = manifest ? suggestBinding(proto, manifest, p.wasm, digest) : emptyBinding(digest);
      msg.textContent = `${p.name}: ${decls.length} channels.`;
      render();
    } catch (e) {
      msg.textContent = errorText(e);
    }
  });

  installBtn.addEventListener("click", async () => {
    const client = deps.client();
    if (!client || !record) return;
    installBtn.disabled = true;
    try {
      const r = await installBinding(client, proto, record, deps.key());
      msg.textContent =
        r.kind === "signed"
          ? `Binding installed, signed, version ${r.version}; it applies at the payload's next start.`
          : "Binding installed unsigned (the board is unclaimed); it applies at the payload's next start.";
    } catch (e) {
      msg.textContent = errorText(e);
    } finally {
      installBtn.disabled = !deps.client();
    }
  });

  const el = h(
    "div",
    { class: "upload binding" },
    h("p", { class: "row" }, loadBtn, installBtn),
    h("table", {}, h("thead", {}, h("tr", {}, h("th", {}, "Channel"), h("th", {}, "Declared"), h("th", {}, "Bound to"), h("th", {}, ""))), rows),
    issuesEl,
    msg,
  );
  return { el };
}
