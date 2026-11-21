// The Device tab's Mesh section: nodes reached through the cloud Zenoh
// router (device/mesh.ts), probed for what they drive (device/probe.ts).
//
// Connect opens the WebSocket to the mesh URL (`?mesh=`, the last one used,
// or `wss://updates.<site>/mesh`); Scan lists every node that answers
// `sys/info` by sender id; Probe reads one node's manifest and control
// points and shows them with the drafted equipment modules, the lathe's
// IoMap and the mwctl commands that deploy over the mesh (shown only when
// every value from the mesh has the shape it must have, quoted).
// "Use as plant" hands the drafted plant to Blocks. The mesh is probe-only:
// nothing here changes a node, and the panel never claims one.
//
// All decisions are in mesh.ts and probe.ts; this file only renders them.

import {
  MeshClient,
  type MeshDevice,
  type MeshLink,
  MeshTransport,
  defaultMeshUrl,
  scan,
  siteOfMeshUrl,
} from "../device/mesh";
import { type DeployPlan, type ProbeResult, deployCommands, emOf, probeNode } from "../device/probe";
import type { PlantModel } from "../model/plant";
import type { Protocol } from "../protocol/wasm";
import { errorText, h } from "./dom";

const URL_KEY = "mesh-url";

export interface MeshPanelOptions {
  /** Opens the link; defaults to `MeshTransport.connect`. */
  connect?: (url: string) => Promise<MeshLink>;
  /** The URL field's first value; defaults to `defaultMeshUrl` for this page. */
  defaultUrl?: string;
  /** "Use as plant": the drafted plant for Blocks. */
  usePlant(p: PlantModel): void;
  /** The probed node's client (read only), or null once the link is gone. */
  onSelect?(c: MeshClient | null): void;
}

function storedUrl(): string | null {
  try {
    return localStorage.getItem(URL_KEY);
  } catch {
    return null;
  }
}

function remember(url: string): void {
  try {
    localStorage.setItem(URL_KEY, url);
  } catch {
    /* storage unavailable */
  }
}

function copyButton(text: () => string): HTMLButtonElement {
  const b = h("button", { type: "button" }, "Copy");
  b.addEventListener("click", () => void navigator.clipboard?.writeText(text()).catch(() => {}));
  return b;
}

