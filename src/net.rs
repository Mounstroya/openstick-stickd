//! Todo lo que toca la red: enlace USB con la laptop, WiFi, LTE, hotspot.
//! Prioridad por métrica de ruta: USB 50 < WiFi 600 < LTE 700 (menor gana).

use crate::util::{nm_fields, ok, run};

/// MAC fija del lado laptop del gadget ECM (/sys/kernel/config/usb_gadget/dual/functions/ecm.usb0/host_addr).
pub fn laptop_mac() -> String {
    std::fs::read_to_string("/sys/kernel/config/usb_gadget/dual/functions/ecm.usb0/host_addr")
        .map(|s| s.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_lowercase())
        .unwrap_or_else(|_| "2a:85:da:41:eb:f9".into())
}

pub const USB_METRIC: &str = "50";
const PROBE: &str = "1.0.0.1"; // solo se usa para probar si la laptop comparte internet

/// IP de la laptop en usb0, buscando su MAC en la tabla de vecinos.
pub fn laptop_ip() -> Option<String> {
    let mac = laptop_mac();
    let (_, out) = run("ip", &["neigh", "show", "dev", "usb0"]);
    out.lines()
        .find(|l| l.starts_with("192.168.5.") && l.contains(&mac) && !l.contains("FAILED") && !l.contains("INCOMPLETE"))
        .and_then(|l| l.split_whitespace().next().map(String::from))
        .or_else(|| {
            // la laptop pudo no haber hablado aún: un ping a broadcast llena la tabla
            let _ = run("ping", &["-c1", "-W1", "-b", "192.168.5.255"]);
            None
        })
}

/// ¿La laptop nos comparte internet? Ruta /32 de prueba vía la laptop y ping.
pub fn usb_uplink_works(laptop: &str) -> bool {
    let _ = run("ip", &["route", "replace", &format!("{PROBE}/32"), "via", laptop, "dev", "usb0"]);
    ok("ping", &["-c1", "-W2", PROBE])
}

pub fn set_usb_default(laptop: Option<&str>) {
    match laptop {
        Some(l) => {
            let _ = run("ip", &["route", "replace", "default", "via", l, "dev", "usb0", "metric", USB_METRIC]);
        }
        None => {
            let _ = run("ip", &["route", "del", "default", "dev", "usb0", "metric", USB_METRIC]);
        }
    }
}

pub fn internet() -> bool {
    ["1.1.1.1", "8.8.8.8", "9.9.9.9"].iter().any(|h| ok("ping", &["-c1", "-W2", h]))
}

/// Dispositivo de la ruta por defecto ganadora (menor métrica).
pub fn uplink_dev() -> Option<String> {
    let (_, out) = run("ip", &["-o", "route", "show", "default"]);
    let mut best: Option<(u32, String)> = None;
    for l in out.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let dev = w.iter().position(|x| *x == "dev").and_then(|i| w.get(i + 1)).map(|s| s.to_string());
        let metric = w.iter().position(|x| *x == "metric").and_then(|i| w.get(i + 1)).and_then(|m| m.parse().ok()).unwrap_or(0);
        if let Some(d) = dev {
            if best.as_ref().is_none_or(|(m, _)| metric < *m) {
                best = Some((metric, d));
            }
        }
    }
    best.map(|(_, d)| d)
}

pub fn con_active(name: &str) -> bool {
    let (_, out) = run("nmcli", &["-t", "-f", "NAME", "con", "show", "--active"]);
    out.lines().any(|l| l == name)
}

pub fn con_up(name: &str) -> bool {
    ok("nmcli", &["--wait", "30", "con", "up", name])
}

pub fn con_down(name: &str) {
    if con_active(name) {
        let _ = run("nmcli", &["con", "down", name]);
    }
}

pub fn set_autoconnect(name: &str, yes: bool) {
    let _ = run("nmcli", &["con", "modify", name, "connection.autoconnect", if yes { "yes" } else { "no" }]);
}

/// Conexiones WiFi cliente guardadas (excluye el hotspot y cualquier red que emita el stick).
pub fn saved_wifis() -> Vec<(String, String)> {
    let (_, out) = run("nmcli", &["-t", "-f", "NAME,TYPE", "con", "show"]);
    out.lines()
        .map(nm_fields)
        .filter(|f| f.len() >= 2 && f[1] == "802-11-wireless" && f[0] != "hotspot")
        .filter_map(|f| {
            let (_, out) = run("nmcli", &["-g", "802-11-wireless.mode,802-11-wireless.ssid", "con", "show", &f[0]]);
            // las redes que emite el propio stick (vitrina) no son redes a las que conectarse: al
            // salir de una vitrina, el escaneo veía su propio SSID y la volvía a levantar
            let (modo, ssid) = out.split_once('\n')?;
            (modo.trim() != "ap").then(|| (f[0].clone(), ssid.trim().to_string()))
        })
        .collect()
}

