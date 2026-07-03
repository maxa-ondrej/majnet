# 20-wireguard — the cluster mesh: three static peers (design doc §7).
# Key is generated on the node and never leaves it; only the pubkey is shared.

WG_KEY=/etc/wireguard/wg0.key
if [[ ! -f $WG_KEY ]]; then
  log "generating WireGuard key"
  (umask 077 && wg genkey > "$WG_KEY")
fi
WG_PUBKEY=$(wg pubkey < "$WG_KEY")
log "WireGuard public key for '$NODE_NAME': $WG_PUBKEY"

render_wg_conf() {
  cat <<EOF
[Interface]
Address = $WG_ADDRESS
ListenPort = ${WG_LISTEN_PORT:-51820}
PrivateKey = $(cat "$WG_KEY")
EOF
  while IFS=: read -r name ip pubkey host port; do
    [[ -z $name ]] && continue
    if [[ $pubkey == REPLACE_PUBKEY || -z $pubkey ]]; then
      warn "peer '$name' has no pubkey yet — skipping (fill node.env and re-run)"
      continue
    fi
    cat <<EOF

# peer: $name
[Peer]
PublicKey = $pubkey
AllowedIPs = $ip/32
Endpoint = $host:$port
PersistentKeepalive = 25
EOF
  done <<< "$WG_PEERS"
}
install_stdin /etc/wireguard/wg0.conf 0600 < <(render_wg_conf)

systemctl enable --now wg-quick@wg0
changed && systemctl reload-or-restart wg-quick@wg0
reset_changed