export function meshPanel(proto: Protocol, opts: MeshPanelOptions): { el: HTMLElement } {
  const connect = opts.connect ?? ((url: string) => MeshTransport.connect(url, proto));
  let link: MeshLink | null = null;
  let probed: ProbeResult | null = null;

  // A `?mesh=` URL comes from whoever made the link: it only fills the
  // field, says so, and is used once the user presses Connect.
  const fromLink = opts.defaultUrl === undefined ? new URLSearchParams(location.search).get("mesh") : null;
  const urlIn = h("input", {
    type: "url",
    size: 40,
    value:
      opts.defaultUrl ?? defaultMeshUrl({ hostname: location.hostname, search: location.search, stored: storedUrl() }),
  });
  const linkNote = fromLink
    ? h("p", { class: "warn" }, `This mesh URL is from this link's ?mesh= (${fromLink}): check it before you connect.`)
    : null;
  /** The URL of the open link (the commands' site comes from it). */
  let activeUrl: string | null = null;
  const active = h("p", { class: "mesh-active mono" }, "not connected");
  const connBtn = h("button", { class: "primary" }, "Connect");
  const scanBtn = h("button", { disabled: true }, "Scan");
  const status = h("span", { class: "muted" }, "not connected");
  const nodes = h("tbody");
  const result = h("div", { class: "mesh-probe" });

  const say = (s: string) => (status.textContent = s);

  function dropped() {
    link = null;
    activeUrl = null;
    active.textContent = "not connected";
    probed = null;
    connBtn.textContent = "Connect";
    scanBtn.disabled = true;
    nodes.replaceChildren();
    result.replaceChildren();
    opts.onSelect?.(null);
  }

  connBtn.addEventListener("click", async () => {
    connBtn.disabled = true;
    try {
      if (link) {
        const l = link;
        dropped();
        await l.close();
        say("disconnected");
        return;
      }
      const url = urlIn.value.trim();
      say(`connecting to ${url} …`);
      const l = await connect(url);
      remember(url);
      link = l;
      activeUrl = url;
      active.textContent = `connected to ${url}`;
      l.onClosed(() => {
        if (link === l) {
          dropped();
          say("the mesh link closed; connect again");
        }
      });
      connBtn.textContent = "Disconnect";
      scanBtn.disabled = false;
      say(`connected as ${l.senderId}`);
    } catch (e) {
      say(`connect failed: ${errorText(e)}`);
    } finally {
      connBtn.disabled = false;
    }
  });

  scanBtn.addEventListener("click", async () => {
    if (!link) return;
    scanBtn.disabled = true;
    say("scanning …");
    try {
      const found = await scan(link, proto);
      nodes.replaceChildren(...found.map((d) => nodeRow(d)));
      say(found.length ? `${found.length} node${found.length === 1 ? "" : "s"} answered` : "no node answered");
    } catch (e) {
      say(`scan failed: ${errorText(e)}`);
    } finally {
      scanBtn.disabled = !link;
    }
  });

  function nodeRow(d: MeshDevice): HTMLTableRowElement {
    const action = d.info
      ? h("button", { type: "button" }, "Probe")
      : h("span", { class: "muted" }, d.problem ?? "no sys/info");
    if (action instanceof HTMLButtonElement) action.addEventListener("click", () => void probe(d, action));
    return h(
      "tr",
      {},
      h("td", { class: "mono" }, d.sender),
      h("td", { class: "mono" }, d.info?.node ?? "—"),
      h("td", {}, d.info?.chip ?? "—"),
      h("td", {}, d.info?.firmware ?? "—"),
      h("td", {}, action),
    );
  }

  async function probe(d: MeshDevice, btn: HTMLButtonElement) {
    if (!link) return;
    btn.disabled = true;
    say(`probing ${d.sender} …`);
    try {
      const client = new MeshClient(link, d, proto);
      probed = await probeNode(client, proto);
      opts.onSelect?.(client);
      result.replaceChildren(...render(probed, d.sender));
      say(`probed ${d.sender}: ${probed.points.length} points`);
    } catch (e) {
      say(`probe failed: ${errorText(e)}`);
    } finally {
      btn.disabled = false;
    }
  }

  function render(r: ProbeResult, sender: string): Node[] {
    const site = activeUrl === null ? null : siteOfMeshUrl(activeUrl);
    const plan = deployCommands(r, { site, sender, wasm: "<payload.wasm>" });
    const io = JSON.stringify(r.io, null, 2);
    const useBtn = h("button", { class: "primary", type: "button" }, "Use as plant");
    useBtn.addEventListener("click", () => {
      if (probed?.plantable) opts.usePlant(probed.plant);
    });
    const points = h(
      "table",
      { class: "mesh-points" },
      h(
        "thead",
        {},
        h("tr", {}, ...["Point", "Kind", "Flow", "Unit", "Range", "Scale", "Module"].map((t) => h("th", {}, t))),
      ),
      h(
        "tbody",
        {},
        ...r.points.map((p) =>
          h(
            "tr",
            {},
            h("td", { class: "mono" }, p.name),
            h("td", {}, p.kind),
            h("td", {}, p.flow),
            h("td", {}, p.unit),
            h("td", {}, `${p.min} … ${p.max}`),
            h("td", {}, p.scale ? String(p.scale) : "—"),
            h("td", {}, emOf(p) ?? "—"),
          ),
        ),
      ),
    );
    return [
      h("h4", {}, r.boardName ?? r.chip, " · node ", h("span", { class: "mono" }, r.node ?? "—")),
      points,
      r.problems.length
        ? h("ul", { class: "problems" }, ...r.problems.map((p) => h("li", {}, p)))
        : h("p", {}, "No problems."),
      r.plantable
        ? h(
            "p",
            { class: "row" },
            useBtn,
            h(
              "span",
              { class: "muted" },
              `unit ${r.unit.id}: ${r.unit.ems.map((e) => e.id).join(", ") || "no modules"}`,
            ),
          )
        : h("p", { class: "warn" }, "This node is not offered as a plant (see the problems above)."),
      h(
        "h4",
        {},
        "IoMap (cnc/model) ",
        copyButton(() => io),
      ),
      h("pre", { class: "mono" }, io),
      ...deploySection(plan),
    ];
  }

  /** The commands (the bind one carries the specs), or (for any value of the wrong
   * shape) only why there are none. */
  function deploySection(plan: DeployPlan): Node[] {
    if (!plan.ok) {
      return [h("h4", {}, "Deploy with mwctl"), h("p", { class: "warn" }, `${plan.refused}. No command is shown.`)];
    }
    const commands = plan.commands.join("\n");
    return [
      h(
        "h4",
        {},
        "Deploy with mwctl ",
        copyButton(() => commands),
      ),
      h(
        "p",
        { class: "muted" },
        "The mesh is probe-only: the browser changes nothing on the node. Run these with the lathe's key, " +
          "with your payload's file for <payload.wasm>.",
      ),
      h("pre", { class: "mono" }, commands),
    ];
  }

  const el = h(
    "div",
    { class: "mesh" },
    h("p", { class: "row" }, h("label", {}, "Mesh ", urlIn), connBtn, scanBtn, status),
    ...(linkNote ? [linkNote] : []),
    active,
    h(
      "table",
      { class: "mesh-nodes" },
      h("thead", {}, h("tr", {}, ...["Sender", "Node", "Chip", "Firmware", ""].map((t) => h("th", {}, t)))),
      nodes,
    ),
    result,
  );
  return { el };
}
