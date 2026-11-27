// The Room panel (design.md BM10), beside the Editor's simulator: the
// signalling server, the room's name, Join and Leave, this page's node id,
// the peers with their channels and how they pair with this page's, and
// the counters (sent, received, refused, kept in the page).
//
// The panel also owns the network the simulated payload runs on while it
// is shown: `run(wasm, events)` loads the payload as the one unit of a
// SimBus whose node id is this page's, with every channel the payload
// declares bound to its room key (`room/<room>/<name>`), so joining or
// leaving, and peers coming and going, need no restart. A payload without
// a `mw.channels` section (WAT, AssemblyScript) gets the declarations
// typed in the panel. A board connected on the Device tab publishes into
// the room; nothing from the room is sent to it.
//
// Peers are not authenticated: the panel says so, and the room carries
// channel data only.

import type { MeshEnv } from "../mesh/env";
import { DEFAULT_SERVER, Mesh, type MeshOptions, pageHello } from "../mesh/mesh";
import type { ChannelDecl, MeshServer, Protocol } from "../protocol/wasm";
import { SimBus } from "../sim/bus";
import { MixedNet, type UnitLink } from "../sim/net";
import type { Clock, SessionEvents, SimSession } from "../sim/session";
import { errorText, h } from "./dom";

export const ROOM_SETTINGS_KEY = "mw.room";

type Store = Pick<Storage, "getItem" | "setItem">;

export interface RoomSettings {
  server: MeshServer;
  room: string;
  /** Channels for a payload without a section: "audio:in level:out". */
  declare: string;
}

export interface RoomPanelDeps {
  proto: Protocol;
  /** This page's node id (16 hex digits). */
  node: string;
  /** Where the settings are kept (the page's localStorage). */
  storage?: Store | null;
  /** The browser's, unless a test passes its own. */
  env?: MeshEnv;
  clock?: Clock;
  /** How often the counters are redrawn while joined; 0 for never. */
  refreshMs?: number;
}

export interface RoomPanel {
  el: HTMLElement;
  /** Runs `wasm` as this page's payload, on the room's network, stepped
   * by the panel's own timer. Throws what the simulator throws. */
  run(wasm: Uint8Array, events?: SessionEvents): SimSession;
  /** Stops the payload `run` started. */
  stop(): void;
  /** The board on the Device tab (null: none). */
  setBoard(link: UnitLink | null): void;
  /** The mesh while joined (tests). */
  readonly mesh: Mesh | null;
  dispose(): void;
}

/** "audio:in level:out" as bytes channels; throws with what is wrong. */
export function parseDeclared(text: string): ChannelDecl[] {
  return text
    .split(/[\s,]+/)
    .filter(Boolean)
    .map((word) => {
      const m = /^([a-z0-9_]{1,24}):(in|out)$/.exec(word);
      if (!m) throw new Error(`"${word}" is not name:in or name:out (names are a-z, 0-9, _)`);
      return { name: m[1]!, dir: m[2] as "in" | "out", kind: "bytes", sensor: null, formats: 0, channels: 0, rateMin: 0, rateMax: 0xffffffff, required: false } satisfies ChannelDecl;
    });
}

function loadSettings(storage: Store | null | undefined): RoomSettings {
  const fallback: RoomSettings = { server: { ...DEFAULT_SERVER }, room: "", declare: "" };
  try {
    const saved = JSON.parse(storage?.getItem(ROOM_SETTINGS_KEY) ?? "null") as Partial<RoomSettings> | null;
    if (!saved || typeof saved !== "object") return fallback;
    const s = (saved.server ?? {}) as Partial<MeshServer>;
    return {
      server: {
        host: typeof s.host === "string" ? s.host : fallback.server.host,
        port: typeof s.port === "number" ? s.port : fallback.server.port,
        path: typeof s.path === "string" ? s.path : fallback.server.path,
        key: typeof s.key === "string" ? s.key : fallback.server.key,
        secure: typeof s.secure === "boolean" ? s.secure : fallback.server.secure,
      },
      room: typeof saved.room === "string" ? saved.room : "",
      declare: typeof saved.declare === "string" ? saved.declare : "",
    };
  } catch {
    return fallback;
  }
}

const STATE_TEXT: Record<Mesh["state"], string> = {
  idle: "Not in a room.",
  joining: "Joining ...",
  joined: "In room",
  reconnecting: "Lost the signalling server; trying again. Links to peers stay up",
  failed: "Could not join",
};

const MODE_TEXT: Record<Mesh["discoveryMode"], string> = {
  starting: "",
  listing: "peers from the server's list",
  hub: "this page is the room's meeting point",
  member: "peers from the room's meeting point",
  searching: "looking for the room's meeting point",
};

