# openstick-stickd

A tiny daemon that turns a cheap **4G LTE USB dongle** (UZ801, Snapdragon 410) running
[OpenStick](https://openstick.org) Debian into a pocket network appliance: it picks the best
internet uplink on its own, and it can broadcast its own Wi-Fi with a captive portal that shows
your portfolio, a business card, an offline chat, a file drop, and more. No internet is needed
for any of that.

## Why Rust

The hardware is very limited, and that decided the language:

| | UZ801 dongle |
|---|---|
| CPU | 4× Cortex-A53 @ 1 GHz (aarch64) |
| RAM | **317 MB** total, about 230 MB free, no swap |
| Storage | 3.3 GB eMMC |
| Base system | Debian 13, with **no Python, no curl, no busybox** out of the box |

A Python or Node implementation would need 20–50 MB of RAM and a runtime installed on the
device. `stickd` is **one statically linked binary (~1 MB, musl)** that uses **under 1 MB of
RAM** at runtime. It is cross-compiled on a laptop and copied over, so nothing has to be
installed on the stick. The only dependencies are `tiny_http`, `qrcode` and `libc`.

## What it does

### Automatic uplink selection
The lowest route metric wins:

1. **USB**: if the laptop the stick is plugged into shares its internet (metric 50)
2. **Wi-Fi**: a saved Wi-Fi network (metric 600)
3. **LTE**: the SIM card (metric 700)
4. **Setup mode**: if there is no internet for 2 minutes, or when you press the button, it
   brings up a password-protected hotspot with the admin panel

### "Showcases" (vitrinas)
These are modes where the stick runs its own **open Wi-Fi and captive portal**, so whoever
connects sees a page right away:

| Showcase | What visitors get |
|---|---|
| **Portfolio** | Your website, either uploaded or an offline copy of a URL (with a manual ↻ Sync button) |
| **Business card** | A contact card with "Save contact" (vCard) and a QR code |
| **Offline mailbox** | Leave messages and files, with no internet |
| **Local chat** | Chat with everyone on the stick's Wi-Fi, moderated by the host |
| **File sharing** | AirDrop-style transfers between any phones/PCs over **WebRTC, peer to peer**; the stick only does signaling |
| **Foot traffic** | Estimates nearby phones from Wi-Fi probe requests, with MACs anonymized |

Any showcase can **start automatically when the stick is on a power bank**. It detects that no
computer is attached to its USB port.

### Guest internet (optional)
Visitors can tap "Continue with internet" and get time-limited, speed-limited, data-capped
access through the stick's uplink. They are isolated from private networks.

### Hardware
- **LEDs**: red = power; green = Wi-Fi / setup / showcase; blue = internet via USB, Wi-Fi or LTE
- **Button** (pinhole): short press toggles setup mode; holding it 8 s reboots

## Building

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl   # linker: rust-lld (see .cargo/config.toml)
cargo test
```

## Installing on the stick

```sh
scp target/aarch64-unknown-linux-musl/release/stickd user@192.168.5.1:/tmp/
ssh user@192.168.5.1 'sudo install -m755 /tmp/stickd /usr/local/sbin/stickd'
```

Then add a systemd unit that runs `/usr/local/sbin/stickd` as root with `Restart=always`. Also
set `HandleRebootKey=ignore` in logind, so the button belongs to `stickd`. The admin panel is
at **http://192.168.5.1** over USB, or at http://192.168.4.1 from the setup hotspot.

`laptop/` contains a NetworkManager dispatcher script that shares the laptop's internet with
the stick automatically when it is plugged in.

## Security notes

- While a showcase is on air, visitors on the open Wi-Fi can reach **only** the guest server.
  They cannot reach the admin panel or SSH, and have no forwarding unless guest internet grants it.
- The admin panel rejects cross-origin POSTs (CSRF) and everything shown is HTML-escaped.
- The setup hotspot uses OpenStick's default password (`openstick`). Change it on your device.

## Known limitations

- **File sharing on iPhone**: the portal is served over plain `http://`. Safari keeps received
  files in RAM until the transfer finishes, so very large files (over ~1–2 GB) may fail on iOS.
  Android and desktop browsers spool to disk and have no practical limit. HTTPS with a real
  certificate would remove this limit (planned).
- **iOS captive portal window**: it cannot download files or open Safari by itself. The page
  tells the user to tap "Done" and open Safari.
- **Foot traffic**: the wcn36xx Wi-Fi chip delivers no frames in pure monitor mode. Counting
  works by running a hidden AP with a monitor interface alongside it, on a single channel.
- The kernel has no `tc` qdiscs (HTB/TBF), so guest speed limits use nftables rate meters,
  which drop packets instead of queuing them.
