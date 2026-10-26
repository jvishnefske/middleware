// Block definitions for the Blocks editor, as Blockly JSON (pure data, no
// Blockly import, so the generator and tests can share the feature map),
// and the toolbox for a board profile.
//
// Custom blocks are prefixed `mw_` (any board) or `badge_` (conagotchi
// badge). The Blockly built-ins offered are the ones the AssemblyScript
// generator implements; every value is an `i32`, so there is no floating
// point, and the only loops are `repeat N` and `for` with constant bounds.

import { type Feature, type ProfileId, supports } from "./profiles";

const EVENT = 45;
const ACTION = 210;
const GPIO = 160;
const TIMER = 290;
const BADGE = 330;

const pin = (name = "PIN") => ({ type: "field_number", name, value: 8, min: 0, max: 48, precision: 1 });
const pull = {
  type: "field_dropdown",
  name: "PULL",
  options: [
    ["no pull", "0"],
    ["pull-up", "1"],
    ["pull-down", "2"],
  ],
};
const timerId = { type: "field_number", name: "ID", value: 0, min: 0, max: 3, precision: 1 };
const doInput = { message1: "%1", args1: [{ type: "input_statement", name: "DO" }] };

/** Blockly JSON definitions of the custom blocks. */
export const BLOCK_DEFS: readonly Record<string, unknown>[] = [
  // ---- events
  { type: "mw_on_init", message0: "on init", ...doInput, colour: EVENT, tooltip: "Runs once when the payload starts." },
  {
    type: "mw_every",
    message0: "every %1 ms",
    args0: [{ type: "field_number", name: "MS", value: 500, min: 10, precision: 1 }],
    ...doInput,
    colour: EVENT,
    tooltip: "Runs periodically (set_tick and the tick export). At least 10 ms.",
  },
  {
    type: "mw_on_message",
    message0: "on message %1",
    args0: [{ type: "field_input", name: "KEY", text: "app/echo/in" }],
    ...doInput,
    colour: EVENT,
    tooltip: "Subscribes to a key and runs for each message received on it.",
  },
  { type: "mw_message_text", message0: "message text", output: "String", colour: EVENT, tooltip: "The received message as text (inside on message)." },
  {
    type: "mw_message_number",
    message0: "message number",
    output: "Number",
    colour: EVENT,
    tooltip: "The received message parsed as a decimal number (inside on message).",
  },
  // ---- actions
  {
    type: "mw_publish",
    message0: "publish %1 %2",
    args0: [
      { type: "field_input", name: "KEY", text: "app/out" },
      { type: "input_value", name: "DATA" },
    ],
    previousStatement: null,
    nextStatement: null,
    colour: ACTION,
    tooltip: "Publishes text, or a number as decimal text, on a key (at most 512 bytes).",
  },
  {
    type: "mw_log",
    message0: "log %1",
    args0: [{ type: "input_value", name: "TEXT" }],
    previousStatement: null,
    nextStatement: null,
    colour: ACTION,
    tooltip: "Writes a log line (at most 256 bytes).",
  },
  { type: "mw_now_ms", message0: "milliseconds since start", output: "Number", colour: ACTION, tooltip: "now_ms(), wrapping." },
  // ---- GPIO
  {
    type: "mw_gpio_output",
    message0: "set pin %1 as output",
    args0: [pin()],
    previousStatement: null,
    nextStatement: null,
    colour: GPIO,
  },
  {
    type: "mw_gpio_input",
    message0: "set pin %1 as input %2",
    args0: [pin(), pull],
    previousStatement: null,
    nextStatement: null,
    colour: GPIO,
  },
  {
    type: "mw_gpio_write",
    message0: "write pin %1 %2",
    args0: [pin(), { type: "input_value", name: "LEVEL" }],
    previousStatement: null,
    nextStatement: null,
    colour: GPIO,
    tooltip: "Drives an output low (0) or high (anything else).",
  },
  {
    type: "mw_gpio_toggle",
    message0: "toggle pin %1",
    args0: [pin()],
    previousStatement: null,
    nextStatement: null,
    colour: GPIO,
    tooltip: "Inverts the level this payload last wrote to an output.",
  },
  { type: "mw_gpio_read", message0: "read pin %1", args0: [pin()], output: null, colour: GPIO, tooltip: "0 or 1." },
  {
    type: "mw_on_pin_edge",
    message0: "on pin %1 %2 edge %3",
    args0: [
      { ...pin(), value: 0 },
      pull,
      {
        type: "field_dropdown",
        name: "EDGE",
        options: [
          ["falling", "2"],
          ["rising", "1"],
          ["any", "3"],
        ],
      },
    ],
    ...doInput,
    colour: GPIO,
    tooltip: "Configures the pin as an input with an interrupt (gpio_irq) and runs on each edge (on_irq).",
  },
  { type: "mw_pin_level", message0: "pin level", output: null, colour: GPIO, tooltip: "The level after the edge (inside on pin edge)." },
  // ---- timers
  {
    type: "mw_timer_start",
    message0: "start timer %1 every %2 ms %3",
    args0: [
      timerId,
      { type: "field_number", name: "MS", value: 500, min: 1, precision: 1 },
      {
        type: "field_dropdown",
        name: "MODE",
        options: [
          ["repeating", "1"],
          ["once", "0"],
        ],
      },
    ],
    previousStatement: null,
    nextStatement: null,
    colour: TIMER,
    tooltip: "timer_start: hardware timer 0 to 3.",
  },
  {
    type: "mw_timer_stop",
    message0: "stop timer %1",
    args0: [timerId],
    previousStatement: null,
    nextStatement: null,
    colour: TIMER,
  },
  { type: "mw_on_timer", message0: "on timer %1", args0: [timerId], ...doInput, colour: TIMER, tooltip: "Runs on each alarm of the timer (on_irq)." },
  // ---- badge
  {
    type: "badge_led",
    message0: "LED %1 colour R %2 G %3 B %4",
    args0: [
      { type: "input_value", name: "INDEX", check: "Number" },
      { type: "input_value", name: "R", check: "Number" },
      { type: "input_value", name: "G", check: "Number" },
      { type: "input_value", name: "B", check: "Number" },
    ],
    inputsInline: true,
    previousStatement: null,
    nextStatement: null,
    colour: BADGE,
    tooltip: "Sets one LED (0 to 7) in the LED buffer; 'set LEDs' sends it. Channels are clamped to 0..255.",
  },
  {
    type: "badge_leds_fill",
    message0: "all LEDs colour R %1 G %2 B %3",
    args0: [
      { type: "input_value", name: "R", check: "Number" },
      { type: "input_value", name: "G", check: "Number" },
      { type: "input_value", name: "B", check: "Number" },
    ],
    inputsInline: true,
    previousStatement: null,
    nextStatement: null,
    colour: BADGE,
    tooltip: "Sets every LED in the LED buffer; 'set LEDs' sends it.",
  },
  {
    type: "badge_set_leds",
    message0: "set LEDs",
    previousStatement: null,
    nextStatement: null,
    colour: BADGE,
    tooltip: "Publishes the LED buffer (8 LEDs, R G B each, 24 bytes) on badge/leds.",
  },
  {
    type: "badge_show_text",
    message0: "show text %1",
    args0: [{ type: "input_value", name: "TEXT" }],
    previousStatement: null,
    nextStatement: null,
    colour: BADGE,
    tooltip: "Shows UTF-8 text on the badge screen (badge/text, at most 256 bytes).",
  },
  {
    type: "badge_pet_mode",
    message0: "pet mode %1",
    args0: [
      {
        type: "field_dropdown",
        name: "MODE",
        options: [
          ["auto", "0"],
          ["idle", "1"],
          ["walk", "2"],
          ["eat", "3"],
          ["play", "4"],
          ["sleep", "5"],
        ],
      },
    ],
    previousStatement: null,
    nextStatement: null,
    colour: BADGE,
    tooltip: "Sets the pet's mode (one byte on badge/pet).",
  },
  {
    type: "badge_on_button",
    message0: "on button %1 %2",
    args0: [
      {
        type: "field_dropdown",
        name: "BUTTON",
        options: [
          ["any", "0"],
          ["BOOT", "1"],
          ["LEFT", "2"],
          ["SELECT", "3"],
          ["START", "4"],
          ["RIGHT", "5"],
        ],
      },
      {
        type: "field_dropdown",
        name: "ACTION",
        options: [
          ["pressed", "1"],
          ["released", "0"],
          ["pressed or released", "2"],
        ],
      },
    ],
    ...doInput,
    colour: BADGE,
    tooltip: "Runs on badge button edges (badge/buttons delivers [button, pressed]).",
  },
  { type: "badge_button", message0: "button number", output: "Number", colour: BADGE, tooltip: "1 BOOT, 2 LEFT, 3 SELECT, 4 START, 5 RIGHT (inside on button)." },
  { type: "badge_pressed", message0: "button pressed", output: null, colour: BADGE, tooltip: "1 if pressed, 0 if released (inside on button)." },
];

