// Blocks tab: a Blockly visual payload editor. The workspace is turned
// into AssemblyScript by src/blocks/generator.ts (shown read-only in
// CodeMirror as you edit), compiled and checked by src/blocks/pipeline.ts,
// then run in the simulator (SimSession) or handed to the Device tab for
// upload, like the Editor tab.
//
// The tab edits a plant (src/model/plant.ts, IR v4: units, equipment
// modules, phase classes and phases, recipes) and, through the unit
// picker, one unit's payload model (src/model/payload.ts: the board, the
// pin table, the state machines, the Blockly workspace and the flows),
// written back into the plant on every change (model/plantEdit.ts
// `withUnitPayload`), so switching units leaves each unit's code as it
// was. Seven views share the toolbar (unit, board, examples, compile,
// simulate, upload, export, import) and the right-hand column (generated
// code, build output, simulator): "Units", the plant's units
// (plantViews.ts; bind one to the connected device's sys/info node id),
// "Blocks", the Blockly workspace, "Pins", the pin table (pinTable.ts),
// "States", the state-machine tables (statesTable.ts), "Flows", the flow
// graphs (flowsView.ts: an SVG canvas and an outline; phase entries and
// actions name the unit's phases), and "Phases", the unit's equipment
// modules, the plant's phase classes and parameters, the unit's phases
// and their body flows, and the read-only phase state model
// (plantViews.ts), and "Recipe", the plant's master recipes as a chart
// and an outline, their allocation to units and a run of every unit in
// the browser simulator under recipe-core's engine (recipeView.ts; its
// tab counts the recipes' errors). A unit with phases compiles to an S88 phase server
// (blocks/phaseGen.ts); the plant's issues for it (plantEdit.ts
// `plantIssues`, in plain words) keep Compile from building and switch
// to Phases.
// They are sub-views of one tab rather than sibling tabs because they are
// parts of one payload: a pin or table change shows at once in the
// generated code and the simulator next to it, and compile and upload act
// on all of them. The views are a tablist (arrow keys, Home and End move
// between them); the last one shown is remembered. The pin table is
// re-checked with protocol-wasm `checkPins` (the firmwares' pin rules),
// the machines with `checkMachines` and the flows with `checkFlows` on
// every pin, machine or flow change; errors
// keep Compile from building (it switches to the view at fault) and are
// counted on the view's tab ("States (2 to fix)"); warnings do not block.
//
// Renaming a pin label, a state, an event or a machine id in a table
// renames the blocks and flows that refer to it in the live workspace. The machine
// blocks name `<machine id>.<state or event>` with dropdowns of the
// tables' names (`field_machine_state`, `field_machine_event`), keeping a
// name the tables no longer have, marked "(not in States)". While the
// simulator runs, the generated code's `sm <id>: ...` log lines
// (machines.ts `parseMachineLog`) give each machine's current state: the
// States view highlights it and a live line in the simulator says it
// ("State: Mode = On (from Off on PRESS)"); the flows' `flow <id>: <node>`
// lines (flows.ts `flowTrail`) light up each flow's latest run on the
// Flows canvas and outline.
//
// The board decides the profile and so which blocks are enabled: blocks
// the profile does not support are disabled in the toolbox and in the
// workspace (with the "profile" disabled reason, so switching back
// re-enables them). Picking a board here or on the Start tab re-seeds a
// pin table that is empty or still the old board's defaults (otherwise
// the table is kept and re-checked, with fixes offered). Pin blocks that
// name a label get a dropdown of the table's labels (`field_pin_label`).
//
// The plant persists in localStorage as version 4 (a version 1-3 save or
// an older Blocks save is migrated to a one-unit plant,
// src/blocks/storage.ts) and is exported as plant.json; Import JSON takes
// a plant (replacing the whole plant) or a payload file (into the selected
// unit). The Examples menu loads the bundled workspaces into the selected
// unit, adding the pins they need, or the worked example plant "Blink
// then Measure".
// The Start tab's "Try it", the Examples menu and Import JSON all ask
// before they replace different work (src/blocks/replace.ts decides;
// cancelling leaves everything as it was). "Try it" then compiles the
// example and starts the simulator.
//
// A "Hide categories" toggle in the workspace heading hides the Blockly toolbox
// to give the blocks the full width; on narrow screens the toolbox is
// slimmer as well (style.css).

import { javascript } from "@codemirror/lang-javascript";
import { EditorState } from "@codemirror/state";
import { EditorView, basicSetup } from "codemirror";
import "blockly/blocks";
import * as Blockly from "blockly/core";
import * as En from "blockly/msg/en";
import { BLOCK_DEFS, PIN_LABEL_BLOCKS, blockSupported, toolbox } from "../blocks/defs";
import { EXAMPLES, exampleWorkspace } from "../blocks/examples";
import { type GenIssue, generate } from "../blocks/generator";
import { buildPayload } from "../blocks/pipeline";
import { PROFILES, type ProfileId } from "../blocks/profiles";
import { exampleModel, planImport, wouldReplaceWork } from "../blocks/replace";
import { browserStore, loadPlant, savePlant } from "../blocks/storage";
import { type PlantModel, parsePlant, phaseDecls, phaseSpecs, plantFromPayload, serializePlant, unitPayload } from "../model/plant";
import { type PlantIssue, nodeFromInfo, plantIssues, renameCm, withUnitPayload } from "../model/plantEdit";
import { phasesView, unitsView } from "./plantViews";
import { recipeView } from "./recipeView";
import { recipeTabText } from "./recipeModel";
import { EXAMPLE_PLANT } from "../model/plantExample";
import type { WorkspaceJson } from "../blocks/workspace";
import {
  MACHINE_EVENT_BLOCKS,
  MACHINE_STATE_BLOCKS,
  type Machine,
  type MachineIssue,
  machineRef,
  machineRefsUsed,
  parseMachineLog,
  parseMachineRef,
} from "../model/machines";
import { type PayloadModel, flowsOf, makeModel, profileOfModel, withBoard, withFlows, withMachines, withPins, withWorkspace } from "../model/payload";
import { type PinFunc, type PinIssue, type PinTable, boardDefaults, checkTable, labelsUsed, seedForTarget, withNeeds } from "../model/pins";
import { TARGET_IDS, type TargetId, chipOf, isTargetId, targetLabel } from "../model/target";
import type { BoardId } from "../onboarding/boards";
import type { BoardPins, PinState, SimEvent } from "../protocol/wasm";
import { SimSession } from "../sim/session";
import type { App } from "./app";
import { errorText, formatBytes, h } from "./dom";
import { type Flow, type FlowIssue, flowTrail, parseFlowLog, renameFlowPin, renameFlowRef } from "../model/flows";
import { flowsTabText, flowTrailText } from "./flowsModel";
import { flowsView } from "./flowsView";
import { pinTableView } from "./pinTable";
import { type Rename, renamePinLabel, simStateText, statesTabText } from "./statesModel";
import { statesView } from "./statesTable";
import { modal } from "./trustPanel";

