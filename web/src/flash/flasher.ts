// Base-firmware flashing with esptool-js over Web Serial (requirement U1).
//
// Connect -> detect chip -> pick image (callback, so the caller can use the
// manifest or a local file) -> write at the image offset with progress ->
// hard reset -> release the port.
//
// Reset handling is esptool-js's: for USB-serial bridges (CP2102, CH340,
// FTDI) it toggles DTR/RTS through the board's auto-reset circuit (ESP8266
// boards are of this kind: a CH340 with the classic DTR/RTS circuit, and
// esptool-js reports CHIP_NAME "ESP8266", manifest key "esp8266"); for the
// native USB-Serial-JTAG (ESP32-S3/C3, Espressif PID 0x1001) it uses the
// USB-JTAG reset sequence. Boards without an auto-reset circuit, or a
// USB-JTAG port whose application has disabled USB, need BOOT held while
// pressing RESET to enter the ROM bootloader.

import { ESPLoader, Transport, USB_JTAG_SERIAL_PID, ESPRESSIF_VID, type IEspLoaderTerminal } from "esptool-js";

export interface ChosenImage {
  data: Uint8Array;
  offset: number;
  label: string;
}

export interface FlashCallbacks {
  log(line: string): void;
  progress(written: number, total: number): void;
  /** Picks the image for the detected chip; throw to abort. */
  chooseImage(chipName: string): Promise<ChosenImage>;
}

export interface FlashResult {
  chip: string;
  image: string;
  usbJtag: boolean;
}

/** Whether a port is the chip's native USB-Serial-JTAG. */
export function isUsbJtag(info: SerialPortInfo): boolean {
  return info.usbVendorId === ESPRESSIF_VID && info.usbProductId === USB_JTAG_SERIAL_PID;
}

/** Flashes `port` (which must be closed; esptool-js opens it). The port is
 * closed again on return, success or failure. */
export async function flashFirmware(port: SerialPort, cb: FlashCallbacks, baudrate = 460800): Promise<FlashResult> {
  const terminal: IEspLoaderTerminal = {
    clean: () => {},
    writeLine: (d) => cb.log(d),
    write: (d) => cb.log(d),
  };
  const usbJtag = isUsbJtag(port.getInfo());
  const transport = new Transport(port, false);
  try {
    const loader = new ESPLoader({ transport, baudrate, romBaudrate: 115200, terminal });
    cb.log(usbJtag ? "Native USB-Serial-JTAG port detected." : "USB-serial bridge port; resetting via DTR/RTS.");
    await loader.main("default_reset");
    const chip = loader.chip.CHIP_NAME;
    cb.log(`Detected ${chip}.`);
    const image = await cb.chooseImage(chip);
    cb.log(`Writing ${image.label} (${image.data.length} bytes) at 0x${image.offset.toString(16)}.`);
    await loader.writeFlash({
      fileArray: [{ data: image.data, address: image.offset }],
      flashMode: "keep",
      flashFreq: "keep",
      flashSize: "keep",
      eraseAll: false,
      compress: true,
      reportProgress: (_i, written, total) => cb.progress(written, total),
    });
    cb.log("Write complete; hard reset.");
    await loader.after("hard_reset");
    return { chip, image: image.label, usbJtag };
  } finally {
    try {
      await transport.disconnect();
    } catch {
      /* already closed */
    }
  }
}
