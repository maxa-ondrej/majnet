# 40-firewall — nftables per trust zone (design doc §7).
#
#   all nodes : SSH (22) + WireGuard (51820) from anywhere; Docker API +
#               everything else only over wg0
#   prod      : additionally 80/443, restricted to Cloudflare ranges
#   main      : additionally 8080 (GitHub webhooks) + 7600 (setup wizard —
#               nothing listens there after setup completes, ADR 0004)
#   private   : no public service ports at all

WG_PORT=${WG_LISTEN_PORT:-51820}

CF_SET=""
if [[ $NODE_ROLE == prod ]]; then
  log "fetching Cloudflare IP ranges"
  cf4=$(curl -fsS https://www.cloudflare.com/ips-v4 | paste -sd, -)
  cf6=$(curl -fsS https://www.cloudflare.com/ips-v6 | paste -sd, -)
  [[ -n $cf4 && -n $cf6 ]] || die "could not fetch Cloudflare ranges"
  CF_SET=$(cat <<EOF
  set cloudflare4 { type ipv4_addr; flags interval; elements = { $cf4 } }
  set cloudflare6 { type ipv6_addr; flags interval; elements = { $cf6 } }
EOF
)
fi

install_stdin /etc/nftables.conf 0644 <<EOF
#!/usr/sbin/nft -f
# Managed by majnet bootstrap (40-firewall.sh) — do not edit by hand.
flush ruleset

table inet filter {
$CF_SET
  chain input {
    type filter hook input priority filter; policy drop;

    ct state established,related accept
    ct state invalid drop
    iif lo accept
    icmp type { echo-request, destination-unreachable, time-exceeded } accept
    icmpv6 type { echo-request, destination-unreachable, time-exceeded, packet-too-big, nd-neighbor-solicit, nd-neighbor-advert, nd-router-advert } accept

    tcp dport 22 accept comment "SSH"
    udp dport $WG_PORT accept comment "WireGuard"

    # Cluster-internal traffic: anything on the WG interface.
    iifname "wg0" accept
$( [[ $NODE_ROLE == prod ]] && cat <<'PROD'

    # Public edge: only Cloudflare may reach edge-main.
    ip  saddr @cloudflare4 tcp dport { 80, 443 } accept
    ip6 saddr @cloudflare6 tcp dport { 80, 443 } accept
PROD
)$( [[ $NODE_ROLE == main ]] && cat <<'MAIN'

    # Control plane: GitHub webhooks (bot) + first-run setup wizard.
    tcp dport { 8080, 7600 } accept
MAIN
)
  }

  chain forward {
    type filter hook forward priority filter; policy accept;
    # Docker manages its own forward rules per bridge network.
  }
}
EOF

systemctl enable --now nftables
changed && nft -f /etc/nftables.conf
reset_changed

if [[ $NODE_ROLE == prod ]]; then
  # Cloudflare ranges drift — refresh weekly by re-running this step.
  install_stdin /etc/systemd/system/majnet-cf-refresh.service 0644 <<EOF
[Unit]
Description=Refresh Cloudflare IP ranges in nftables
[Service]
Type=oneshot
ExecStart=$(pwd)/bootstrap.sh 40
EOF
  install_stdin /etc/systemd/system/majnet-cf-refresh.timer 0644 <<'EOF'
[Unit]
Description=Weekly Cloudflare range refresh
[Timer]
OnCalendar=weekly
Persistent=true
[Install]
WantedBy=timers.target
EOF
  systemctl daemon-reload
  systemctl enable --now majnet-cf-refresh.timer
fi
