//! Requirement E15: exports a chip profile as a board support package.
//!
//!   cargo run --example bsp_export -- <chip>          # mw-bsp/1 JSON
//!   cargo run --example bsp_export -- <chip> --wire   # sys/board body, hex
//!
//! `<chip>` is `esp32`, `esp32s3`, `esp32c3` or `esp8266`. Both outputs come
//! from the same [`Profile`] the firmware serves on `sys/board`, so a host
//! that bundles the JSON (Skygraph) can compare it with the device, and the
//! hex is that comparison's golden fixture. `build-images.sh` writes the JSON
//! next to each image as `<chip>.bsp.json`.
//!
//! `firmware` is the version `sys/info` reports (`+no-wasm` without the
//! runtime), so a BSP is identified as `<chip>@<firmware>`.

use esp_base::chip::{Chip, Profile};
use middleware::loader::{BOARD_INFO_MAX, BoardInfo};
use middleware::wasm_abi::ABI_VERSION;
use std::fmt::Write as _;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn chip(name: &str) -> Option<&'static Profile> {
    [Chip::Esp32, Chip::Esp32s3, Chip::Esp32c3, Chip::Esp8266]
        .into_iter()
        .map(Chip::profile)
        .find(|p| p.name == name)
}

fn pins(mask: u64) -> String {
    let list: Vec<String> = (0..64u32)
        .filter(|n| mask >> n & 1 == 1)
        .map(|n| n.to_string())
        .collect();
    format!("[{}]", list.join(", "))
}

fn json(p: &Profile) -> String {
    let b: BoardInfo<'_> = p.board_info(ABI_VERSION);
    let firmware = if p.runtime {
        VERSION.to_owned()
    } else {
        format!("{VERSION}+no-wasm")
    };
    let table: Vec<String> = b
        .pwm_pins
        .iter()
        .enumerate()
        .map(|(i, gpio)| {
            format!(
                "{{ \"timer\": {}, \"channel\": {}, \"gpio\": {gpio} }}",
                i / 2,
                i % 2
            )
        })
        .collect();
    let mut s = String::new();
    let _ = writeln!(s, "{{");
    let _ = writeln!(s, "  \"format\": \"mw-bsp/1\",");
    let _ = writeln!(s, "  \"chip\": \"{}\",", p.name);
    let _ = writeln!(s, "  \"firmware\": \"{firmware}\",");
    let _ = writeln!(s, "  \"abi\": {},", b.abi_version);
    let _ = writeln!(s, "  \"runtime\": {},", p.runtime);
    let _ = writeln!(s, "  \"maxImage\": {},", b.max_image);
    let _ = writeln!(s, "  \"maxMemory\": {},", b.max_memory);
    let _ = writeln!(
        s,
        "  \"console\": {{ \"usbJtag\": {}, \"uartTx\": {}, \"uartRx\": {} }},",
        p.usb_serial_jtag, b.uart_tx, b.uart_rx
    );
    let _ = writeln!(s, "  \"pins\": {{");
    let _ = writeln!(s, "    \"gpio\": {},", pins(b.gpio));
    let _ = writeln!(s, "    \"reserved\": {},", pins(b.reserved));
    let _ = writeln!(s, "    \"inputOnly\": {},", pins(b.input_only));
    let _ = writeln!(s, "    \"strapping\": {},", pins(b.strapping));
    let _ = writeln!(s, "    \"adc1\": {}", pins(b.adc));
    let _ = writeln!(s, "  }},");
    let _ = writeln!(s, "  \"pwm\": {{");
    let _ = writeln!(s, "    \"maxBits\": {},", b.pwm_max_bits);
    let _ = writeln!(s, "    \"refTick\": {},", p.pwm_ref_tick);
    if table.is_empty() {
        let _ = writeln!(s, "    \"table\": []");
    } else {
        let _ = writeln!(
            s,
            "    \"table\": [\n      {}\n    ]",
            table.join(",\n      ")
        );
    }
    let _ = writeln!(s, "  }}");
    let _ = writeln!(s, "}}");
    s
}

fn wire(p: &Profile) -> String {
    let mut buf = [0u8; BOARD_INFO_MAX];
    let n = p
        .board_info(ABI_VERSION)
        .encode(&mut buf)
        .expect("fits BOARD_INFO_MAX");
    buf[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        + "\n"
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (name, wire_out) = match args.as_slice() {
        [c] => (c.as_str(), false),
        [c, w] if w == "--wire" => (c.as_str(), true),
        _ => {
            eprintln!("usage: bsp_export <esp32|esp32s3|esp32c3|esp8266> [--wire]");
            return ExitCode::from(2);
        }
    };
    let Some(p) = chip(name) else {
        eprintln!("unknown chip '{name}' (expected esp32, esp32s3, esp32c3 or esp8266)");
        return ExitCode::from(2);
    };
    print!("{}", if wire_out { wire(p) } else { json(p) });
    ExitCode::SUCCESS
}
