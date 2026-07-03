#!/usr/bin/env bash
# Shared helpers for bootstrap steps. Sourced, not executed.

set -euo pipefail

log()  { printf '\033[1;34m[majnet]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[majnet]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[majnet]\033[0m %s\n' "$*" >&2; exit 1; }

require_root() {
  [[ $EUID -eq 0 ]] || die "must run as root"
}

load_config() {
  local cfg=/etc/majnet/node.env
  [[ -f $cfg ]] || die "missing $cfg — copy node.env.example there and edit"
  # shellcheck source=/dev/null
  source "$cfg"
  : "${NODE_NAME:?}" "${NODE_ROLE:?}" "${WG_ADDRESS:?}"
  case $NODE_ROLE in
    main|prod|private) ;;
    *) die "NODE_ROLE must be main|prod|private, got: $NODE_ROLE" ;;
  esac
}

# install_file <src-content-via-stdin> <dest> <mode> — idempotent write;
# returns 0 and sets CHANGED=1 only if content differed.
install_stdin() {
  local dest=$1 mode=${2:-0644} tmp
  tmp=$(mktemp)
  cat > "$tmp"
  if [[ -f $dest ]] && cmp -s "$tmp" "$dest"; then
    rm -f "$tmp"
    return 0
  fi
  install -D -m "$mode" "$tmp" "$dest"
  rm -f "$tmp"
  CHANGED=1
}

# The step-side of the CHANGED protocol: `changed && systemctl restart …`,
# then `reset_changed` as the step's last line (returns 0, so a false
# condition above never fails the `source` in bootstrap.sh under set -e).
changed() { [[ ${CHANGED:-} == 1 ]]; }
reset_changed() { CHANGED=; }

apt_ensure() {
  local missing=()
  for pkg in "$@"; do
    dpkg -s "$pkg" &>/dev/null || missing+=("$pkg")
  done
  if ((${#missing[@]})); then
    log "installing: ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}"
  fi
}
