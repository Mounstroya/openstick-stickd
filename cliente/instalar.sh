#!/usr/bin/env bash
# Deja un stick en modo cliente: proxy HTTPS por WireGuard, con candado. Se corre desde la PC
# que tiene el stick por USB, con el binario ya compilado.
#
#   DOMINIOS=".operador1.com .operador2.com" USUARIO=cliente-x \
#   TUNEL=10.9.0.2 HUB=10.9.0.1 ./cliente/instalar.sh
#
# Si el stick no tiene /etc/wireguard/wg0.conf hay que dar además HUB_PUBKEY y HUB_ENDPOINT
# (ip:puerto). La llave privada se genera EN el stick y nunca sale de ahí.
# Opcionales: PUERTO=8888  PUERTOS=443  STICK=user@192.168.5.1  LLAVE=~/.ssh/id_ed25519_openstick
#
# NO cambia el modo del stick: cuando ya esté el WiFi del cliente guardado, elige
# «Cliente (proxy)» en el panel. Para deshacer: ./cliente/quitar.sh
set -euo pipefail
cd "$(dirname "$0")"
STICK=${STICK:-user@192.168.5.1}
LLAVE=${LLAVE:-$HOME/.ssh/id_ed25519_openstick}
PUERTO=${PUERTO:-8888}
PUERTOS=${PUERTOS:-443}
BIN=${BIN:-../target/aarch64-unknown-linux-gnu/release/stickd}
: "${TUNEL:?falta TUNEL (IP del stick en el túnel)}" "${HUB:?falta HUB (IP del servidor en el túnel)}"
: "${USUARIO:?falta USUARIO}" "${DOMINIOS:?falta DOMINIOS}"
for ip in "$TUNEL" "$HUB"; do [[ $ip =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || { echo "IP no válida: $ip"; exit 1; }; done
[[ $PUERTO =~ ^[0-9]{2,5}$ && $PUERTOS =~ ^[0-9,]+$ && $USUARIO =~ ^[A-Za-z0-9_-]+$ ]] || { echo "PUERTO, PUERTOS o USUARIO no válidos"; exit 1; }
[ -f "$BIN" ] || { echo "Falta compilar: $BIN"; exit 1; }
# SSH_OPC: opciones extra de ssh/scp, p. ej. "-o UserKnownHostsFile=$HOME/stick2_known_hosts"
read -r -a OPC <<< "${SSH_OPC:-}"
S=(ssh "${OPC[@]}" -i "$LLAVE" -o IdentitiesOnly=yes -o ConnectTimeout=8 "$STICK")

T=$(mktemp -d); trap 'rm -rf "$T"' EXIT; chmod 700 "$T"
CLAVE=$(head -c 24 /dev/urandom | base64 | tr -d '/+=')
{ echo "escucha = $TUNEL:$PUERTO"; echo "usuario = $USUARIO:$CLAVE"; for d in $DOMINIOS; do echo "permitir = $d"; done
  echo "puertos = $PUERTOS"; echo "max = 32"; } > "$T/proxy.conf"
sed -e "s/@HUB@/$HUB/g" -e "s/@PUERTO@/$PUERTO/g" guard.nft > "$T/cliente-guard.nft"
cp stickd-proxy.service stickd-cliente-guard.service "$T/"; cp "$BIN" "$T/stickd"

echo "==> Copiando"
R=$("${S[@]}" 'mktemp -d'); scp -q "${OPC[@]}" -i "$LLAVE" -o IdentitiesOnly=yes "$T"/* "$STICK:$R/"

echo "==> Instalando"
"${S[@]}" "sudo R=$R TUNEL=$TUNEL HUB=$HUB HUB_PUBKEY='${HUB_PUBKEY:-}' HUB_ENDPOINT='${HUB_ENDPOINT:-}' bash -s" <<'R'
set -euo pipefail
trap 'rm -rf "$R"' EXIT
id stickproxy >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin stickproxy
if [ ! -f /etc/wireguard/wg0.conf ]; then
  [ -n "$HUB_PUBKEY" ] && [ -n "$HUB_ENDPOINT" ] || { echo "No hay wg0.conf: faltan HUB_PUBKEY y HUB_ENDPOINT"; exit 1; }
  umask 077; mkdir -p /etc/wireguard
  wg genkey > /etc/wireguard/stick_private.key
  printf '[Interface]\nAddress = %s/32\nPostUp = wg set %%i private-key /etc/wireguard/stick_private.key\n\n[Peer]\nPublicKey = %s\nEndpoint = %s\nAllowedIPs = %s/32\nPersistentKeepalive = 25\n' \
    "$TUNEL" "$HUB_PUBKEY" "$HUB_ENDPOINT" "$HUB" > /etc/wireguard/wg0.conf
  umask 022
fi
mkdir -p /etc/stickd
# se revisa el candado ANTES de tocar nada más
nft -c -f "$R/cliente-guard.nft"
install -m600 "$R/proxy.conf" /etc/stickd/proxy.conf
install -m644 "$R/cliente-guard.nft" /etc/stickd/cliente-guard.nft
install -m644 "$R"/*.service /etc/systemd/system/
[ -e /var/tmp/stickd.antes-cliente ] || cp -a /usr/local/sbin/stickd /var/tmp/stickd.antes-cliente
if ! cmp -s "$R/stickd" /usr/local/sbin/stickd; then
  install -m755 "$R/stickd" /usr/local/sbin/stickd
  systemctl restart stickd
fi
# el SOCKS de antes (microsocks) deja ver la red local: se apaga
if systemctl cat microsocks.service >/dev/null 2>&1; then systemctl disable --now microsocks.service; fi
systemctl daemon-reload
systemctl enable --now wg-quick@wg0.service
systemctl enable stickd-cliente-guard.service stickd-proxy.service
systemctl restart stickd-cliente-guard.service
systemctl restart stickd-proxy.service
sleep 2
systemctl is-active stickd stickd-cliente-guard stickd-proxy wg-quick@wg0
echo "Llave pública del stick: $(wg show wg0 public-key)"
echo "Modo actual (no se cambió): $(cat /etc/stickd/modo 2>/dev/null)"
R
echo
echo "Proxy:  http://$USUARIO:$CLAVE@$TUNEL:$PUERTO"
echo "Guarda esa línea en el .env del servidor; no se vuelve a mostrar."
