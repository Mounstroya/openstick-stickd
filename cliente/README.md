# Modo cliente: salida por el internet de donde está el stick

Deja un stick fijo en la casa o el local de un cliente, **con su permiso**, para que los sistemas
que ese cliente contrató consulten a su operador con su propio internet. El stick abre un túnel
WireGuard hacia el servidor (funciona detrás de CGNAT, sin abrir puertos) y ofrece ahí un proxy HTTPS.

```
servidor del cliente ──WireGuard──> stick ──WiFi del cliente──> solo los dominios permitidos
```

## Qué puede y qué no

| | |
|---|---|
| Salir a los dominios de `permitir`, en los puertos de `puertos` | sí |
| Cualquier otro sitio | no (403) |
| La red local del cliente (módem, cámaras, PCs) | no: lo bloquean el proxy y el firewall |
| Usarlo sin contraseña, o desde otro equipo del túnel | no |
| Entrar al panel o por SSH desde el WiFi del cliente | no |
| Ver lo que se consulta | no: es `CONNECT`, el tráfico va cifrado de punta a punta |
| Abrir solo su red de configuración cuando falla el internet | no: únicamente con el botón |

No es para esquivar bloqueos ni límites de un operador. Un stick atiende a un solo cliente.

## Instalar

0. Stick recién grabado con la imagen OpenStick: pon tu llave SSH y corre `./cliente/preparar.sh`.
   Regenera la llave SSH del equipo y el machine-id (la imagen trae **los mismos en todos los
   sticks**), quita las contraseñas de fábrica y cambia la del hotspot.
1. Compila (ver el README principal) y conecta el stick por USB (uno a la vez: todos usan la misma
   MAC e IP por USB).
2. `DOMINIOS=".operador.com" USUARIO=cliente-x TUNEL=10.9.0.2 HUB=10.9.0.1 ./cliente/instalar.sh`
3. En el servidor, agrega la llave pública que imprime como `[Peer]`, con `AllowedIPs = <TUNEL>/32`.
4. En el panel (`http://192.168.5.1`) guarda el WiFi del cliente (2.4 GHz) y elige **Cliente (proxy)**.
5. Si no usaste `preparar.sh`, cambia la contraseña del hotspot de configuración, que viene con una
   conocida: `nmcli con modify hotspot wifi-sec.psk <otra>`

Probar desde el servidor:

```sh
curl -x http://usuario:clave@10.9.0.2:8888 https://www.operador.com/   # responde
curl -x http://usuario:clave@10.9.0.2:8888 https://example.com/        # 403
curl -x http://usuario:clave@10.9.0.2:8888 https://192.168.1.1/        # 403
```

Deshacer: `./cliente/quitar.sh`.

## Cuidado

- **No clones la imagen de un stick a otro**: cada uno tiene su calibración de radio e IMEI, y la
  copia llevaría las llaves de WireGuard y las redes WiFi.
- Las llaves, IP, contraseñas y nombres de clientes **no van en este repo**.
- Si el cliente cambia la clave de su WiFi, hay que presionar el botón y guardarla otra vez.
- Si el stick se queda sin internet, el proxy no responde: el sistema debe tratarlo como falla, sin
  salir por otro lado sin avisar.
