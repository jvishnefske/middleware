// The Blockly workspace as plain data: the shape of
// `Blockly.serialization.workspaces.save()`, read without Blockly so the
// generator, the examples and the import check run in node.
//
// Only the parts the generator reads are typed. Unknown properties are
// kept as they are, so a workspace round-trips through import and export.

/** A connection's target: a real block, a shadow, or both. */
export interface ConnectionJson {
  block?: BlockJson;
  shadow?: BlockJson;
}

/** One serialized block. */
export interface BlockJson {
  type: string;
  id?: string;
  x?: number;
  y?: number;
  fields?: Record<string, unknown>;
  inputs?: Record<string, ConnectionJson>;
  next?: ConnectionJson;
  extraState?: unknown;
  /** Blockly 10 and older. */
  enabled?: boolean;
  /** Blockly 11 and newer: why the block is disabled; empty or absent
   * means enabled. */
  disabledReasons?: string[];
  [extra: string]: unknown;
}

export interface VariableJson {
  name: string;
  id: string;
  type?: string;
}

/** A serialized workspace. */
export interface WorkspaceJson {
  blocks?: { languageVersion?: number; blocks?: BlockJson[] };
  variables?: VariableJson[];
  [extra: string]: unknown;
}

/** The block connected to `c`: the real block if there is one, else the
 * shadow. */
export function target(c: ConnectionJson | undefined): BlockJson | undefined {
  return c?.block ?? c?.shadow;
}

export function input(b: BlockJson, name: string): BlockJson | undefined {
  return target(b.inputs?.[name]);
}

export function nextBlock(b: BlockJson): BlockJson | undefined {
  return target(b.next);
}

export function isDisabled(b: BlockJson): boolean {
  return b.enabled === false || (Array.isArray(b.disabledReasons) && b.disabledReasons.length > 0);
}

export function topBlocks(ws: WorkspaceJson): BlockJson[] {
  return ws.blocks?.blocks ?? [];
}

/** A field's value as a string ("" if absent). */
export function fieldText(b: BlockJson, name: string): string {
  const v = b.fields?.[name];
  return typeof v === "string" ? v : typeof v === "number" ? String(v) : "";
}

/** A numeric field, or NaN if absent or not a number. */
export function fieldNumber(b: BlockJson, name: string): number {
  const v = b.fields?.[name];
  if (typeof v === "number") return v;
  if (typeof v === "string" && v.trim() !== "") return Number(v);
  return Number.NaN;
}

/** The variable a `VAR` field names: `{id}` (with the workspace's variable
 * list) or `{name}` when variables were saved inline. */
export function variableName(ws: WorkspaceJson, b: BlockJson, field = "VAR"): string | null {
  const v = b.fields?.[field];
  if (!v || typeof v !== "object") return null;
  const ref = v as { id?: unknown; name?: unknown };
  if (typeof ref.id === "string") {
    const found = ws.variables?.find((x) => x.id === ref.id);
    if (found) return found.name;
  }
  return typeof ref.name === "string" ? ref.name : null;
}

/** Walks every block (top blocks, inputs, next) once. Iterative, with an
 * explicit stack, so a deep stack of blocks cannot overflow the JS stack. */
export function everyBlock(ws: WorkspaceJson): BlockJson[] {
  const out: BlockJson[] = [];
  const stack = [...topBlocks(ws)];
  while (stack.length > 0) {
    const b = stack.pop();
    if (!b) continue;
    out.push(b);
    for (const c of Object.values(b.inputs ?? {})) {
      if (c.block) stack.push(c.block);
      if (c.shadow) stack.push(c.shadow);
    }
    const n = nextBlock(b);
    if (n) stack.push(n);
  }
  return out;
}

/** Checks that `value` looks like a serialized workspace before it is
 * handed to Blockly (import from a file). Returns the problem, or null. */
export function workspaceProblem(value: unknown): string | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return "not a JSON object";
  const ws = value as WorkspaceJson;
  if (ws.blocks !== undefined) {
    if (typeof ws.blocks !== "object" || ws.blocks === null) return "\"blocks\" must be an object";
    if (ws.blocks.blocks !== undefined && !Array.isArray(ws.blocks.blocks)) return "\"blocks.blocks\" must be an array";
    for (const b of ws.blocks.blocks ?? []) {
      if (!b || typeof b !== "object" || typeof (b as BlockJson).type !== "string") return "every block needs a \"type\"";
    }
  }
  if (ws.variables !== undefined && !Array.isArray(ws.variables)) return "\"variables\" must be an array";
  return null;
}
