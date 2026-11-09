# checks.image-<name>-contents (design.md M41, M42): unpacks one image
# tarball and fails unless
#   * the config runs as 65532:65532 with the binary as the entrypoint,
#     carries the three org.opencontainers.image labels and the epoch as
#     its creation time,
#   * no layer holds a shell or any /bin, /sbin or /usr directory,
#   * the entrypoint is a static executable (no ELF interpreter, no
#     dynamic section),
#   * CA certificates and zoneinfo are present.
{ runCommand, jq, binutils, gnutar, image, name }:
runCommand "image-${name}-contents" { nativeBuildInputs = [ jq binutils gnutar ]; } ''
  set -euo pipefail
  mkdir archive root
  tar -xf ${image} -C archive
  config=archive/$(jq -r '.[0].Config' archive/manifest.json)

  fail() { echo "image ${name}: $*" >&2; exit 1; }

  [ "$(jq -r '.config.User' "$config")" = "65532:65532" ] || fail "not uid/gid 65532"
  [ "$(jq -r '.created' "$config")" = "1970-01-01T00:00:01Z" ] \
    || [ "$(jq -r '.created' "$config")" = "1970-01-01T00:00:00Z" ] || fail "creation time not the epoch"
  for l in source revision version; do
    v=$(jq -r ".config.Labels[\"org.opencontainers.image.$l\"] // empty" "$config")
    [ -n "$v" ] || fail "label org.opencontainers.image.$l missing"
  done
  [ "$(jq -r '.[0].RepoTags[0]' archive/manifest.json)" != "null" ] || fail "no repo tag"
  jq -r '.[0].RepoTags[0]' archive/manifest.json | grep -q '^localhost/mw/${name}:' \
    || fail "repo tag not localhost/mw/${name}:<tag>"

  for layer in $(jq -r '.[0].Layers[]' archive/manifest.json); do
    tar -xf "archive/$layer" -C root
  done
  for d in bin sbin usr; do
    [ ! -e "root/$d" ] || fail "has /$d"
  done
  if find root -name sh -o -name bash -o -name busybox | grep -q .; then
    fail "contains a shell"
  fi

  entry=root$(jq -r '.config.Entrypoint[0]' "$config")
  [ -x "$entry" ] || fail "entrypoint $entry missing"
  if readelf -lW "$entry" | grep -q 'Requesting program interpreter'; then
    fail "entrypoint has an ELF interpreter (dynamically linked)"
  fi
  if readelf -dW "$entry" | grep -q '(NEEDED)'; then
    fail "entrypoint needs shared libraries"
  fi

  ls root/nix/store/*-nss-cacert-*/etc/ssl/certs/ca-bundle.crt >/dev/null || fail "no CA bundle"
  ls -d root/nix/store/*-tzdata-*/share/zoneinfo >/dev/null || fail "no zoneinfo"

  echo "image ${name}: ok" > $out
''
