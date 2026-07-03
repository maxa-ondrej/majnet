#!/usr/bin/env bash
# MajNet node bootstrap — idempotent, safe to re-run (design doc §4, §19 phase 0).
#
# Usage (on a fresh Debian 12/13 minimal install, as root):
#   1. mkdir -p /etc/majnet && cp node.env.example /etc/majnet/node.env && $EDITOR /etc/majnet/node.env
#   2. copy PKI material (from pki/gen-certs.sh) to /etc/majnet/pki/
#   3. ./bootstrap.sh            # runs all steps
#      ./bootstrap.sh 20 30     # or just selected steps by prefix
#
# Node recovery = re-run this + restic restore + reconciler reconverges from git.

set -euo pipefail
cd "$(dirname "$0")"
# shellcheck source-path=SCRIPTDIR
source lib/common.sh

require_root
load_config

log "bootstrapping node '$NODE_NAME' (role: $NODE_ROLE)"

steps=(steps/*.sh)
if (($#)); then
  selected=()
  for prefix in "$@"; do
    for s in "${steps[@]}"; do
      [[ $(basename "$s") == "$prefix"* ]] && selected+=("$s")
    done
  done
  steps=("${selected[@]}")
fi

for step in "${steps[@]}"; do
  log "── $(basename "$step") ──────────────────────"
  # shellcheck source=/dev/null
  source "$step"
done

log "done. If this was the first run: share the WireGuard pubkey above with"
log "the other nodes' node.env, re-run step 20 everywhere, then verify:"
log "  wg show && curl --cacert /etc/majnet/pki/ca.pem https://\$WG_IP:\$DOCKER_API_PORT/_ping"