export function roomPanel(deps: RoomPanelDeps): RoomPanel {
  const { proto } = deps;
  const settings = loadSettings(deps.storage);
  const refreshMs = deps.refreshMs ?? 1000;
  let mesh: Mesh | null = null;
  let bus: SimBus | null = null;
  let net: MixedNet | null = null;
  let payload: Uint8Array | null = null;
  /** The room the running payload's channels were bound for. */
  let boundRoom: string | null = null;
  let board: UnitLink | null = null;
  let offBoard: (() => void) | null = null;
  let refresh: ReturnType<typeof setInterval> | null = null;
  /** A problem with what was typed, shown until the next attempt. */
  let problem = "";

  const field = (label: string, input: HTMLElement) => h("label", {}, `${label} `, input);
  const hostIn = h("input", { type: "text", value: settings.server.host, size: 18, spellcheck: "false" });
  const portIn = h("input", { type: "number", value: settings.server.port, min: 1, max: 65535 });
  const pathIn = h("input", { type: "text", value: settings.server.path, size: 8, spellcheck: "false" });
  const keyIn = h("input", { type: "text", value: settings.server.key, size: 8, spellcheck: "false" });
  const secureIn = h("input", { type: "checkbox", checked: settings.server.secure });
  const roomIn = h("input", { type: "text", value: settings.room, size: 14, maxlength: proto.mesh.constants.maxRoom, spellcheck: "false", placeholder: "a-z 0-9 _" });
  const declareIn = h("input", { type: "text", value: settings.declare, size: 24, spellcheck: "false", placeholder: "audio:in level:out" });
  const joinBtn = h("button", { type: "submit", class: "primary" }, "Join");
  const leaveBtn = h("button", { type: "button", disabled: true }, "Leave");
  const status = h("p", { class: "room-status", role: "status" });
  const counters = h("p", { class: "muted room-counters" });
  const hint = h("p", { class: "note", hidden: true });
  const peersEl = h("div", { class: "room-peers" });

  const serverNow = (): MeshServer => ({
    host: hostIn.value.trim(),
    port: Number(portIn.value),
    path: pathIn.value.trim() || "/",
    key: keyIn.value.trim(),
    secure: secureIn.checked,
  });

  function save(): void {
    try {
      deps.storage?.setItem(ROOM_SETTINGS_KEY, JSON.stringify({ server: serverNow(), room: roomIn.value.trim(), declare: declareIn.value } satisfies RoomSettings));
    } catch {
      // Not kept.
    }
  }

  const describe = (c: ChannelDecl) => `${c.name} ${c.dir === "in" ? "(reads)" : "(writes)"}`;

  function render(): void {
    const joined = mesh !== null && mesh.state !== "idle" && mesh.state !== "failed";
    joinBtn.disabled = joined;
    leaveBtn.disabled = !joined;
    for (const el of [hostIn, portIn, pathIn, keyIn, secureIn, roomIn]) el.disabled = joined;
    if (!mesh || problem) {
      status.textContent = problem || STATE_TEXT.idle;
      status.classList.toggle("note", problem !== "");
      counters.textContent = "";
      peersEl.replaceChildren();
      hint.hidden = true;
      hint.textContent = "";
      return;
    }
    const mode = MODE_TEXT[mesh.discoveryMode];
    const why = mesh.error ? `: ${mesh.error}` : "";
    status.textContent = mesh.state === "joined" ? `${STATE_TEXT.joined} ${mesh.room}${mode ? ` (${mode})` : ""}.` : mesh.state === "idle" ? STATE_TEXT.idle : `${STATE_TEXT[mesh.state]}${why}.`;
    status.classList.toggle("note", mesh.state === "failed" || mesh.state === "reconnecting");
    const c = mesh.counters;
    counters.textContent = `Sent ${c.sent}, received ${c.received}, refused ${c.refused}, kept in this page ${c.kept}.`;
    hint.hidden = !(joined && payload !== null && boundRoom !== mesh.room);
    hint.textContent = hint.hidden ? "" : "The running payload's channels were connected before this room was chosen (or it declares none). Start the simulation again to put them in the room.";

    const room = mesh.room;
    const peers = mesh.peers;
    const unlinked = mesh.listed.filter((id) => !peers.some((p) => p.id === id));
    const rows = peers.map((p) => {
      const pairs = p.pairs.map((pair) => h("li", { class: pair.state === "mismatch" ? "note" : pair.state === "connected" ? "" : "muted" }, pair.state === "mismatch" ? `Does not fit: ${pair.message}` : pair.message));
      return h(
        "tr",
        {},
        h("td", {}, h("code", {}, p.node)),
        h("td", {}, p.state === "open" ? `linked${p.info ? ` (${p.info.chip})` : ""}` : p.state === "hello" ? "saying hello" : "connecting"),
        h("td", {}, p.channels.map(describe).join(", ") || h("span", { class: "muted" }, "none")),
        h("td", {}, pairs.length ? h("ul", {}, ...pairs) : h("span", { class: "muted" }, "no channel in common")),
        h("td", {}, `${p.received} received${p.refused ? `, ${p.refused} refused` : ""}`),
      );
    });
    peersEl.replaceChildren(
      ...[
        peers.length
          ? h(
              "table",
              { class: "room-table" },
              h("caption", {}, "Peers"),
              h("thead", {}, h("tr", {}, ...["Node", "Link", "Its channels", "With this page", "From it"].map((t) => h("th", { scope: "col" }, t)))),
              h("tbody", {}, ...rows),
            )
          : joined
            ? h("p", { class: "muted" }, "Nobody else is linked yet. Open this page in another browser and join the same room.")
            : null,
        unlinked.length
          ? h("p", { class: "muted" }, `In the room but not linked (waiting to be dialled, or beyond ${proto.mesh.constants.maxPeers} links): ${unlinked.map((id) => proto.mesh.parsePeerId(room, id) ?? id).join(", ")}`)
          : null,
      ].filter((n) => n !== null),
    );
  }

  function wireBoard(): void {
    offBoard?.();
    offBoard = null;
    net?.detach("board");
    if (!board) return;
    offBoard = mesh?.addBoard(board) ?? null;
    try {
      // The page's own payload hears the board too, from the board's node.
      net?.attach("board", board);
    } catch {
      // A unit of that name is there already.
    }
  }

  function leave(): void {
    if (refresh !== null) clearInterval(refresh);
    refresh = null;
    offBoard?.();
    offBoard = null;
    mesh?.leave();
    mesh = null;
    problem = "";
    render();
  }

  async function join(): Promise<void> {
    if (mesh && mesh.state !== "failed") return;
    leave();
    const room = roomIn.value.trim();
    if (!proto.mesh.validRoom(room)) {
      problem = `A room name is 1 to ${proto.mesh.constants.maxRoom} of a-z, 0-9 and _.`;
      render();
      roomIn.focus();
      return;
    }
    const opts: MeshOptions = { proto, room, node: deps.node, server: serverNow(), env: deps.env, hello: () => pageHello(proto, deps.node, payload) };
    let mine: Mesh;
    try {
      // Throws for an address that does not make a URL.
      proto.mesh.peersUrl(serverNow());
      mine = new Mesh(opts);
    } catch (e) {
      problem = `${errorText(e)}.`;
      render();
      return;
    }
    save();
    mesh = mine;
    mine.onChange(() => {
      if (mesh === mine) render();
    });
    if (net) mine.bind(net);
    wireBoard();
    if (refreshMs > 0) refresh = setInterval(render, refreshMs);
    render();
    await mine.join();
    if (mesh === mine) render();
  }

  function stop(): void {
    net?.dispose();
    bus?.dispose();
    bus = net = null;
    payload = null;
    boundRoom = null;
    render();
  }

  function run(wasm: Uint8Array, events: SessionEvents = {}): SimSession {
    stop();
    let module = wasm;
    if (proto.readChannels(wasm).length === 0 && declareIn.value.trim()) {
      module = proto.withChannels(wasm, proto.encodeChannels(parseDeclared(declareIn.value)));
    }
    // The room the channels go to: the one joined, else the one typed.
    const room = mesh?.room ?? roomIn.value.trim();
    const inRoom = proto.mesh.validRoom(room) && proto.readChannels(module).length > 0;
    const b = new SimBus(proto, deps.clock);
    try {
      const session = b.add("local", module, { node: deps.node, events, binding: inRoom ? proto.mesh.binding(module, undefined, room) : undefined });
      bus = b;
      net = new MixedNet(b);
      payload = module;
      boundRoom = inRoom ? room : null;
      mesh?.bind(net);
      wireBoard();
      b.start();
      save();
      render();
      return session;
    } catch (e) {
      b.dispose();
      throw e;
    }
  }

  const form = h("form", { class: "row" }, field("Room", roomIn), joinBtn, leaveBtn);
  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    void join();
  });
  leaveBtn.addEventListener("click", leave);
  declareIn.addEventListener("change", save);

  const el = h(
    "section",
    { class: "room-panel", "aria-label": "Room" },
    h("h3", {}, "Room"),
    h(
      "p",
      { class: "muted" },
      "Browsers that join the same room link directly (WebRTC), and their payloads' channels connect by name. This page is node ",
      h("code", {}, deps.node),
      ".",
    ),
    form,
    h(
      "details",
      {},
      h("summary", {}, "Server and channels"),
      h("p", { class: "row" }, field("Signalling server", hostIn), field("Port", portIn), field("Path", pathIn), field("Key", keyIn), h("label", {}, secureIn, " TLS")),
      h("p", { class: "muted" }, "Any PeerJS-compatible server. The default is the public PeerJS one; docs/browser-mesh.md says how to run your own."),
      h("p", { class: "row" }, field("Channels of a payload that declares none", declareIn)),
    ),
    status,
    hint,
    counters,
    peersEl,
    h(
      "p",
      { class: "muted" },
      "Peers are not checked: anyone who knows the room's name can join it and send channel data under any node id. The room carries channel data only, never system keys, and nothing from it is sent to a board.",
    ),
  );
  render();

  return {
    el,
    run,
    stop,
    setBoard(link) {
      board = link;
      wireBoard();
    },
    get mesh() {
      return mesh;
    },
    dispose() {
      leave();
      stop();
    },
  };
}
