# 50-agents — observability + backup agents (design doc §15, §16).
# restic is installed in 10-base; repository config is a phase-5 concern.

if [[ -z ${BESZEL_AGENT_KEY:-} ]]; then
  warn "BESZEL_AGENT_KEY empty — skipping Beszel agent (set it once the hub on main is up)"
  return 0
fi

# Beszel agent as a container, listening on the WG IP only.
WG_IP=${WG_ADDRESS%/*}
docker container inspect beszel-agent &>/dev/null && docker rm -f beszel-agent >/dev/null
log "starting beszel-agent on $WG_IP:45876"
docker run -d --name beszel-agent --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock:ro \
  -p "$WG_IP:45876:45876" \
  -e KEY="$BESZEL_AGENT_KEY" \
  henrygd/beszel-agent:latest
