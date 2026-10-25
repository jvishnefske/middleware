// Tiny DOM helpers shared by the pages.

type Child = Node | string | null | undefined | false;
type Attrs = Record<string, string | number | boolean | EventListener | undefined>;

/** Creates an element. `on*` attributes become event listeners; booleans
 * toggle the attribute. */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === false) continue;
    if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v);
    else if (v === true) el.setAttribute(k, "");
    else el.setAttribute(k, String(v));
  }
  for (const c of children) if (c) el.append(c);
  return el;
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Printable ASCII/UTF-8 text, or space-separated hex. */
export function formatBytes(b: Uint8Array): string {
  const text = new TextDecoder("utf-8", { fatal: false }).decode(b);
  const printable = /^[\x20-\x7e\t\r\n -￿]*$/.test(text) && !text.includes("�");
  return printable ? JSON.stringify(text) : [...b].map((x) => x.toString(16).padStart(2, "0")).join(" ");
}

/** Parses hex like "01 ff 0a" or "01ff0a". */
export function parseHex(s: string): Uint8Array {
  const clean = s.replace(/[\s,]|0x/gi, "");
  if (clean.length % 2 !== 0 || /[^0-9a-f]/i.test(clean)) throw new Error("invalid hex");
  return Uint8Array.from(clean.match(/../g) ?? [], (x) => parseInt(x, 16));
}

export function progressBar(): { el: HTMLProgressElement; set(done: number, total: number): void } {
  const el = h("progress", { max: 1, value: 0 });
  return {
    el,
    set(done, total) {
      el.max = Math.max(total, 1);
      el.value = done;
    },
  };
}
