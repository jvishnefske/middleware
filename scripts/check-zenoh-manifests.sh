#!/usr/bin/env bash
# Validates the generated router manifests (deploy/zenoh/zenohd.yaml,
# design.md Z22) with kubeconform in strict mode.
#
# With vendored schemas in deploy/schemas/ (Kubernetes `<kind>-<group>-<v>.json`,
# Istio `<group>/<kind>_<v>.json`) it runs offline and skips kinds without
# a vendored schema; with MW_SCHEMAS=online it fetches the Kubernetes and
# datreeio CRD-catalog schemas instead, so every kind is checked.
# kubeconform comes from PATH, or from nixpkgs when absent.
set -euo pipefail
cd "$(dirname "$0")/.."

run() {
    if command -v kubeconform >/dev/null; then
        kubeconform "$@"
    else
        nix shell nixpkgs#kubeconform -c kubeconform "$@"
    fi
}

manifests=deploy/zenoh/zenohd.yaml
if [[ "${MW_SCHEMAS:-}" == online ]]; then
    run -strict -summary -kubernetes-version 1.33.0 \
        -schema-location default \
        -schema-location 'https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json' \
        "$manifests"
else
    [[ -d deploy/schemas ]] || { echo "no deploy/schemas; run with MW_SCHEMAS=online" >&2; exit 1; }
    run -strict -summary -ignore-missing-schemas \
        -schema-location 'deploy/schemas/{{.ResourceKind}}{{.KindSuffix}}.json' \
        -schema-location 'deploy/schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json' \
        "$manifests"
fi
