# 60-backups — nightly DB dumps → restic → offsite (design doc §15).
# Needs /etc/majnet/restic.env with RESTIC_REPOSITORY, RESTIC_PASSWORD and
# any backend creds (B2_ACCOUNT_ID/…). Skipped entirely when absent.

if [[ ! -f /etc/majnet/restic.env ]]; then
  warn "no /etc/majnet/restic.env — skipping backup setup (phase-5 wiring)"
  return 0
fi

install_stdin /usr/local/bin/majnet-backup 0755 <<'EOF'
#!/usr/bin/env bash
# Nightly: dump every majnet DB engine present on this node, then restic.
set -euo pipefail
source /etc/majnet/restic.env
DUMP_DIR=/var/backups/majnet
mkdir -p "$DUMP_DIR"

if docker container inspect majnet-postgres &>/dev/null; then
  docker exec majnet-postgres pg_dumpall -U postgres | gzip > "$DUMP_DIR/postgres.sql.gz"
fi
if docker container inspect majnet-mariadb &>/dev/null; then
  docker exec majnet-mariadb sh -c 'mariadb-dump -uroot -p"$MARIADB_ROOT_PASSWORD" --all-databases' | gzip > "$DUMP_DIR/mariadb.sql.gz"
fi

restic backup "$DUMP_DIR" --tag majnet --host "$(hostname)"
restic forget --tag majnet --host "$(hostname)" --keep-daily 14 --keep-weekly 8 --prune
EOF

install_stdin /etc/systemd/system/majnet-backup.service 0644 <<'EOF'
[Unit]
Description=MajNet nightly backup (dumps + restic)
[Service]
Type=oneshot
ExecStart=/usr/local/bin/majnet-backup
EOF

install_stdin /etc/systemd/system/majnet-backup.timer 0644 <<'EOF'
[Unit]
Description=MajNet nightly backup
[Timer]
OnCalendar=*-*-* 03:30:00
RandomizedDelaySec=15m
Persistent=true
[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable --now majnet-backup.timer
reset_changed
