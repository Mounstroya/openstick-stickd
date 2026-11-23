# Writing stickd plugins

A plugin lets anyone add a new module ("vitrina") to their stick by dropping a
single file in — no rebuild, no restart. A plugin is a **shared object (`.so`)
with a C ABI**, so you can write it in any language that compiles to one (C, Go
with `-buildmode=c-shared`, Zig, Rust with `crate-type = ["cdylib"]`, …).

The full, authoritative contract — every struct and every function, with the
memory and threading rules — is **[`abi.h`](abi.h)**. This page is the overview.

## What a plugin is

When your plugin is "on air", the stick brings up an open Wi-Fi with a captive
portal and sends every guest request to your `stickd_guest()`. You render your
page there. You also get a section in the admin panel
(`stickd_admin_html` / `stickd_admin_post`) and a one-line status
(`stickd_resumen`). Your persistent data lives in `/var/lib/stickd/<your-id>/`.

Your plugin must export every obligatory symbol listed in `abi.h`
(`stickd_init`, `stickd_get_info`, `stickd_guest`, `stickd_admin_html`,
`stickd_admin_post`, `stickd_resumen`, `stickd_start`, `stickd_stop`,
`stickd_shutdown`, and the two `*_free_*` helpers).

See **[`../plugin-ejemplo/hola.c`](../plugin-ejemplo/hola.c)** for a complete,
minimal example.

## The host API — what the stick lets you do

`stickd_init()` hands you a `stickd_host` table of functions the stick exposes,
so you don't have to shell out yourself. Store the pointer. The functions:

| Function | What it does |
|---|---|
| `log(msg)` | Write a line to the stick's journal (`journalctl -u stickd`) |
| `free_string(s)` | Free any `char*` returned by the functions below |
| `uplink_json()` | Uplink status: internet yes/no, exit (usb/wifi/lte), SSID, LTE signal/operator, WireGuard |
| `wifi_scan_json()` | Nearby Wi-Fi networks (SSID, signal, security) |
| `http_get(url, &status)` | HTTP(S) GET through the active uplink (text, ≤ 8 MB) |
| `sms_send(number, text)` | Send an SMS through the modem (if there's a SIM) |
| `sms_list_json()` | SMS stored on the modem |
| `led(which, pattern)` | Red/green/blue LED: `on`/`off`/`timer`/`heartbeat` |
| `guests_json()` | Devices connected to the showcase Wi-Fi |

Functions returning `char*` must be freed with `host->free_string()`. Exact JSON
shapes are documented in `abi.h`.

## Compiling for the stick (aarch64)

The stick is **aarch64, Debian (glibc)**. Build a native aarch64 `.so`.

**From your PC (recommended — keeps the stick lean):**

```sh
# Debian/Ubuntu: sudo apt install gcc-aarch64-linux-gnu
aarch64-linux-gnu-gcc -shared -fPIC -O2 -o mi.so mi.c
```

**On the stick itself**, if it has a compiler installed:

```sh
gcc -shared -fPIC -O2 -o mi.so mi.c
```

**Without any toolchain**, let GitHub Actions compile it: put your `.c` in
[`../plugins-comunidad/`](../plugins-comunidad/), push (or open a PR), and download the
compiled `.so` from the Actions run. See that folder's README.

## Installing / removing

- **Panel:** `Plugins → Subir` (pick the `.so`). It loads in place, no restart.
- **Or copy the file:** `scp mi.so user@192.168.5.1:/var/lib/stickd/plugins/`
  then in the panel: `Plugins → Recargar carpeta`.
- **Remove:** `Plugins → Quitar` (deletes the file and unloads the module).

## Security — read before loading someone else's plugin

A plugin is **native code running as root inside the stickd process**. There is
**no sandbox**. Installing a `.so` means trusting its author completely — it can
do anything on the device. Only load plugins whose source you've read, or that
you compiled yourself. The host API exists so well-behaved plugins never need to
touch the system directly; it is not a security boundary.
