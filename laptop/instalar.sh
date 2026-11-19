#!/bin/sh
set -e
install -m 755 -o root -g root /home/yaelmontoya/openstick/laptop/90-openstick-compartir /etc/NetworkManager/dispatcher.d/90-openstick-compartir
nmcli con modify "Conexión cableada 3" connection.id "OpenStick USB" 802-3-ethernet.mac-address 2a:85:da:41:eb:f9 2>/dev/null || true
/etc/NetworkManager/dispatcher.d/90-openstick-compartir enx2a85da41ebf9 up
iptables -S FORWARD | grep openstick; iptables -t nat -S POSTROUTING | grep openstick