/** The Examples menu entry that loads the worked example plant. */
const PLANT_EXAMPLE = "plant:blink-measure";
/** Where the selected unit is remembered. */
const UNIT_KEY = "blocks-unit";
/** Disabled reason for blocks the selected profile does not support. */
const PROFILE_REASON = "profile";
const BUTTONS: [number, string][] = [
  [1, "BOOT"],
  [2, "LEFT"],
  [3, "SELECT"],
  [4, "START"],
  [5, "RIGHT"],
];

/** The profile's toolbox, typed for Blockly (defs.ts stays Blockly-free). */
function toolboxFor(profile: ProfileId): Blockly.utils.toolbox.ToolboxDefinition {
  return toolbox(profile) as unknown as Blockly.utils.toolbox.ToolboxDefinition;
}

/** Where `field_pin_label` dropdowns get their options: the pin table's
 * labels, plus labels blocks already name (so loading a workspace whose
 * label is missing from the table keeps it, shown as missing). */
const labelSource = { table: [] as string[], extra: [] as string[] };

/** A dropdown of pin-table labels that accepts any label (a block may
 * name a label the table no longer has; the generator reports it). */
class PinLabelField extends Blockly.FieldDropdown {
  private pending: string | null = null;
  static override fromJson(): PinLabelField {
    return new PinLabelField();
  }
  constructor() {
    super(() => [["LED", "LED"]]);
  }
  override getOptions(): Blockly.MenuOption[] {
    const opts: Blockly.MenuOption[] = labelSource.table.map((l) => [l, l]);
    const current = this.pending ?? (this.getValue() as string | null);
    for (const l of [...labelSource.extra, current ?? ""]) {
      if (l && !labelSource.table.includes(l) && !opts.some((o) => o[1] === l)) opts.push([`${l} (not in Pins)`, l]);
    }
    return opts.length > 0 ? opts : [["LED", "LED"]];
  }
  protected override doClassValidation_(v?: string): string | null {
    if (typeof v !== "string" || v === "") return null;
    this.pending = v;
    return v;
  }
}

/** Where the machine fields get their options: the payload's machines,
 * plus references blocks already make (kept, shown as missing). */
const machineSource = { machines: [] as readonly Machine[], extra: { states: [] as string[], events: [] as string[] } };

/** A dropdown of `<machine id>.<state>` (or `.<event>`) references that
 * accepts any reference (the generator reports one the tables lack). */
class MachineRefField extends Blockly.FieldDropdown {
  private pending: string | null = null;
  constructor(private readonly kind: "state" | "event") {
    super(() => [["(no machine)", "Machine.Idle"]]);
  }
  override getOptions(): Blockly.MenuOption[] {
    const opts: Blockly.MenuOption[] = machineSource.machines.flatMap((m) =>
      (this.kind === "state" ? m.states : m.events).filter((x) => x.name).map((x): Blockly.MenuOption => [`${m.id}.${x.name}`, machineRef(m.id, x.name)]),
    );
    const current = this.pending ?? (this.getValue() as string | null);
    const extra = this.kind === "state" ? machineSource.extra.states : machineSource.extra.events;
    for (const v of [...extra, current ?? ""]) {
      if (v && !opts.some((o) => o[1] === v)) opts.push([`${v} (not in States)`, v]);
    }
    return opts.length > 0 ? opts : [["(add a machine in States)", this.kind === "state" ? "Machine.Idle" : "Machine.GO"]];
  }
  protected override doClassValidation_(v?: string): string | null {
    if (typeof v !== "string" || v === "") return null;
    this.pending = v;
    return v;
  }
}
class MachineStateField extends MachineRefField {
  static override fromJson(): MachineStateField {
    return new MachineStateField();
  }
  constructor() {
    super("state");
  }
}
class MachineEventField extends MachineRefField {
  static override fromJson(): MachineEventField {
    return new MachineEventField();
  }
  constructor() {
    super("event");
  }
}

let registered = false;
function registerBlocks(): void {
  if (registered) return;
  registered = true;
  Blockly.setLocale(En as unknown as Record<string, string>);
  Blockly.fieldRegistry.register("field_pin_label", PinLabelField);
  Blockly.fieldRegistry.register("field_machine_state", MachineStateField);
  Blockly.fieldRegistry.register("field_machine_event", MachineEventField);
  Blockly.defineBlocksWithJsonArray(BLOCK_DEFS as never[]);
}

const START_BOARD_KEY = "start-board";
/** The board for a payload that does not name one: the Start tab's pick,
 * else the ESP32. */
function fallbackBoard(): BoardId {
  try {
    const b = localStorage.getItem(START_BOARD_KEY);
    if (isTargetId(b) && b !== "badge") return b;
  } catch {
    /* storage unavailable */
  }
  return "esp32";
}

function themes(): { light: Blockly.Theme; dark: Blockly.Theme } {
  const base = Blockly.Themes.Classic;
  const light = Blockly.Theme.defineTheme("mw-light", {
    name: "mw-light",
    base,
    componentStyles: { workspaceBackgroundColour: "#ffffff", toolboxBackgroundColour: "#f7f7f5", flyoutBackgroundColour: "#ececea" },
  });
  const dark = Blockly.Theme.defineTheme("mw-dark", {
    name: "mw-dark",
    base,
    componentStyles: {
      workspaceBackgroundColour: "#17181a",
      toolboxBackgroundColour: "#212226",
      toolboxForegroundColour: "#e8e8ea",
      flyoutBackgroundColour: "#2a2b30",
      flyoutForegroundColour: "#e8e8ea",
      flyoutOpacity: 1,
      scrollbarColour: "#5a5b62",
      insertionMarkerColour: "#ffffff",
      cursorColour: "#e8e8ea",
    },
  });
  return { light, dark };
}

