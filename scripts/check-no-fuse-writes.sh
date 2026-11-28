#!/usr/bin/env bash
# NF1-NF3 (design.md "No fuse or OTP writes"): nothing in this repository
# may burn a microcontroller's one-time-programmable memory. A burned eFuse
# or OTP row (secure boot, flash encryption, JTAG disable, boot mode, keys,
# rollback counters) can never be cleared and can brick a board.
#
# Fails when any tracked source, script, tool or build file names an API or
# a command that writes fuses or OTP:
#   ESP32 family   esp-idf esp_efuse_* writers, espefuse/espsecure, secure
#                  boot or flash encryption switches
#   RP2350         embassy-rp's otp::write_raw_word / write_ecc_word (safe
#                  functions: forbid(unsafe_code) does not stop them), the
#                  boot ROM's otp_access / explicit_buy (which raises the OTP
#                  rollback counter in secure mode), picotool otp/seal
#   i.MX RT        writes to the OCOTP controller (0x401F_4000) through the
#                  PAC or a raw address, NXP's blhost/sdphost/elftosb, fuse
#                  words such as BT_FUSE_SEL
#   STM32          option byte writes (FLASH OPTR through the PAC, RDP,
#                  OBL_LAUNCH), the OTP area and option bytes by address
#                  (0x1FFF_7000-0x1FFF_78FF on the G0), STM32CubeProgrammer
#                  and st-flash option byte commands, dfu-util on an
#                  alternate setting other than 0
#
# Reads stay allowed: the ESP base MAC (esp_hal::efuse::base_mac_address),
# the RP2350 chip id (embassy_rp::otp::get_chipid, read_*_word) and OCOTP's
# shadow registers (`.read()`).
#
# The host tools also refuse such writes at run time
# (firmware-imxrt/tools/fusesafe.py); this check keeps new code from
# adding a path.
set -euo pipefail
cd "$(dirname "$0")/.."

# Files that describe the rule rather than break it.
exclude=(
  ':!scripts/check-no-fuse-writes.sh'
  ':!firmware-imxrt/tools/fusesafe.py'
  ':!firmware-rp/clippy.toml'
  ':!design.md'
  ':!CHANGELOG.md'
  ':!*.lock'
  ':!deploy/schemas/**'
)

patterns=(
  # ESP32 family
  'esp_efuse_(write|burn|set|batch_write)'
  '\bespefuse(\.py)?\b'
  '\bespsecure(\.py)?\b'
  'burn[_-](efuse|key|bit|block)'
  'CONFIG_SECURE_(BOOT|FLASH_ENC)'
  'CONFIG_FLASH_ENCRYPTION'
  'FLASH_CRYPT_CNT'
  '\-\-(secure-boot|flash-encryption|encrypt)\b'
  # RP2350
  'otp::write_(raw|ecc)_word'
  '\bwrite_(raw|ecc)_word\s*\('
  '\botp_access(_ns)?\b'
  'rom_data::explicit_buy'
  'picotool\s+(otp|seal|encrypt)'
  # i.MX RT
  'OCOTP[[:alnum:]_]*\.[[:alnum:]_]+\(\)\.(write|modify|write_value)\b'
  '(write_reg|modify_reg)!\s*\(\s*ocotp'
  '0x401[fF]_?4[0-9a-fA-F_]{3}'
  '\b(blhost|sdphost|elftosb)\b'
  'efuse-program-once'
  'BT_FUSE_SEL'
  # STM32 (NF5)
  '\boptr\(\)\.(write|modify|write_value)\b'
  '\.set_(rdp|obl_launch|optstrt)\s*\('
  '0x1[fF]{3}_?7[0-8][0-9a-fA-F]{2}\b'
  '\bSTM32_Programmer_CLI\b'
  'st-flash[^#]*--area=option'
  'dfu-util[^#]*-a\s*[1-9]'
)

status=0
for p in "${patterns[@]}"; do
  if hits=$(git grep --untracked -n -I -E "$p" -- . "${exclude[@]}"); then
    echo "FAIL: fuse/OTP write pattern /$p/:" >&2
    echo "$hits" >&2
    status=1
  fi
done

# The run-time guard must stay on every write path of the i.MX RT tools.
for f in jflash.py ramload.py flash.py rsp.py; do
  grep -q fusesafe "firmware-imxrt/tools/$f" ||
    { echo "FAIL: firmware-imxrt/tools/$f writes target memory without fusesafe" >&2; status=1; }
done
python3 -I firmware-imxrt/tools/fusesafe.py >/dev/null ||
  { echo "FAIL: fusesafe.py self-test" >&2; status=1; }

[ $status = 0 ] && echo "OK: no fuse or OTP writes"
exit $status