/** The Blockly built-ins the generator implements. */
export const BUILTIN_BLOCKS: readonly string[] = [
  "controls_if",
  "logic_compare",
  "logic_operation",
  "logic_negate",
  "logic_boolean",
  "logic_ternary",
  "controls_repeat",
  "controls_repeat_ext",
  "controls_for",
  "controls_flow_statements",
  "math_number",
  "math_arithmetic",
  "math_single",
  "math_modulo",
  "math_constrain",
  "math_random_int",
  "math_number_property",
  "text",
  "text_join",
  "text_print",
  "variables_get",
  "variables_set",
  "math_change",
];

/** The feature a block needs, or null if no profile offers it. */
export function featureOf(type: string): Feature | null {
  if (BUILTIN_BLOCKS.includes(type)) return "core";
  if (type.startsWith("badge_")) return "badge";
  if (type.startsWith("mw_gpio_") || type === "mw_on_pin_edge" || type === "mw_pin_level") return "gpio";
  if (type.startsWith("mw_timer_") || type === "mw_on_timer") return "timers";
  if (BLOCK_DEFS.some((d) => d.type === type)) return "core";
  return null;
}

/** Whether `profile` supports a block type. */
export function blockSupported(type: string, profile: ProfileId): boolean {
  const f = featureOf(type);
  return f !== null && supports(profile, f);
}

