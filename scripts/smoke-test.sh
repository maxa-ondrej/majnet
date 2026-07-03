#!/usr/bin/env bash
# End-to-end smoke test of the reconciler against the LOCAL Docker daemon.
#
# Exercises the §12 loop without GitHub or any server:
#   fixture env branch → converge → healthy container with decrypted SOPS
#   secret on tmpfs → blue-green on config change → GC when config is gone.
#
# Requires: docker, sops, age-keygen, cargo (all in the nix dev shell).
# Usage: scripts/smoke-test.sh   (from the repo root; direnv or nix develop)

set -euo pipefail
cd "$(dirname "$0")/.."

APP=hello
PROJECT=demo
IMAGE=nginx:1.27-alpine   # has busybox wget for the health check
LISTEN=127.0.0.1:19090
WORK=$(mktemp -d)
RECON_PID=

red()   { printf '\033[31m✗ %s\033[0m\n' "$*"; }
green() { printf '\033[32m✓ %s\033[0m\n' "$*"; }
step()  { printf '\033[1;34m── %s\033[0m\n' "$*"; }

cleanup() {
  [[ -n $RECON_PID ]] && kill "$RECON_PID" 2>/dev/null || true
  docker ps -aq --filter "label=majnet.project=$PROJECT" | xargs -r docker rm -f >/dev/null 2>&1 || true
  docker network rm "proj-$PROJECT" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { red "$1"; echo "--- reconciler log tail ---"; tail -30 "$WORK/reconciler.log" 2>/dev/null || true; exit 1; }

# wait_for <seconds> <description> <command...>
wait_for() {
  local timeout=$1 what=$2; shift 2
  for _ in $(seq 1 "$timeout"); do
    if "$@" >/dev/null 2>&1; then green "$what"; return 0; fi
    sleep 1
  done
  fail "timed out waiting for: $what"
}

app_container() {
  docker ps -q --filter "label=majnet.project=$PROJECT" --filter "label=majnet.app=$APP" --filter status=running
}
app_is_healthy() {
  local id; id=$(app_container); [[ -n $id ]] || return 1
  [[ $(docker inspect -f '{{.State.Health.Status}}' "$id") == healthy ]]
}
app_env_rev() {
  local id; id=$(app_container); [[ -n $id ]] || return 1
  docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$id" | grep -q "^REV=$1$"
}
app_gone() { [[ -z $(app_container) ]]; }
app_single() { [[ $(app_container | wc -l | tr -d ' ') == 1 ]]; }

step "preflight"
docker info >/dev/null || { red "docker daemon not reachable"; exit 1; }
{ command -v sops && command -v age-keygen; } >/dev/null || { red "need sops + age-keygen (nix dev shell)"; exit 1; }

step "building fixture in $WORK"
SNAP="$WORK/snapshots"
# The project's env-branch dirs are created here (git tracks no empty dirs).
mkdir -p "$SNAP" "$WORK/age" "$SNAP/$PROJECT/ops/env/stable/secrets"
cp -R scripts/smoke/fixture/* "$SNAP/"

# Class age key + one SOPS-encrypted secret, exactly as the bot would pass it through.
age-keygen -o "$WORK/age/age-stable.key" 2>/dev/null
AGE_PUB=$(age-keygen -y "$WORK/age/age-stable.key")
printf 'greeting: hello-from-sops\n' > "$WORK/secret.yaml"
sops encrypt --age "$AGE_PUB" --input-type yaml --output-type yaml "$WORK/secret.yaml" \
  > "$SNAP/$PROJECT/ops/env/stable/secrets/$APP.yaml"

# Digest-pinned image, like a rendered manifest.
docker pull -q "$IMAGE" >/dev/null
DIGEST=$(docker inspect -f '{{index .RepoDigests 0}}' "$IMAGE")
manifest() { # manifest <rev>
  cat > "$SNAP/$PROJECT/ops/env/stable/$APP.yaml" <<EOF
name: $APP
image: $DIGEST
env:
  REV: "$1"
secrets: [greeting]
health:
  path: /
  port: 80
EOF
}
manifest 1
green "fixture ready ($DIGEST)"

step "building + starting reconciler (local mode)"
cargo build -q -p majnet-reconciler
MAJNET_BOT_URL=http://unused.invalid \
MAJNET_DOCKER_LOCAL=1 \
MAJNET_SNAPSHOT_DIR="$SNAP" \
MAJNET_AGE_KEY_DIR="$WORK/age" \
MAJNET_DATA_DIR="$WORK/data" \
MAJNET_LISTEN="$LISTEN" \
MAJNET_POLL_INTERVAL_SECS=3 \
RUST_LOG=info \
  target/debug/majnet-reconciler > "$WORK/reconciler.log" 2>&1 &
RECON_PID=$!
wait_for 10 "reconciler is up" curl -fs "http://$LISTEN/healthz"

step "1) initial converge: container healthy, secret decrypted onto tmpfs"
wait_for 90 "app container healthy" app_is_healthy
if docker exec "$(app_container)" sh -c 'test "$(cat /run/secrets/greeting)" = hello-from-sops'; then
  green "SOPS secret decrypted and mounted at /run/secrets/greeting"
else
  fail "secret file wrong or missing"
fi
docker network inspect "proj-$PROJECT" >/dev/null 2>&1 && green "project network exists"

step "2) blue-green: config change replaces the container, no gap"
OLD_ID=$(app_container)
manifest 2
curl -fs -X POST "http://$LISTEN/notify" -H 'content-type: application/json' -d '{}' >/dev/null
wait_for 90 "new container serving REV=2" app_env_rev 2
if [[ $(app_container) != "$OLD_ID" ]]; then green "old container replaced"; else fail "container was not replaced"; fi
wait_for 30 "exactly one container remains" app_single
app_is_healthy && green "replacement is healthy"

step "3) GC: manifest removed from git → container removed"
rm "$SNAP/$PROJECT/ops/env/stable/$APP.yaml" "$SNAP/$PROJECT/ops/env/stable/secrets/$APP.yaml"
curl -fs -X POST "http://$LISTEN/notify" -H 'content-type: application/json' -d '{}' >/dev/null
wait_for 60 "app container gone" app_gone

step "4) event log tells the story"
curl -fs "http://$LISTEN/api/events?limit=20" | grep -q '"action":"converge hello"' && green "converge events recorded"
curl -fs "http://$LISTEN/api/events?limit=20" | grep -q '"action":"gc"' && green "gc event recorded"

echo
green "SMOKE TEST PASSED — render→converge→blue-green→GC all work against real Docker"