export function blocksPage(app: App): HTMLElement {
  const { proto } = app;
  const store = browserStore();
  const saved = loadPlant(store, fallbackBoard());
  /** The plant (IR v4): every unit, the phases and the recipes. The
   * selected unit's payload is `model`, written back by `syncPlant()`. */
  let plant: PlantModel =
    saved?.plant ??
    plantFromPayload(
      (() => {
        const blink = exampleWorkspace("blink");
        const board = fallbackBoard();
        return blink ? exampleModel(blink, makeModel(board, {}, boardDefaults(board)), board) : makeModel(board, {});
      })(),
    );
  let unitId: string = (() => {
    try {
      const id = localStorage.getItem(UNIT_KEY);
      if (id && plant.physical.units.some((u) => u.id === id)) return id;
    } catch {
      /* storage unavailable */
    }
    return plant.physical.units[0]?.id ?? "";
  })();
  /** The selected unit's payload as last synced; the workspace part is
   * refreshed from Blockly by `current()`. */
  let model: PayloadModel = unitPayload(plant, unitId) as PayloadModel;
  let plantIssueList: PlantIssue[] = [];
  /** The node id the connected device reported, if any. */
  let deviceNode: string | null = null;
  let profile: ProfileId = profileOfModel(model);
  let pinIssues: PinIssue[] = [];
  let machineIssues: MachineIssue[] = [];
  let flowIssues: FlowIssue[] = [];
  /** Each flow's latest run in the simulator, by id (null: not running). */
  let flowRuns: Map<string, readonly string[]> | null = null;
  /** Each machine's state in the running simulator, by id (null: none). */
  let simStates: Map<string, string> | null = null;
  let boardPins: BoardPins | null = null;
  let compiled: Uint8Array | null = null;
  let session: SimSession | null = null;
  let ws: Blockly.WorkspaceSvg | null = null;
  let saveTimer: ReturnType<typeof setTimeout> | null = null;
  /** Set while the code view and block warnings are refreshed, so the
   * change events that causes are not handled again. */
  let regenerating = false;

  // ---- controls
  const profileSel = h("select", {}, ...TARGET_IDS.map((id) => h("option", { value: id }, targetLabel(id))));
  profileSel.value = model.board;
  const profileNote = h("span", { class: "muted" }, PROFILES[profile].note);
  const exampleSel = h(
    "select",
    { class: "examples", "aria-label": "Load an example" },
    h("option", { value: "" }, "Examples..."),
    ...EXAMPLES.map((e) => h("option", { value: e.id }, `${e.label} (${PROFILES[e.profile].label})`)),
    h("option", { value: PLANT_EXAMPLE }, "Plant: Blink then Measure (two units, S88 phases)"),
  );
  const compileBtn = h("button", { class: "primary" }, "Compile");
  const simBtn = h("button", { disabled: true }, "Simulate");
  const stopSimBtn = h("button", { disabled: true }, "Stop");
  const uploadBtn = h("button", { disabled: true }, "Upload to device");
  const downloadBtn = h("button", { disabled: true }, "Download .wasm");
  const exportBtn = h("button", {}, "Export JSON");
  const importFile = h("input", { type: "file", accept: ".json,application/json", hidden: true });
  const importBtn = h("button", {}, "Import JSON");
  const out = h("pre", { class: "log", "aria-live": "polite" }, "Compile to see errors, size and imports.");
  const issuesEl = h("ul", { class: "issues" });

  const code = new EditorView({
    doc: "",
    extensions: [basicSetup, javascript({ typescript: true }), EditorState.readOnly.of(true), EditorView.editable.of(false)],
  });

  const area = h("div", { class: "blockly-area" });
  const toolboxBtn = h("button", { type: "button", class: "toolbox-toggle", "aria-pressed": "false" }, "Hide categories");
  toolboxBtn.addEventListener("click", () => {
    const tb = ws?.getToolbox() as Blockly.Toolbox | null | undefined;
    if (!ws || !tb) return;
    const hide = toolboxBtn.getAttribute("aria-pressed") !== "true";
    tb.setVisible(!hide);
    toolboxBtn.setAttribute("aria-pressed", String(hide));
    toolboxBtn.textContent = hide ? "Show categories" : "Hide categories";
    Blockly.svgResize(ws);
  });

  function currentWorkspace(): WorkspaceJson {
    return ws ? (Blockly.serialization.workspaces.save(ws) as WorkspaceJson) : model.logic[0].workspace;
  }
  /** The payload as it stands (workspace read from Blockly). */
  function current(): PayloadModel {
    model = withWorkspace(model, currentWorkspace());
    return model;
  }
  /** The plant with the selected unit's payload as it stands. */
  function syncPlant(): PlantModel {
    plant = withUnitPayload(plant, unitId, current());
    return plant;
  }

  /** Re-checks the pin table (protocol-wasm checkPins) and redraws it. */
  function checkPins() {
    const chip = chipOf(model.board);
    try {
      boardPins = chip ? proto.boardPins(chip) : null;
      pinIssues = checkTable(proto.checkPins, model.board, model.pins);
    } catch (e) {
      boardPins = null;
      pinIssues = [{ row: null, field: "table", message: `Could not check the pins: ${errorText(e)}` }];
    }
    labelSource.table = model.pins.rows.map((r) => r.label).filter(Boolean);
    pinsView.render(model.board, model.pins, pinIssues, boardPins);
    pinsTab.textContent = pinIssues.length ? `Pins (${pinIssues.length} to fix)` : "Pins";
    checkMachines();
  }

  /** Re-checks the machines against the pin table (protocol-wasm
   * checkMachines) and redraws the States view. */
  function checkMachines() {
    try {
      machineIssues = model.machines.length ? proto.checkMachines(model.machines, model.pins.rows) : [];
    } catch (e) {
      machineIssues = [
        { machine: 0, list: "machine", row: null, field: "size", severity: "error", message: `Could not check the state machines: ${errorText(e)}` },
      ];
    }
    machineSource.machines = model.machines;
    renderStates();
    statesTab.textContent = statesTabText(machineIssues);
    checkFlows();
  }

  /** Re-checks the flows against the machines and the pin table
   * (protocol-wasm checkFlows) and redraws the Flows view. */
  function checkFlows() {
    const flows = flowsOf(model);
    try {
      flowIssues = flows.length ? proto.checkFlows(flows, model.machines, model.pins.rows, phaseDecls(plant, unitId)) : [];
    } catch (e) {
      flowIssues = [{ flow: 0, node: null, wire: null, severity: "error", message: `Could not check the flows: ${errorText(e)}` }];
    }
    renderFlows();
    flowsTab.textContent = flowsTabText(flowIssues);
  }

  function renderFlows() {
    flowsV.render(
      flowsOf(model),
      flowIssues,
      model.machines,
      model.pins.rows.map((r) => ({ label: r.label, func: r.func })),
      flowRuns,
      phaseDecls(plant, unitId),
    );
  }

  function setFlows(flows: readonly Flow[]) {
    model = withFlows(model, flows);
    checkFlows();
    invalidate();
    regenerate();
    persist();
  }

  function renderStates() {
    statesV.render(
      model.machines,
      machineIssues,
      model.pins.rows.map((r) => ({ label: r.label, func: r.func })),
      simStates,
    );
  }

  function setMachines(machines: readonly Machine[], rename?: Rename) {
    model = withMachines(model, machines);
    if (rename) model = withFlows(model, renameFlowRef(flowsOf(model), rename.kind, rename.id, rename.from, rename.to));
    machineSource.machines = machines;
    if (rename && ws) {
      Blockly.Events.setGroup(true);
      try {
        for (const b of ws.getAllBlocks(false)) {
          const field = MACHINE_STATE_BLOCKS.includes(b.type) ? "STATE" : MACHINE_EVENT_BLOCKS.includes(b.type) ? "EVENT" : null;
          if (!field) continue;
          if (rename.kind !== "machine" && (rename.kind === "event") !== (field === "EVENT")) continue;
          const f = b.getField(field);
          const ref = parseMachineRef(String(f?.getValue() ?? ""));
          if (!f || !ref) continue;
          if (rename.kind === "machine") {
            if (ref.id === rename.from) f.setValue(machineRef(rename.to, ref.name));
          } else if (ref.id === rename.id && ref.name === rename.from) f.setValue(machineRef(rename.id, rename.to));
        }
      } finally {
        Blockly.Events.setGroup(false);
      }
    }
    refreshMachineFields();
    checkMachines();
    invalidate();
    regenerate();
    persist();
  }

  /** Redraws the machine blocks' dropdown text (a name may have become
   * missing or present again). */
  function refreshMachineFields() {
    if (!ws) return;
    for (const b of ws.getAllBlocks(false)) {
      const field = MACHINE_STATE_BLOCKS.includes(b.type) ? "STATE" : MACHINE_EVENT_BLOCKS.includes(b.type) ? "EVENT" : null;
      if (field) b.getField(field)?.forceRerender();
    }
  }

  function setPins(pins: PinTable, rename?: { from: string; to: string }) {
    model = withPins(model, pins);
    // The machines' guards and pin actions follow a label rename too.
    if (rename) model = withMachines(model, renamePinLabel(model.machines, rename.from, rename.to));
    if (rename) model = withFlows(model, renameFlowPin(flowsOf(model), rename.from, rename.to));
    if (rename) plant = renameCm(plant, unitId, rename.from, rename.to);
    if (rename && ws) {
      Blockly.Events.setGroup(true);
      try {
        labelSource.table = pins.rows.map((r) => r.label).filter(Boolean);
        for (const b of ws.getAllBlocks(false)) {
          if (!PIN_LABEL_BLOCKS.includes(b.type)) continue;
          const f = b.getField("LABEL");
          if (f && f.getValue() === rename.from) f.setValue(rename.to);
        }
      } finally {
        Blockly.Events.setGroup(false);
      }
    }
    checkPins();
    invalidate();
    regenerate();
    persist();
  }

  const pinsView = pinTableView({ onChange: setPins });
  const statesV = statesView({
    onChange: setMachines,
    onAddPin: (label: string, func: PinFunc) => {
      setPins(withNeeds(model.pins, model.board, [{ label, func }]));
      out.textContent = `Added pin ${label} to the pin table; choose its GPIO in Pins.`;
    },
  });

  const flowsV = flowsView({
    onChange: setFlows,
    onMachines: (machines) => setMachines(machines),
    onAddPin: (label: string, func: PinFunc) => {
      setPins(withNeeds(model.pins, model.board, [{ label, func }]));
      out.textContent = `Added pin ${label} to the pin table; choose its GPIO in Pins.`;
    },
  });

  function invalidate() {
    if (compiled) out.textContent += "\n(The workspace or board changed; compile again.)";
    compiled = null;
    simBtn.disabled = uploadBtn.disabled = downloadBtn.disabled = true;
  }

  function showIssues(errors: GenIssue[], warnings: GenIssue[]) {
    if (ws) for (const b of ws.getAllBlocks(false)) b.setWarningText(null);
    const items = [...errors.map((e) => ["error", e] as const), ...warnings.map((w) => ["warning", w] as const)];
    issuesEl.replaceChildren(
      ...items.map(([kind, issue]) => {
        const block = issue.blockId ? ws?.getBlockById(issue.blockId) : null;
        block?.setWarningText(issue.message);
        const li = h("li", { class: kind }, `${kind}: ${issue.message}`);
        if (block) {
          li.classList.add("link");
          li.addEventListener("click", () => {
            ws?.centerOnBlock(block.id);
            block.select();
          });
        }
        return li;
      }),
    );
  }

  /** Regenerates the code view from the workspace. */
  function regenerate() {
    regenerating = true;
    try {
      const g = generate(currentWorkspace(), profile, model.pins, model.machines, flowsOf(model), phaseSpecs(plant, unitId));
      code.dispatch({ changes: { from: 0, to: code.state.doc.length, insert: g.source } });
      showIssues(g.errors, g.warnings);
    } finally {
      regenerating = false;
    }
  }

  function persist() {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => savePlant(store, syncPlant()), 300);
    renderPlant();
  }

  // ---- the plant: units and phases
  const unitSel = h("select", { "aria-label": "Unit the views edit" });
  function renderUnitSel() {
    unitSel.replaceChildren(...plant.physical.units.map((u) => h("option", { value: u.id, selected: u.id === unitId }, u.id || "(no id)")));
    unitSel.value = unitId;
  }
  function renderPlant() {
    plant = withUnitPayload(plant, unitId, model);
    plantIssueList = plantIssues(plant);
    unitsV.render(plant, unitId, plantIssueList, deviceNode);
    phasesV.render(plant, unitId, plantIssueList, proto.phaseModel ? proto.phaseModel() : null);
    renderUnitSel();
    const units = plantIssueList.filter((i) => i.place === "units").length;
    const mine = new Set(plant.procedural.phases.flatMap((p, i) => (p.unit === unitId ? [i] : [])));
    const phases = plantIssueList.filter(
      (i) => (i.place === "ems" && i.unit === unitId) || i.place === "classes" || i.place === "params" || ((i.place === "phases" || i.place === "bodies") && mine.has(i.row)),
    ).length;
    unitsTab.textContent = units ? `Units (${units} to fix)` : "Units";
    phasesTab.textContent = phases ? `Phases (${phases} to fix)` : "Phases";
    recipeV.render(plant, deviceNode);
    recipeTab.textContent = recipeTabText(recipeV.issues(plant));
  }
  /** Switches the Blocks, Pins, States, Flows and Phases views to unit `id`. */
  function selectUnit(id: string) {
    if (id === unitId || !plant.physical.units.some((u) => u.id === id)) {
      renderPlant();
      return;
    }
    syncPlant();
    unitId = id;
    try {
      localStorage.setItem(UNIT_KEY, id);
    } catch {
      /* storage unavailable */
    }
    loadModel(unitPayload(plant, id) as PayloadModel);
    renderBadge();
  }
  /** A plant edit from the Units or Phases view. */
  function setPlant(next: PlantModel) {
    syncPlant();
    const before = JSON.stringify(unitPayload(plant, unitId));
    plant = next;
    if (!plant.physical.units.some((u) => u.id === unitId)) unitId = plant.physical.units[0]?.id ?? "";
    const m = unitPayload(plant, unitId) as PayloadModel;
    if (JSON.stringify(m) !== before) loadModel(m);
    else {
      checkFlows();
      invalidate();
      regenerate();
      persist();
    }
  }
  const plantEvents = {
    onChange: setPlant,
    onSelect: (id: string) => selectUnit(id),
    onOpenFlow: (id: string) => {
      flowsV.select(id);
      showView("flows");
    },
  };
  const unitsV = unitsView(plantEvents);
  const phasesV = phasesView(plantEvents);
  const recipeV = recipeView(proto, { onChange: setPlant });
  unitSel.addEventListener("change", () => selectUnit(unitSel.value));
  app.onDeviceInfo((i) => {
    deviceNode = i ? nodeFromInfo(i.node) : null;
    renderPlant();
  });

  /** Disables blocks the profile does not support (and re-enables the
   * ones it does), and swaps the toolbox. */
  function applyProfile() {
    profileNote.textContent = PROFILES[profile].note;
    ws?.updateToolbox(toolboxFor(profile));
    markBlocks();
  }

  function markBlocks() {
    if (ws) for (const b of ws.getAllBlocks(false)) b.setDisabledReason(!blockSupported(b.type, profile), PROFILE_REASON);
  }

  function loadModel(m: PayloadModel) {
    model = m;
    profile = profileOfModel(m);
    profileSel.value = m.board;
    labelSource.extra = labelsUsed(m.logic[0].workspace);
    labelSource.table = m.pins.rows.map((r) => r.label).filter(Boolean);
    machineSource.machines = m.machines;
    machineSource.extra = machineRefsUsed(m.logic[0].workspace);
    if (ws) {
      Blockly.Events.disable();
      try {
        ws.clear();
        Blockly.serialization.workspaces.load(m.logic[0].workspace, ws);
      } finally {
        Blockly.Events.enable();
      }
      applyProfile();
    }
    checkPins();
    invalidate();
    regenerate();
    persist();
  }

  /** The board changed (here or on the Start tab). */
  function setBoard(board: TargetId) {
    if (board === model.board) return;
    model = withPins(withBoard(current(), board), seedForTarget(model.pins, model.board, board));
    profile = profileOfModel(model);
    profileSel.value = board;
    applyProfile();
    checkPins();
    invalidate();
    regenerate();
    persist();
    renderBadge();
  }

  function inject() {
    if (ws) return;
    registerBlocks();
    const { light, dark } = themes();
    const dark_ = window.matchMedia?.("(prefers-color-scheme: dark)");
    ws = Blockly.inject(area, {
      toolbox: toolboxFor(profile),
      theme: dark_?.matches ? dark : light,
      trashcan: true,
      zoom: { controls: true, wheel: true, startScale: 0.9 },
      grid: { spacing: 24, length: 2, colour: "#8884", snap: true },
      move: { scrollbars: true, drag: true, wheel: false },
    });
    dark_?.addEventListener("change", (e) => ws?.setTheme(e.matches ? dark : light));
    ws.addChangeListener(Blockly.Events.disableOrphans);
    labelSource.extra = labelsUsed(model.logic[0].workspace);
    machineSource.machines = model.machines;
    machineSource.extra = machineRefsUsed(model.logic[0].workspace);
    try {
      Blockly.serialization.workspaces.load(model.logic[0].workspace, ws);
    } catch (e) {
      out.textContent = `Could not restore the saved workspace: ${errorText(e)}`;
    }
    applyProfile();
    ws.addChangeListener((e: Blockly.Events.Abstract) => {
      if (e.isUiEvent || regenerating) return;
      if (e.type === Blockly.Events.BLOCK_CREATE) markBlocks();
      invalidate();
      regenerate();
      persist();
    });
    new ResizeObserver(() => ws && Blockly.svgResize(ws)).observe(area);
    checkPins();
    regenerate();
    renderPlant();
    if (saved?.migrated) out.textContent = `Your saved payload is now unit ${unitId} of a plant (Units view); it generates the same code.`;
  }
  // Blockly needs its container in the document; main.ts appends the page
  // right after building it.
  queueMicrotask(inject);

  profileSel.addEventListener("change", () => {
    if (isTargetId(profileSel.value)) setBoard(profileSel.value);
  });
  app.onBoardChosen((id) => setBoard(id));

  /** True when `next` can replace the payload: nothing would be lost, or
   * the user confirms replacing it with `what`. */
  async function mayReplace(next: PayloadModel, what: string): Promise<boolean> {
    if (!wouldReplaceWork(current(), next)) return true;
    return (await confirmReplace(what)) !== null;
  }
  function confirmReplace(what: string) {
    return modal(
      `Replace your payload with ${what}?`,
      [h("p", {}, "Your blocks and state machines are replaced (the pin table keeps your pins). Export JSON first to keep them.")],
      "Replace",
    );
  }

  exampleSel.addEventListener("change", async () => {
    if (exampleSel.value === PLANT_EXAMPLE) {
      exampleSel.value = "";
      const next = parsePlant(serializePlant(EXAMPLE_PLANT), fallbackBoard());
      if (!next.ok) return;
      if (serializePlant(syncPlant()) !== serializePlant(next.plant) && (await confirmReplace("the Blink then Measure plant")) === null) {
        out.textContent = "Kept your plant; the example plant was not loaded.";
        return;
      }
      plant = next.plant;
      unitId = plant.physical.units[0]?.id ?? "";
      loadModel(unitPayload(plant, unitId) as PayloadModel);
      renderBadge();
      out.textContent = "Loaded the Blink then Measure plant: units pico-led and esp-sense (Units view), their phases (Phases view).";
      return;
    }
    const e = exampleWorkspace(exampleSel.value);
    if (!e) {
      exampleSel.value = "";
      return;
    }
    const next = exampleModel(e, current(), fallbackBoard());
    const ok = await mayReplace(next, e.label);
    // The menu is a launcher: back to its placeholder whether loaded or not.
    exampleSel.value = "";
    if (!ok) {
      out.textContent = `Kept your blocks; the ${e.label} example was not loaded.`;
      return;
    }
    loadModel(next);
    renderBadge();
    out.textContent = `Loaded the ${e.label} example.`;
  });

  exportBtn.addEventListener("click", () => {
    const url = URL.createObjectURL(new Blob([serializePlant(syncPlant())], { type: "application/json" }));
    h("a", { href: url, download: "plant.json" }).click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });
  importBtn.addEventListener("click", () => importFile.click());
  importFile.addEventListener("change", async () => {
    const f = importFile.files?.[0];
    importFile.value = "";
    if (!f) return;
    const text = await f.text();
    const whole = (() => {
      try {
        const v = JSON.parse(text) as { version?: unknown };
        return v !== null && typeof v === "object" && v.version === 4;
      } catch {
        return false;
      }
    })();
    if (whole) {
      const r = parsePlant(text, fallbackBoard());
      if (!r.ok) {
        out.textContent = `Import failed: ${r.error}`;
        return;
      }
      if (serializePlant(syncPlant()) !== serializePlant(r.plant) && (await confirmReplace(f.name)) === null) {
        out.textContent = `Kept your plant; ${f.name} was not imported.`;
        return;
      }
      plant = r.plant;
      unitId = plant.physical.units[0]?.id ?? "";
      loadModel(unitPayload(plant, unitId) as PayloadModel);
      renderBadge();
      out.textContent = `Imported ${f.name}: ${plant.physical.units.length} unit${plant.physical.units.length === 1 ? "" : "s"}.`;
      return;
    }
    const plan = planImport(text, current(), fallbackBoard());
    if (plan.kind === "error") {
      out.textContent = `Import failed: ${plan.error}`;
      return;
    }
    if (plan.kind === "confirm" && (await confirmReplace(f.name)) === null) {
      out.textContent = `Kept your blocks; ${f.name} was not imported.`;
      return;
    }
    try {
      loadModel(plan.model);
      renderBadge();
      out.textContent = `Imported ${f.name}.`;
    } catch (e) {
      out.textContent = `Import failed: ${errorText(e)}`;
    }
  });

  /** Compiles the workspace; true when `compiled` holds the module. */
  async function compile(): Promise<boolean> {
    compileBtn.disabled = true;
    out.textContent = "Compiling (loading the AssemblyScript compiler the first time) ...";
    try {
      const m = current();
      renderPlant();
      const mine = new Set(plant.procedural.phases.flatMap((p, i) => (p.unit === unitId ? [i] : [])));
      const phaseProblems = plantIssueList.filter(
        (i) => (i.place === "ems" && i.unit === unitId) || ((i.place === "phases" || i.place === "bodies") && mine.has(i.row)) || i.place === "classes" || i.place === "params",
      );
      if (phaseProblems.length > 0) {
        invalidate();
        out.textContent = ["phases failed:", ...phaseProblems.map((i) => `Phases: ${i.message}`)].join("\n");
        showView("phases");
        return false;
      }
      const r = await buildPayload(m.logic[0].workspace, profile, proto.abi, {
        pins: m.pins,
        issues: pinIssues,
        machines: m.machines,
        machineIssues,
        flows: flowsOf(m),
        flowIssues,
        phases: phaseSpecs(plant, unitId),
      });
      if (!r.ok) {
        invalidate();
        out.textContent = [`${r.stage} failed:`, ...r.errors].join("\n");
        if (r.issues.length > 0) showIssues(r.issues, []);
        if (r.stage === "pins") showView("pins");
        if (r.stage === "states") showView("states");
        if (r.stage === "flows") showView("flows");
        return false;
      }
      compiled = r.wasm;
      out.textContent = [
        `OK: ${r.wasm.length} bytes, ${r.memoryBytes} bytes of linear memory${r.memoryBytes < 65536 ? " (1-byte pages)" : ""}.`,
        `Imports: ${r.report.imports.join(", ") || "none"}`,
        `Exports: ${r.report.exports.join(", ")}`,
        ...r.report.warnings.map((w) => `ABI warning: ${w}`),
        r.log,
      ]
        .filter(Boolean)
        .join("\n");
      simBtn.disabled = !proto.hasSimulator;
      uploadBtn.disabled = downloadBtn.disabled = false;
      return true;
    } catch (e) {
      out.textContent = `Compiler failed: ${errorText(e)}`;
      return false;
    } finally {
      compileBtn.disabled = false;
    }
  }
  compileBtn.addEventListener("click", () => void compile());

  downloadBtn.addEventListener("click", () => {
    if (!compiled) return;
    const url = URL.createObjectURL(new Blob([compiled as BlobPart], { type: "application/wasm" }));
    h("a", { href: url, download: `blocks-${profile}.wasm` }).click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });

  uploadBtn.addEventListener("click", () => {
    if (!compiled) return;
    app.offerPayload({ name: `blocks-${profile}.wasm`, wasm: compiled });
    app.showTab("device");
  });

  // ---- simulator
  const pinsEl = h("div", { class: "pins" });
  const ledsEl = h("div", { class: "pins badge-leds" });
  const screenEl = h("code", {}, "(nothing shown)");
  const petEl = h("span", {}, "-");
  const buttonsEl = h("div", { class: "row" });
  const badgeEl = h(
    "div",
    {},
    h("h3", {}, "Badge"),
    ledsEl,
    h("p", {}, "Screen ", screenEl, h("br"), "Pet mode ", petEl),
    buttonsEl,
  );
  const msgLog = h("pre", { class: "log sim-log" });
  const simStateEl = h("p", { id: "sim-states", class: "sim-states", "aria-live": "polite" });
  const simFlowsEl = h("p", { id: "sim-flows", class: "sim-states" });
  const subsEl = h("span", { class: "muted" });
  const simKey = h("input", { type: "text", value: "app/echo/in" });
  const simData = h("input", { type: "text", value: "ping" });
  const simSend = h("button", { type: "submit", disabled: true }, "Deliver");
  const simForm = h("form", { class: "row" }, h("label", {}, "Key ", simKey), h("label", {}, "Payload ", simData), simSend);

  const logLine = (s: string) => {
    const t = session ? `${(session.nowMs() / 1000).toFixed(2)}s` : "";
    msgLog.textContent += `${t.padStart(8)} ${s}\n`;
    msgLog.scrollTop = msgLog.scrollHeight;
  };

  function renderPins(pins: ReadonlyMap<number, PinState>) {
    pinsEl.replaceChildren(
      ...[...pins.entries()]
        .sort(([a], [b]) => a - b)
        .map(([pin, s]) => {
          const label = model.pins.rows.find((r) => r.pin === pin && r.label)?.label;
          const name = label ? `${label} (GPIO ${pin})` : `GPIO ${pin}`;
          if (s.mode === "output") return h("div", { class: "pin" }, h("span", { class: `led${s.high ? " on" : ""}` }), name);
          const sw = h("input", { type: "checkbox", checked: !!s.high });
          sw.addEventListener("change", () => session?.setInput(pin, sw.checked));
          return h("label", { class: "pin" }, h("span", { class: "switch" }, sw), `${name} (pull ${s.pull})`);
        }),
    );
    if (pins.size === 0) pinsEl.append(h("span", { class: "muted" }, "No pins configured."));
  }

  function renderLeds(rgb: Uint8Array) {
    ledsEl.replaceChildren(
      ...[...Array(8).keys()].map((i) => {
        const [r, g, b] = [rgb[i * 3] ?? 0, rgb[i * 3 + 1] ?? 0, rgb[i * 3 + 2] ?? 0];
        const lit = r + g + b > 0;
        const led = h("span", { class: "led", title: `LED ${i}: ${r} ${g} ${b}` });
        if (lit) {
          led.style.background = `rgb(${r}, ${g}, ${b})`;
          led.style.boxShadow = `0 0 8px rgb(${r}, ${g}, ${b})`;
        }
        return led;
      }),
    );
  }

  function renderBadge() {
    badgeEl.hidden = profile !== "badge";
  }

  function pressButton(n: number) {
    if (!session) return;
    for (const pressed of [1, 0]) {
      const hit = session.deliver("badge/buttons", Uint8Array.of(n, pressed));
      logLine(`in   badge/buttons [${n}, ${pressed}]${hit ? "" : " (no subscription)"}`);
    }
  }
  buttonsEl.append(
    ...BUTTONS.map(([n, label]) => {
      const b = h("button", { type: "button" }, label);
      b.addEventListener("click", () => pressButton(n));
      return b;
    }),
  );

  function onEvent(e: SimEvent) {
    if (e.kind === "log") {
      // A flow's node lines drive the Flows highlight and the "Flows:"
      // line; listing them would bury every other line (a 250 ms flow
      // logs eight a second).
      if (!parseFlowLog(e.line)) logLine(`log  ${e.line}`);
      const p = parseMachineLog(e.line);
      if (p) {
        simStates = new Map(simStates ?? []).set(p.id, p.state);
        simStateEl.textContent = simStateText(p);
        renderStates();
      }
      if (parseFlowLog(e.line)) {
        const entries = new Map(flowsOf(model).map((f) => [f.id, f.nodes.find((n) => n.kind === "entry")?.id ?? ""] as const));
        flowRuns = flowTrail(flowRuns ?? new Map(), e.line, entries);
        simFlowsEl.textContent = flowTrailText(flowRuns);
        renderFlows();
      }
    }
    else if (e.kind === "publish") {
      logLine(`pub  ${e.key} ${formatBytes(e.data)}`);
      if (e.key === "badge/leds" && e.data.length === 24) renderLeds(e.data);
      else if (e.key === "badge/text") screenEl.textContent = new TextDecoder().decode(e.data);
      else if (e.key === "badge/pet" && e.data.length === 1) petEl.textContent = ["auto", "idle", "walk", "eat", "play", "sleep"][e.data[0] ?? 0] ?? "?";
    } else if (e.kind === "irq" && e.irq !== "Timer") logLine(`irq  ${e.irq} source ${e.source} value ${e.value}`);
  }

  function stopSim() {
    session?.dispose();
    session = null;
    stopSimBtn.disabled = simSend.disabled = true;
    if (simStates) {
      simStates = null;
      simStateEl.textContent = "";
      renderStates();
    }
    if (flowRuns) {
      flowRuns = null;
      simFlowsEl.textContent = "";
      renderFlows();
    }
  }

  function simulate() {
    if (!compiled) return;
    stopSim();
    msgLog.textContent = "";
    simStates = new Map();
    flowRuns = new Map();
    renderLeds(new Uint8Array(24));
    screenEl.textContent = "(nothing shown)";
    petEl.textContent = "-";
    try {
      session = new SimSession(proto, compiled, {
        onEvent,
        onPins: renderPins,
        onError: (m) => {
          logLine(`stopped: ${m}`);
          stopSimBtn.disabled = simSend.disabled = true;
        },
      });
      session.start();
      subsEl.textContent = `Subscriptions: ${session.subscriptions.join(", ") || "none"}`;
      stopSimBtn.disabled = simSend.disabled = false;
    } catch (e) {
      session = null;
      logLine(`load failed: ${errorText(e)}`);
    }
  }
  simBtn.addEventListener("click", simulate);

  // Start tab: load the example (asking first if that would replace other
  // work), compile, simulate and bring the simulated pins into view.
  app.onTryExample(async (id) => {
    const e = exampleWorkspace(id);
    if (!e) return;
    const next = exampleModel(e, current(), fallbackBoard());
    if (!(await mayReplace(next, e.label))) return;
    showView("blocks");
    loadModel(next);
    renderBadge();
    if (!(await compile())) return;
    if (!proto.hasSimulator) return;
    simulate();
    const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    simHeading.scrollIntoView({ behavior: reduce ? "auto" : "smooth", block: "start" });
  });
  stopSimBtn.addEventListener("click", () => {
    stopSim();
    logLine("stopped");
  });
  simForm.addEventListener("submit", (ev) => {
    ev.preventDefault();
    if (!session) return;
    try {
      const key = simKey.value.trim();
      const data = new TextEncoder().encode(simData.value);
      const hit = session.deliver(key, data);
      logLine(`in   ${key} ${formatBytes(data)}${hit ? "" : " (no subscription)"}`);
    } catch (e) {
      logLine(errorText(e));
    }
  });

  // ---- views: Blocks (Blockly) and Pins (the pin table)
  const unitsTab = h("button", { role: "tab", id: "view-units-tab", "aria-controls": "view-units", type: "button" }, "Units");
  const phasesTab = h("button", { role: "tab", id: "view-phases-tab", "aria-controls": "view-phases", type: "button" }, "Phases");
  const recipeTab = h("button", { role: "tab", id: "view-recipe-tab", "aria-controls": "view-recipe", type: "button" }, "Recipe");
  const blocksTab = h("button", { role: "tab", id: "view-blocks-tab", "aria-controls": "view-blocks", type: "button" }, "Blocks");
  const pinsTab = h("button", { role: "tab", id: "view-pins-tab", "aria-controls": "view-pins", type: "button" }, "Pins");
  const statesTab = h("button", { role: "tab", id: "view-states-tab", "aria-controls": "view-states", type: "button" }, "States");
  const flowsTab = h("button", { role: "tab", id: "view-flows-tab", "aria-controls": "view-flows", type: "button" }, "Flows");
  const blocksView = h(
    "div",
    { role: "tabpanel", id: "view-blocks", "aria-labelledby": "view-blocks-tab" },
    h("div", { class: "row workspace-head" }, h("h3", {}, "Workspace"), toolboxBtn),
    area,
    issuesEl,
  );
  const pinsPanel = h("div", { role: "tabpanel", id: "view-pins", "aria-labelledby": "view-pins-tab" }, pinsView.el);
  const statesPanel = h("div", { role: "tabpanel", id: "view-states", "aria-labelledby": "view-states-tab" }, statesV.el);
  const flowsPanel = h("div", { role: "tabpanel", id: "view-flows", "aria-labelledby": "view-flows-tab" }, flowsV.el);
  const unitsPanel = h("div", { role: "tabpanel", id: "view-units", "aria-labelledby": "view-units-tab" }, unitsV.el);
  const phasesPanel = h("div", { role: "tabpanel", id: "view-phases", "aria-labelledby": "view-phases-tab" }, phasesV.el);
  const recipePanel = h("div", { role: "tabpanel", id: "view-recipe", "aria-labelledby": "view-recipe-tab" }, recipeV.el);
  const VIEW_KEY = "blocks-view";
  type View = "units" | "blocks" | "pins" | "states" | "flows" | "phases" | "recipe";
  const VIEWS: readonly { v: View; tab: HTMLButtonElement }[] = [
    { v: "units", tab: unitsTab },
    { v: "blocks", tab: blocksTab },
    { v: "pins", tab: pinsTab },
    { v: "states", tab: statesTab },
    { v: "flows", tab: flowsTab },
    { v: "phases", tab: phasesTab },
    { v: "recipe", tab: recipeTab },
  ];
  function showView(v: View) {
    unitsPanel.hidden = v !== "units";
    phasesPanel.hidden = v !== "phases";
    recipePanel.hidden = v !== "recipe";
    blocksView.hidden = v !== "blocks";
    pinsPanel.hidden = v !== "pins";
    statesPanel.hidden = v !== "states";
    flowsPanel.hidden = v !== "flows";
    for (const [t, on] of VIEWS.map((x) => [x.tab, x.v === v] as const)) {
      t.setAttribute("aria-selected", String(on));
      t.tabIndex = on ? 0 : -1;
    }
    if (v === "blocks" && ws) Blockly.svgResize(ws);
    try {
      localStorage.setItem(VIEW_KEY, v);
    } catch {
      /* storage unavailable */
    }
  }
  blocksTab.addEventListener("click", () => showView("blocks"));
  pinsTab.addEventListener("click", () => showView("pins"));
  statesTab.addEventListener("click", () => showView("states"));
  flowsTab.addEventListener("click", () => showView("flows"));
  unitsTab.addEventListener("click", () => showView("units"));
  phasesTab.addEventListener("click", () => showView("phases"));
  recipeTab.addEventListener("click", () => showView("recipe"));
  const views = h("div", { role: "tablist", class: "subtabs", "aria-label": "Payload views" }, unitsTab, blocksTab, pinsTab, statesTab, flowsTab, phasesTab, recipeTab);
  // Arrow keys (wrapping), Home and End move between the view tabs (the
  // tabs pattern, selection follows focus).
  views.addEventListener("keydown", (e) => {
    const at = VIEWS.findIndex((x) => x.tab === document.activeElement);
    if (at < 0) return;
    const n = VIEWS.length;
    const to =
      e.key === "ArrowRight" ? (at + 1) % n : e.key === "ArrowLeft" ? (at + n - 1) % n : e.key === "Home" ? 0 : e.key === "End" ? n - 1 : -1;
    const next = VIEWS[to];
    if (!next) return;
    next.tab.focus();
    showView(next.v);
    e.preventDefault();
  });
  let savedView: string | null = null;
  try {
    savedView = localStorage.getItem(VIEW_KEY);
  } catch {
    /* storage unavailable */
  }
  showView(VIEWS.find((x) => x.v === savedView)?.v ?? "blocks");

  const simHeading = h("h3", {}, "Simulator");
  renderPins(new Map());
  renderLeds(new Uint8Array(24));
  renderBadge();

  return h(
    "section",
    { class: "page blocks" },
    h("h2", {}, "Blocks"),
    h(
      "p",
      { class: "row" },
      h("label", {}, "Unit ", unitSel),
      h("label", {}, "Board ", profileSel),
      exampleSel,
      compileBtn,
      simBtn,
      stopSimBtn,
      uploadBtn,
      downloadBtn,
      exportBtn,
      importBtn,
      importFile,
    ),
    h("p", {}, profileNote),
    h(
      "div",
      { class: "cols" },
      h("div", {}, views, unitsPanel, blocksView, pinsPanel, statesPanel, flowsPanel, phasesPanel, recipePanel),
      h(
        "div",
        {},
        h("h3", {}, "Generated AssemblyScript"),
        h("div", { class: "code generated" }, code.dom),
        h("h3", {}, "Build output"),
        out,
        simHeading,
        proto.hasSimulator
          ? h("p", { class: "muted" }, "Runs in wasm-host, the device runtime, compiled to WASM, with its default fuel per call (the badge grants less).")
          : h("p", { class: "note" }, "This build has no simulator."),
        pinsEl,
        badgeEl,
        subsEl,
        simStateEl,
        simFlowsEl,
        simForm,
        msgLog,
      ),
    ),
  );
}
