#!/usr/bin/env bash
# Compila stickd para el stick (aarch64, glibc), corre los tests y lo instala,
# respaldando el binario anterior y SIN cambiar el modo actual del aparato.
#
#   ./desplegar.sh
#
# Requisitos (una sola vez):
#   rustup target add aarch64-unknown-linux-gnu
#   sudo apt install gcc-aarch64-linux-gnu
set -euo pipefail

STICK=${STICK:-user@192.168.5.1}
LLAVE=${LLAVE:-$HOME/.ssh/id_ed25519_openstick}
TARGET=aarch64-unknown-linux-gnu
BIN=target/$TARGET/release/stickd
SSH="ssh -i $LLAVE $STICK"

cd "$(dirname "$0")"

echo "==> Tests"
cargo test --quiet

echo "==> Compilando release ($TARGET)"
cargo build --release --target "$TARGET"

echo "==> Modo actual del stick (para no cambiarlo)"
MODO=$($SSH 'cat /etc/stickd/modo' || echo '?')
echo "    modo = $MODO"

echo "==> Instalando en $STICK"
scp -q -i "$LLAVE" "$BIN" "$STICK:/tmp/stickd-nuevo"
$SSH 'sudo cp -a /usr/local/sbin/stickd /var/tmp/stickd.anterior \
   && sudo install -m755 /tmp/stickd-nuevo /usr/local/sbin/stickd \
   && sudo systemctl restart stickd \
   && rm -f /tmp/stickd-nuevo \
   && sleep 2 \
   && echo "    estado: $(systemctl is-active stickd)  modo: $(cat /etc/stickd/modo)"'

echo "==> Listo. Si algo falla, el binario anterior quedó en /var/tmp/stickd.anterior"
