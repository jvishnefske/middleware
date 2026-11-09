#!/usr/bin/env bash
# Validates the rendered Kubernetes manifests offline (design.md M14, M34):
#   1. `cargo test -p mw-service --test manifest` proves that the goldens in
#      service/tests/golden/ are exactly what `--print-manifests` renders
#      today (byte for byte);
#   2. kubeconform -strict checks every golden document against the vendored
#      JSON schemas in deploy/schemas/ (Kubernetes 1.31 standalone-strict,
#      Istio security.istio.io/v1 and networking.istio.io/v1; sources in
#      deploy/schemas/README.md). No network: an object without a vendored
#      schema fails rather than being fetched or skipped.
# kubeconform comes from PATH, or else from `nix shell nixpkgs#kubeconform`.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== M13: goldens match the renderer"
cargo test --quiet -p mw-service --test manifest

shopt -s nullglob
goldens=(service/tests/golden/*.yaml)
[ ${#goldens[@]} -gt 0 ] || fail "no goldens under service/tests/golden"

echo "== M34: vendored schemas are the recorded files"
sha256sum --check --quiet deploy/schemas/SHA256SUMS || fail "deploy/schemas differs from SHA256SUMS"

if command -v kubeconform >/dev/null 2>&1; then
    kubeconform=(kubeconform)
else
    kubeconform=(nix shell nixpkgs#kubeconform -c kubeconform)
fi

echo "== M14: kubeconform -strict, vendored schemas only (${#goldens[@]} files)"
"${kubeconform[@]}" -strict -summary -output text \
    -kubernetes-version 1.31.0 \
    -schema-location 'deploy/schemas/{{ .ResourceKind }}{{ .KindSuffix }}.json' \
    -schema-location 'deploy/schemas/{{ .Group }}/{{ .ResourceKind }}_{{ .ResourceAPIVersion }}.json' \
    "${goldens[@]}"

# A misspelt field must fail in both the core and the Istio schemas: prove
# -strict and the vendored schemas are in force.
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT
for typo in 's/automountServiceAccountToken:/automountServiceAcountToken:/' \
            's/        principals:/        principal:/' \
            's/mode: ISTIO_MUTUAL/mode: ISTIO_MUTAL/'; do
    sed "$typo" service/tests/golden/demo-echo.yaml > "$probe/typo.yaml"
    cmp -s "$probe/typo.yaml" service/tests/golden/demo-echo.yaml && fail "probe '$typo' changed nothing"
    if "${kubeconform[@]}" -strict -kubernetes-version 1.31.0 \
        -schema-location 'deploy/schemas/{{ .ResourceKind }}{{ .KindSuffix }}.json' \
        -schema-location 'deploy/schemas/{{ .Group }}/{{ .ResourceKind }}_{{ .ResourceAPIVersion }}.json' \
        "$probe/typo.yaml" >/dev/null 2>&1; then
        fail "kubeconform accepted '$typo': schemas are not strict"
    fi
done
echo "OK: manifests validate"