/** Hat blocks: the event handlers whose stacks become exported code. */
export const EVENT_BLOCKS: readonly string[] = ["mw_on_init", "mw_every", "mw_on_message", "mw_on_pin_edge", "mw_on_timer", "badge_on_button"];

type ToolboxItem = Record<string, unknown>;

const num = (n: number) => ({ shadow: { type: "math_number", fields: { NUM: n } } });
const txt = (s: string) => ({ shadow: { type: "text", fields: { TEXT: s } } });

function blk(type: string, extra: Record<string, unknown> = {}): ToolboxItem {
  return { kind: "block", type, ...extra };
}

/** The toolbox (Blockly JSON) for a profile. Blocks the profile does not
 * support are shown disabled. */
export function toolbox(profile: ProfileId): ToolboxItem {
  const mark = (item: ToolboxItem): ToolboxItem =>
    blockSupported(String(item.type), profile) ? item : { ...item, disabled: true };
  const category = (name: string, colour: number | string, contents: ToolboxItem[], extra: Record<string, unknown> = {}) => ({
    kind: "category",
    name,
    colour: String(colour),
    contents: contents.map(mark),
    ...extra,
  });
  return {
    kind: "categoryToolbox",
    contents: [
      category("Events", EVENT, [
        blk("mw_on_init"),
        blk("mw_every"),
        blk("mw_on_message"),
        blk("mw_message_text"),
        blk("mw_message_number"),
      ]),
      category("Actions", ACTION, [
        blk("mw_publish", { inputs: { DATA: txt("hello") } }),
        blk("mw_log", { inputs: { TEXT: txt("hello") } }),
        blk("mw_now_ms"),
      ]),
      category("GPIO", GPIO, [
        blk("mw_gpio_output"),
        blk("mw_gpio_input"),
        blk("mw_gpio_write", { inputs: { LEVEL: num(1) } }),
        blk("mw_gpio_toggle"),
        blk("mw_gpio_read"),
        blk("mw_on_pin_edge"),
        blk("mw_pin_level"),
      ]),
      category("Timers", TIMER, [blk("mw_timer_start"), blk("mw_timer_stop"), blk("mw_on_timer")]),
      category("Badge", BADGE, [
        blk("badge_led", { inputs: { INDEX: num(0), R: num(255), G: num(0), B: num(0) } }),
        blk("badge_leds_fill", { inputs: { R: num(0), G: num(0), B: num(0) } }),
        blk("badge_set_leds"),
        blk("badge_show_text", { inputs: { TEXT: txt("hello") } }),
        blk("badge_pet_mode"),
        blk("badge_on_button"),
        blk("badge_button"),
        blk("badge_pressed"),
      ]),
      { kind: "sep" },
      category("Logic", "%{BKY_LOGIC_HUE}", [
        blk("controls_if"),
        blk("logic_compare"),
        blk("logic_operation"),
        blk("logic_negate"),
        blk("logic_boolean"),
        blk("logic_ternary"),
      ]),
      category("Loops", "%{BKY_LOOPS_HUE}", [
        blk("controls_repeat"),
        blk("controls_for", { inputs: { FROM: num(0), TO: num(7), BY: num(1) } }),
        blk("controls_flow_statements"),
      ]),
      category("Math", "%{BKY_MATH_HUE}", [
        blk("math_number"),
        blk("math_arithmetic", { inputs: { A: num(1), B: num(1) } }),
        blk("math_single", { fields: { OP: "ABS" }, inputs: { NUM: num(-5) } }),
        blk("math_modulo", { inputs: { DIVIDEND: num(10), DIVISOR: num(8) } }),
        blk("math_constrain", { inputs: { VALUE: num(50), LOW: num(0), HIGH: num(255) } }),
        blk("math_random_int", { inputs: { FROM: num(0), TO: num(255) } }),
        blk("math_number_property", { inputs: { NUMBER_TO_CHECK: num(0) } }),
      ]),
      category("Text", "%{BKY_TEXTS_HUE}", [blk("text"), blk("text_join"), blk("text_print", { inputs: { TEXT: txt("hello") } })]),
      { kind: "category", name: "Variables", colour: "%{BKY_VARIABLES_HUE}", custom: "VARIABLE" },
    ],
  };
}
