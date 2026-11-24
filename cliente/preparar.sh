#!/usr/bin/env bash
# Prepara un stick RECIÉN grabado con la imagen OpenStick (una vez, antes de instalar.sh). Se corre
# desde la PC que lo tiene por USB y le comparte internet. Ya debe entrar tu llave por SSH.
#
#   ./cliente/preparar.sh
#
#   - regenera la llave SSH del equipo y el machine-id: la imagen trae los mismos en todos
#   - bloquea las contraseñas de fábrica; solo se entra con llave
#   - pone una contraseña al azar al hotspot de configuración (se imprime una sola vez)
#   - DNS, botón, zona horaria e instala stickd
set -euo pipefail
cd "$(dirname "$0")"
STICK=${STICK:-user@192.168.5.1}
LLAVE=${LLAVE:-$HOME/.ssh/id_ed25519_openstick}
ZONA=${ZONA:-America/Mexico_City}
BIN=${BIN:-../target/aarch64-unknown-linux-gnu/release/stickd}
# SSH_OPC: opciones extra de ssh/scp, p. ej. "-o UserKnownHostsFile=$HOME/stick2_known_hosts"
read -r -a OPC <<< "${SSH_OPC:-}"
S=(ssh -n "${OPC[@]}" -i "$LLAVE" -o IdentitiesOnly=yes -o ConnectTimeout=8 "$STICK")
[ -f "$BIN" ] || { echo "Falta compilar: $BIN"; exit 1; }
[[ $ZONA =~ ^[A-Za-z_/]+$ ]] || { echo "ZONA no válida"; exit 1; }
"${S[@]}" 'test -s ~/.ssh/authorized_keys && sudo -n true' || { echo "No entra tu llave o no hay sudo"; exit 1; }
PSK=$(head -c 32 /dev/urandom | base64 | tr -dc 'a-z2-9' | head -c 12)
[ ${#PSK} = 12 ] || PSK=$(head -c 64 /dev/urandom | base64 | tr -dc 'a-z2-9' | head -c 12)

R=$("${S[@]}" 'mktemp -d')
scp -q "${OPC[@]}" -i "$LLAVE" -o IdentitiesOnly=yes "$BIN" stickd.service "$STICK:$R/"
PC=$("${S[@]}" 'echo ${SSH_CONNECTION%% *}')
cat <<'FIN' | ssh "${OPC[@]}" -i "$LLAVE" -o IdentitiesOnly=yes "$STICK" "sudo R=$R PSK=$PSK PC=$PC ZONA=$ZONA bash -s"
set -euo pipefail
trap 'rm -rf "$R"' EXIT
timedatectl set-timezone "$ZONA"
# DNS fijo: el dnsmasq principal de NetworkManager no corre en todos los modos
printf '[main]\ndns=none\nrc-manager=unmanaged\n' > /etc/NetworkManager/conf.d/90-stickd-dns.conf
printf '[keyfile]\nunmanaged-devices=interface-name:wg0\n' > /etc/NetworkManager/conf.d/99-unmanaged-wg.conf
rm -f /etc/resolv.conf; printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\nnameserver 9.9.9.9\n' > /etc/resolv.conf
nmcli general reload
ip route replace default via "$PC" dev usb0 metric 50
for i in 1 2 3 4 5 6; do getent hosts deb.debian.org >/dev/null && break; sleep 2; done
for i in $(seq 30); do [ "$(timedatectl show -p NTPSynchronized --value)" = yes ] && break; sleep 2; done
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends wireguard-tools nftables iw curl >/dev/null
# contraseñas fuera
printf 'PasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin no\n' > /etc/ssh/sshd_config.d/10-solo-llaves.conf
passwd -l user >/dev/null; passwd -l root >/dev/null
[ -d /root/.ssh ] && rm -f /root/.ssh/authorized_keys
# identidad propia
rm -f /etc/ssh/ssh_host_*; ssh-keygen -A >/dev/null
rm -f /etc/machine-id /var/lib/dbus/machine-id; systemd-machine-id-setup >/dev/null
[ -e /var/lib/dbus ] && ln -sf /etc/machine-id /var/lib/dbus/machine-id
sshd -t
# el botón es de stickd; el hotspot no arranca solo y con clave propia
mkdir -p /etc/systemd/logind.conf.d /etc/stickd
printf '[Login]\nHandleRebootKey=ignore\nHandleRebootKeyLongPress=ignore\n' > /etc/systemd/logind.conf.d/stickd-boton.conf
nmcli con modify hotspot wifi-sec.psk "$PSK" connection.autoconnect no connection.autoconnect-priority 0
install -m755 "$R/stickd" /usr/local/sbin/stickd
install -m644 "$R/stickd.service" /etc/systemd/system/stickd.service
[ -f /etc/stickd/modo ] || echo auto > /etc/stickd/modo
systemctl daemon-reload
systemctl enable -q stickd.service
systemctl restart systemd-logind ssh stickd
sleep 3; echo "stickd: $(systemctl is-active stickd)  machine-id: $(cut -c1-8 /etc/machine-id)"
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
FIN
echo "Clave del hotspot de configuración: $PSK   (anótala; no se vuelve a mostrar)"
echo "La huella SSH del stick cambió: borra la anterior con  ssh-keygen -R 192.168.5.1"
