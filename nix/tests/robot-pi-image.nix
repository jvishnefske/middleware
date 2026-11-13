# checks.robot-pi-image (design.md MS53, MS54): the built SD image, taken
# apart without mounting anything (sfdisk, mtools, debugfs), and the system
# it boots. Fails unless
#   * there is exactly one `.img.zst`, at most `maxCompressedMiB`, that
#     unpacks to at most `maxRawMiB`, with a FAT partition and an ext4 one
#   * the FAT partition holds bootcode.bin, start.elf, config.txt (with a
#     [pi02] section), u-boot-rpi3.bin and the Pi firmware's device trees
#     for the Zero 2 W and the Pi 3 B/B+
#   * the ext4 root holds only boot/, nix/ and the store registration (no
#     /etc, /var or /root: every runtime file arrives on the device), and
#     extlinux.conf
#   * the kernel's dtbs include broadcom/bcm2837-rpi-zero-2-w.dtb and
#     bcm2837-rpi-3-b.dtb (what U-Boot passes through extlinux)
#   * the system has a unit for every service in `piServices`, and
#     etc/udev/rules.d/70-mw-pico.rules names vendor 2e8a, products 000a and
#     0009, manufacturer middleware, product rp-base and /dev/mw-pico
#   * mw-gateway's and mw-robot's executables are static AArch64 binaries
#   * no store path in the closure holds a file named join-token,
#     bundle.spiffe, robot.env, authorized_keys, ssh_host_*, *.psk, or a
#     wpa_supplicant.conf outside share/doc
{ runCommand, closureInfo, zstd, util-linux, mtools, e2fsprogs, binutils, jq, gnugrep, findutils, gawk
, image, toplevel, piServices
, maxCompressedMiB ? 600, maxRawMiB ? 2560 }:
let
  closure = closureInfo { rootPaths = [ toplevel ]; };
in
runCommand "robot-pi-image" {
  nativeBuildInputs = [ zstd util-linux mtools e2fsprogs binutils jq gnugrep findutils gawk ];
} ''
  set -euo pipefail
  fail() { echo "robot-pi-image: $*" >&2; exit 1; }

  imgs=(${image}/sd-image/*.img.zst)
  [ ''${#imgs[@]} -eq 1 ] && [ -f "''${imgs[0]}" ] || fail "want exactly one .img.zst in ${image}/sd-image"
  zimg=''${imgs[0]}
  zmib=$(( $(stat -c %s "$zimg") / 1048576 ))
  echo "compressed: $zmib MiB ($(basename "$zimg"))"
  [ "$zmib" -le ${toString maxCompressedMiB} ] || fail "compressed image $zmib MiB > ${toString maxCompressedMiB} MiB"

  zstd -q -d "$zimg" -o disk.img
  rmib=$(( $(stat -c %s disk.img) / 1048576 ))
  echo "raw: $rmib MiB"
  [ "$rmib" -le ${toString maxRawMiB} ] || fail "raw image $rmib MiB > ${toString maxRawMiB} MiB"

  parts=$(sfdisk -J disk.img)
  [ "$(echo "$parts" | jq '.partitiontable.partitions | length')" = 2 ] || fail "want two partitions"
  part() { echo "$parts" | jq -r ".partitiontable.partitions[$1].$2"; }
  [ "$(part 0 type)" = b ] || fail "partition 1 is not FAT32"
  [ "$(part 1 type)" = 83 ] || fail "partition 2 is not Linux"
  dd if=disk.img of=fat.img bs=512 skip="$(part 0 start)" count="$(part 0 size)" status=none
  dd if=disk.img of=root.img bs=512 skip="$(part 1 start)" count="$(part 1 size)" status=none
  rm disk.img

  export MTOOLS_SKIP_CHECK=1
  fat=$(mdir -b -i fat.img ::)
  for f in bootcode.bin start.elf config.txt u-boot-rpi3.bin \
           bcm2710-rpi-zero-2-w.dtb bcm2710-rpi-3-b.dtb bcm2710-rpi-3-b-plus.dtb; do
    echo "$fat" | grep -qx "::/$f" || fail "firmware partition lacks $f"
  done
  mtype -i fat.img ::config.txt | grep -qF '[pi02]' || fail "config.txt has no [pi02] section"

  # `ls -p`: /inode/mode/uid/gid/name/size/ per entry.
  top=$(debugfs -R 'ls -p /' root.img 2>/dev/null | awk -F/ 'NF > 6 && $6 != "." && $6 != ".." { print $6 }' | sort -u)
  echo "root: $(echo $top)"
  for e in $top; do
    case "$e" in
      boot|nix|nix-path-registration|lost+found) ;;
      *) fail "root file system has /$e (only boot, nix and the registration belong in the image)" ;;
    esac
  done
  debugfs -R 'stat /boot/extlinux/extlinux.conf' root.img 2>/dev/null | grep -q 'Type: regular' \
    || fail "no /boot/extlinux/extlinux.conf"
  rm root.img fat.img

  for d in bcm2837-rpi-zero-2-w bcm2837-rpi-3-b bcm2837-rpi-3-b-plus; do
    [ -f ${toplevel}/dtbs/broadcom/$d.dtb ] || fail "kernel dtbs lack broadcom/$d.dtb"
  done

  for s in ${toString piServices}; do
    [ -e ${toplevel}/etc/systemd/system/$s.service ] || fail "no unit $s.service"
  done
  rules=${toplevel}/etc/udev/rules.d/70-mw-pico.rules
  [ -f "$rules" ] || fail "no 70-mw-pico.rules"
  for w in 'ATTRS{idVendor}=="2e8a"' 'ATTRS{idProduct}=="000a"' 'ATTRS{idProduct}=="0009"' \
           'ATTRS{manufacturer}=="middleware"' 'ATTRS{product}=="rp-base"' 'SYMLINK+="mw-pico"' \
           'ENV{SYSTEMD_WANTS}+="mw-gateway.service"'; do
    grep -qF "$w" "$rules" || fail "70-mw-pico.rules lacks $w"
  done

  for s in mw-gateway mw-robot; do
    exe=$(grep -E '^ExecStart=' ${toplevel}/etc/systemd/system/$s.service | grep -oE '/nix/store/[^ ]+/bin/'"$s" | head -n1)
    [ -n "$exe" ] || fail "$s.service runs no $s"
    readelf -hW "$exe" | grep -Eq 'Machine:[[:space:]]+AArch64' || fail "$exe is not AArch64"
    ! readelf -lW "$exe" | grep -q 'program interpreter' || fail "$exe is dynamically linked"
  done

  bad=$(find $(cat ${closure}/store-paths) \( -name join-token -o -name bundle.spiffe \
          -o -name robot.env -o -name authorized_keys -o -name 'ssh_host_*' -o -name '*.psk' \
          -o \( -name wpa_supplicant.conf -not -path '*/share/doc/*' \) \) -print 2>/dev/null || true)
  [ -z "$bad" ] || fail "runtime files in the closure: $bad"

  echo "closure: $(wc -l < ${closure}/store-paths) paths"
  echo "compressed $zmib MiB, raw $rmib MiB" > $out
''
