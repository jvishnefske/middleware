#!/usr/bin/env bash
# Worst-case stack of one signature verification (design.md TK3), per
# algorithm, on the firmware CPU families: Cortex-M0+ (RP2040), Cortex-M7
# (RT1050; the RP2350's M33 is between the two) and RV32IMC (ESP32-C3).
# Frames come from LLVM (`-Z emit-stack-sizes`, nightly with build-std),
# call edges from the disassembly's call relocations; the verify path has
# no indirect calls (the script lists any it finds). Xtensa (ESP32, S3,
# ESP8266) is not covered: its toolchain emits no stack-size section.
set -euo pipefail
cd "$(dirname "$0")"
BIN="$(rustc +nightly --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin"
for t in thumbv6m-none-eabi thumbv7em-none-eabihf riscv32imc-unknown-none-elf; do
  RUSTFLAGS="-Z emit-stack-sizes" cargo +nightly build -q -Zbuild-std=core --release --target "$t"
  a="target/$t/release/libverify_stack.a"
  syms=$("$BIN/llvm-nm" "$a" | grep -o '_RNv[^ ]*verify_\(ed25519\|p256\)[^ ]*' | sort -u)
  echo "== $t"
  python3 -I stack.py "$BIN" "$PWD/$a" $syms | grep -E 'worst|indirect' \
    | sed -E 's/_RNv.*(verify_[a-z0-9]+).*: /\1: /'
done