pub fn wifi_clients_up() -> bool {
    saved_wifis().iter().any(|(n, _)| con_active(n))
}

/// Conecta a la mejor red guardada que esté al alcance. NO usa `nmcli device connect`,
/// porque NM puede elegir el hotspot (prioridad 100) como "mejor" conexión.
pub fn connect_wifi() {
    let visible = scan();
    let mut cands: Vec<(u32, String)> = saved_wifis()
        .into_iter()
        .filter_map(|(name, ssid)| visible.iter().find(|(s, _, _)| *s == ssid).map(|(_, sig, _)| (*sig, name)))
        .collect();
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, name) in cands {
        if con_up(&name) {
            return;
        }
    }
}

pub fn wifis_down() {
    for (n, _) in saved_wifis() {
        con_down(&n);
    }
}

/// Escaneo de redes: (ssid, señal, seguridad). Debe hacerse ANTES de levantar el hotspot.
pub fn scan() -> Vec<(String, u32, String)> {
    let (_, out) = run("nmcli", &["-t", "-f", "SSID,SIGNAL,SECURITY", "dev", "wifi", "list", "--rescan", "yes"]);
    let mut v: Vec<(String, u32, String)> = out
        .lines()
        .map(nm_fields)
        .filter(|f| f.len() >= 3 && !f[0].is_empty())
        .map(|f| (f[0].clone(), f[1].parse().unwrap_or(0), f[2].clone()))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.dedup_by(|a, b| a.0 == b.0);
    v
}

pub fn add_wifi(ssid: &str, psk: &str) -> Result<String, String> {
    let name = format!("wifi-{}", ssid.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').take(24).collect::<String>());
    let _ = run("nmcli", &["con", "delete", &name]);
    let mut args = vec!["con", "add", "type", "wifi", "ifname", "wlan0", "con-name", &name, "ssid", ssid, "connection.autoconnect", "yes", "connection.autoconnect-priority", "10"];
    if !psk.is_empty() {
        args.extend(["wifi-sec.key-mgmt", "wpa-psk", "wifi-sec.psk", psk]);
    }
    let o = std::process::Command::new("nmcli").args(&args).output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(name) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

pub fn forget_wifi(name: &str) -> bool {
    // solo borra conexiones WiFi cliente, nunca hotspot/usb/lte
    saved_wifis().iter().any(|(n, _)| n == name) && ok("nmcli", &["con", "delete", name])
}

pub struct Lte {
    pub state: String,
    pub signal: String,
    pub operator: String,
    pub tech: String,
    pub sim: bool,
}

pub fn lte() -> Lte {
    let (_, out) = run("mmcli", &["-m", "any", "--output-keyvalue"]);
    let get = |k: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(k))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| v != "--")
            .unwrap_or_default()
    };
    let reason = get("modem.generic.state-failed-reason");
    Lte {
        state: get("modem.generic.state "),
        signal: get("modem.generic.signal-quality.value"),
        operator: get("modem.3gpp.operator-name"),
        tech: get("modem.generic.access-technologies"),
        sim: !reason.contains("sim-missing") && !out.is_empty(),
    }
}

pub fn addrs() -> String {
    run("ip", &["-br", "-4", "addr"]).1.lines().filter(|l| !l.starts_with("lo")).collect::<Vec<_>>().join("\n")
}

pub fn current_ssid() -> String {
    let (_, out) = run("nmcli", &["-t", "-f", "ACTIVE,SSID", "dev", "wifi"]);
    out.lines().map(nm_fields).find(|f| f.first().map(|s| s == "yes").unwrap_or(false)).and_then(|f| f.get(1).cloned()).unwrap_or_default()
}

pub fn wireguard() -> String {
    let (okk, out) = run("wg", &["show", "wg0", "latest-handshakes"]);
    if !okk {
        return "no configurado".into();
    }
    let ts: u64 = out.split_whitespace().nth(1).and_then(|t| t.parse().ok()).unwrap_or(0);
    if ts == 0 {
        return "sin handshake".into();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("último handshake hace {} s", now.saturating_sub(ts))
}
