// One selected serial port shared by the Flash and Device tabs.
//
// Only one owner may use the port at a time. `acquire` asks the current
// owner to release (close its reader/writer and the port) before handing it
// over, so switching tabs never leaves a locked stream behind.

export interface PortLike {
  getInfo(): SerialPortInfo;
}

export function webSerialSupported(nav: Navigator = navigator): boolean {
  return "serial" in nav;
}

/** Why Web Serial is unavailable, or null when it is usable. */
export function webSerialProblem(win: { isSecureContext: boolean; navigator: Navigator }): string | null {
  if (!win.isSecureContext) return "Web Serial needs a secure context: open the site over https or on http://localhost.";
  if (!webSerialSupported(win.navigator)) return "This browser has no Web Serial API. Use Chrome or Edge (desktop).";
  return null;
}

export function describePort(info: SerialPortInfo): string {
  const hex = (n?: number) => (n === undefined ? "?" : n.toString(16).padStart(4, "0"));
  const known: Record<string, string> = {
    "303a:1001": "ESP USB-Serial-JTAG",
    "10c4:ea60": "CP210x bridge",
    "1a86:7523": "CH340 bridge",
    "1a86:55d4": "CH9102 bridge",
    "0403:6001": "FTDI bridge",
    "0403:6015": "FTDI bridge",
  };
  const id = `${hex(info.usbVendorId)}:${hex(info.usbProductId)}`;
  return known[id] ? `${known[id]} (${id})` : `USB ${id}`;
}

type Release = () => Promise<void>;

export class PortManager<P extends PortLike = SerialPort> {
  private port: P | null = null;
  private owner: string | null = null;
  private release: Release | null = null;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly request: () => Promise<P>) {}

  get current(): P | null {
    return this.port;
  }

  get currentOwner(): string | null {
    return this.owner;
  }

  onChange(f: () => void): void {
    this.listeners.add(f);
  }

  private changed(): void {
    for (const f of this.listeners) f();
  }

  /** Prompts the user for a port (replacing the current one). */
  async choose(): Promise<P> {
    const p = await this.request();
    await this.releaseCurrent();
    this.port = p;
    this.changed();
    return p;
  }

  /** Takes the port for `owner`, releasing any other owner first. */
  async acquire(owner: string, release: Release): Promise<P> {
    const p = this.port ?? (await this.choose());
    if (this.owner !== owner) await this.releaseCurrent();
    this.owner = owner;
    this.release = release;
    this.changed();
    return p;
  }

  /** Called by an owner that has released the port itself. */
  released(owner: string): void {
    if (this.owner === owner) {
      this.owner = null;
      this.release = null;
      this.changed();
    }
  }

  /** Asks the current owner to release. */
  async releaseCurrent(): Promise<void> {
    const r = this.release;
    this.release = null;
    this.owner = null;
    if (r) {
      try {
        await r();
      } catch {
        /* the owner already lost the port */
      }
    }
    this.changed();
  }
}
