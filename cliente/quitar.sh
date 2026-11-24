#!/usr/bin/env bash
# Deshace cliente/instalar.sh: quita el proxy y el candado y regresa el stickd anterior.
# No borra WireGuard. SOCKS=1 vuelve a prender microsocks si existía (OJO: deja ver la red local).
set -euo pipefail
STICK=${STICK:-user@192.168.5.1}
LLAVE=${LLAVE:-$HOME/.ssh/id_ed25519_openstick}
ssh -i "$LLAVE" -o ConnectTimeout=8 "$STICK" "sudo SOCKS=${SOCKS:-0} bash -s" <<'R'
set -euo pipefail
systemctl disable --now stickd-proxy.service stickd-cliente-guard.service || true
nft delete table inet stickd_cliente 2>/dev/null || true
rm -f /etc/systemd/system/stickd-proxy.service /etc/systemd/system/stickd-cliente-guard.service /etc/stickd/proxy.conf /etc/stickd/cliente-guard.nft
systemctl daemon-reload
if [ -f /var/tmp/stickd.antes-cliente ]; then
  # ese stickd no conoce el modo cliente
  grep -qx cliente /etc/stickd/modo 2>/dev/null && echo auto > /etc/stickd/modo
  install -m755 /var/tmp/stickd.antes-cliente /usr/local/sbin/stickd
  rm -f /var/tmp/stickd.antes-cliente
  systemctl restart stickd
fi
if [ "$SOCKS" = 1 ]; then systemctl enable --now microsocks.service; fi
echo "Listo. stickd: $(systemctl is-active stickd)"
R
