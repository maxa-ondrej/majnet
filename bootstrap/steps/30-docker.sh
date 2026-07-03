# 30-docker — Docker CE with the API bound ONLY to the WireGuard IP, mTLS
# required (client certs held by the reconciler). Design doc §5, §7.

if ! command -v docker &>/dev/null; then
  log "installing Docker CE"
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  # shellcheck source=/dev/null  # /etc/os-release exists only on the node
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update -q
fi
apt_ensure docker-ce docker-ce-cli containerd.io

WG_IP=${WG_ADDRESS%/*}
PKI=/etc/majnet/pki
for f in ca.pem server-cert.pem server-key.pem; do
  [[ -f $PKI/$f ]] || die "missing $PKI/$f — generate with bootstrap/pki/gen-certs.sh and copy over"
done

install_stdin /etc/docker/daemon.json 0644 <<EOF
{
  "hosts": ["unix:///var/run/docker.sock", "tcp://$WG_IP:${DOCKER_API_PORT:-2376}"],
  "tls": true,
  "tlsverify": true,
  "tlscacert": "$PKI/ca.pem",
  "tlscert": "$PKI/server-cert.pem",
  "tlskey": "$PKI/server-key.pem",
  "live-restore": true,
  "log-driver": "local",
  "log-opts": { "max-size": "20m", "max-file": "3" }
}
EOF

# daemon.json "hosts" conflicts with the packaged unit's -H flag — clear it.
install_stdin /etc/systemd/system/docker.service.d/majnet.conf 0644 <<'EOF'
[Unit]
# Docker must not start before wg0 exists, or the tcp bind fails.
After=wg-quick@wg0.service
Requires=wg-quick@wg0.service

[Service]
ExecStart=
ExecStart=/usr/bin/dockerd --containerd=/run/containerd/containerd.sock
EOF

systemctl daemon-reload
systemctl enable --now docker
changed && systemctl restart docker
reset_changed
