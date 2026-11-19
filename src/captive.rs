//! Núcleo del portal cautivo para las vitrinas:
//!   - DNS falso en :5353 (todo nombre → 192.168.4.1) para que el teléfono "vea" un portal
//!   - servidor HTTP de invitados en :8080 que atiende las URLs de detección y pasa el resto
//!     al módulo activo
//!   - reglas nft (tabla inet stickd_captive) SOLO sobre wlan0: DNS→5353, HTTP→8080, y todo
//!     lo demás bloqueado (sin SSH, sin panel de admin, sin reenvío a internet)

use crate::util::{log, run};
use crate::vitrina::{GuestReq, GuestResp, Vitrina};
use std::io::Read;
use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

pub const AP_IP: [u8; 4] = [192, 168, 4, 1];
const GUEST_PORT: u16 = 8080;
const DNS_PORT: u16 = 5353;

/// Índice del módulo activo en el vector de vitrinas (None = ninguna vitrina con AP activa).
pub type Activa = Arc<Mutex<Option<usize>>>;

const NFT: &str = r#"table inet stickd_captive
delete table inet stickd_captive
table inet stickd_captive {
  chain entrada_nat {
    type nat hook prerouting priority dstnat; policy accept;
    iifname "wlan0" udp dport 53 dnat ip to 192.168.4.1:5353
    iifname "wlan0" tcp dport 53 dnat ip to 192.168.4.1:5353
    iifname "wlan0" tcp dport 80 dnat ip to 192.168.4.1:8080
  }
  chain entrada {
    type filter hook input priority -10; policy accept;
    iifname "wlan0" ct state established,related accept
    iifname "wlan0" udp dport { 67, 5353 } accept
    iifname "wlan0" tcp dport 8080 accept
    iifname "wlan0" icmp type echo-request accept
    iifname "wlan0" drop
  }
  chain reenvio {
    type filter hook forward priority -10; policy accept;
    iifname "wlan0" drop
    oifname "wlan0" drop
  }
}
"#;

pub fn firewall_on() -> bool {
    let path = "/run/stickd-captive.nft";
    if std::fs::write(path, NFT).is_err() {
        return false;
    }
    let ok = run("nft", &["-f", path]).0;
    if !ok {
        log("captive: no pude cargar las reglas nft");
    }
    ok
}

pub fn firewall_off() {
    let _ = run("nft", &["delete", "table", "inet", "stickd_captive"]);
}

pub fn spawn(vitrinas: Arc<Vec<Box<dyn Vitrina>>>, activa: Activa) {
    std::thread::spawn(dns_server);
    std::thread::spawn(move || guest_server(vitrinas, activa));
}

// ---------------------------------------------------------------- DNS

fn dns_server() {
    let sock = match UdpSocket::bind(("0.0.0.0", DNS_PORT)) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("captive: DNS no pudo abrir :{DNS_PORT}: {e}"));
            return;
        }
    };
    let mut buf = [0u8; 512];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        if let Some(resp) = dns_answer(&buf[..n]) {
            let _ = sock.send_to(&resp, from);
        }
    }
}

/// Responde cualquier pregunta A con 192.168.4.1 (TTL 10 s). AAAA y demás: sin respuestas.
pub fn dns_answer(q: &[u8]) -> Option<Vec<u8>> {
    if q.len() < 12 || q[2] & 0x80 != 0 {
        return None; // muy corto o ya es respuesta
    }
    let qdcount = u16::from_be_bytes([q[4], q[5]]);
    if qdcount != 1 {
        return None;
    }
    // recorrer el QNAME
    let mut i = 12;
    loop {
        let len = *q.get(i)? as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        i += 1 + len;
    }
    let qtype = u16::from_be_bytes([*q.get(i)?, *q.get(i + 1)?]);
    let qend = i + 4;
    if qend > q.len() {
        return None;
    }
    let mut r = Vec::with_capacity(qend + 16);
    r.extend_from_slice(&q[0..2]); // id
    r.push(0x84 | (q[2] & 0x01)); // QR=1, AA=1, RD copiado
    r.push(0x80); // RA=1, RCODE=0
    r.extend_from_slice(&[0, 1]); // QDCOUNT
    let an: u16 = if qtype == 1 { 1 } else { 0 };
    r.extend_from_slice(&an.to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]); // NS, AR
    r.extend_from_slice(&q[12..qend]);
    if an == 1 {
        r.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 10, 0, 4]);
        r.extend_from_slice(&AP_IP);
    }
    Some(r)
}

// ---------------------------------------------------------------- HTTP invitados

/// URLs que usan los sistemas para detectar portal cautivo.
pub fn is_probe(host: &str, path: &str) -> bool {
    let h = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    matches!(path, "/generate_204" | "/gen_204" | "/hotspot-detect.html" | "/library/test/success.html" | "/connecttest.txt" | "/ncsi.txt" | "/redirect" | "/canonical.html" | "/success.txt")
        || h.ends_with("captive.apple.com")
        || h.contains("connectivitycheck")
        || h.contains("msftconnecttest")
        || h.contains("msftncsi")
        || h == "detectportal.firefox.com"
        || h == "nmcheck.gnome.org"
}

fn guest_server(vitrinas: Arc<Vec<Box<dyn Vitrina>>>, activa: Activa) {
    let server = match Server::http(("0.0.0.0", GUEST_PORT)) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("captive: no pude abrir :{GUEST_PORT}: {e}"));
            return;
        }
    };
    for mut req in server.incoming_requests() {
        let ip = req.remote_addr().map(|a| a.ip());
        let permitido = match ip {
            Some(IpAddr::V4(v)) => {
                let o = v.octets();
                v.is_loopback() || (o[0] == 192 && o[1] == 168 && (o[2] == 4 || o[2] == 5))
            }
            _ => false,
        };
        let idx = *activa.lock().unwrap();
        let (Some(idx), true) = (idx, permitido) else {
            let _ = req.respond(Response::from_string("no hay vitrina activa").with_status_code(503));
            continue;
        };
        let v = &vitrinas[idx];
        let hdr = |name: &'static str| {
            req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string()).unwrap_or_default()
        };
        let host = hdr("Host");
        let ua = hdr("User-Agent");
        let ct = hdr("Content-Type");
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let (path, query) = (path.to_string(), query.to_string());
        let method = req.method().as_str().to_string();

        let resp = if is_probe(&host, &path) {
            GuestResp::redirect("http://192.168.4.1/")
        } else {
            let mut body = Vec::new();
            let limit = v.max_body() as u64 + 1;
            let _ = req.as_reader().take(limit).read_to_end(&mut body);
            if body.len() as u64 >= limit {
                GuestResp { status: 413, content_type: "text/plain; charset=utf-8".into(), headers: vec![], body: "archivo demasiado grande".into() }
            } else {
                let g = GuestReq { method: &method, path: &path, query: &query, host: &host, user_agent: &ua, content_type: &ct, body: &body, client_ip: ip.unwrap() };
                v.guest(&g)
            }
        };
        let _ = req.respond(to_tiny(resp));
    }
}

pub fn to_tiny(r: GuestResp) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut resp = Response::from_data(r.body).with_status_code(r.status);
    if let Ok(h) = Header::from_bytes("Content-Type", r.content_type.as_bytes()) {
        resp.add_header(h);
    }
    // nada de caché: el mini-navegador de portal cautivo es muy agresivo cacheando
    resp.add_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    for (k, v) in r.headers {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    resp
}

// ---------------------------------------------------------------- AP

/// Crea o actualiza la conexión NM "vitrina" (abierta) con el SSID dado y la levanta.
pub fn ap_up(ssid: &str) -> bool {
    let exists = run("nmcli", &["-t", "-f", "NAME", "con", "show"]).1.lines().any(|l| l == "vitrina");
    if !exists {
        let ok = run(
            "nmcli",
            &[
                "con", "add", "type", "wifi", "ifname", "wlan0", "con-name", "vitrina", "ssid", ssid, "802-11-wireless.mode", "ap",
                "802-11-wireless.band", "bg", "ipv4.method", "shared", "ipv4.addresses", "192.168.4.1/24", "ipv6.method", "ignore",
                "connection.autoconnect", "no", "connection.autoconnect-priority", "-10",
            ],
        )
        .0;
        if !ok {
            log("captive: no pude crear la conexión vitrina");
            return false;
        }
    } else {
        let _ = run("nmcli", &["con", "modify", "vitrina", "802-11-wireless.ssid", ssid]);
    }
    run("nmcli", &["--wait", "30", "con", "up", "vitrina"]).0
}

pub fn ap_down() {
    let _ = run("nmcli", &["con", "down", "vitrina"]);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dns_responde_a() {
        // id=0x1234, RD, 1 pregunta: example.com A IN
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(b"\x07example\x03com\x00\x00\x01\x00\x01");
        let r = dns_answer(&q).unwrap();
        assert_eq!(&r[0..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1);
        assert_eq!(&r[r.len() - 4..], &AP_IP);
    }
    #[test]
    fn dns_aaaa_sin_respuesta() {
        let mut q = vec![0, 1, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(b"\x01a\x00\x00\x1c\x00\x01");
        let r = dns_answer(&q).unwrap();
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0);
    }
    #[test]
    fn detecta_sondas() {
        assert!(is_probe("connectivitycheck.gstatic.com", "/generate_204"));
        assert!(is_probe("captive.apple.com", "/hotspot-detect.html"));
        assert!(is_probe("www.msftconnecttest.com", "/connecttest.txt"));
        assert!(!is_probe("192.168.4.1", "/"));
    }
}

/// ¿Este invitado ya tiene acceso a internet (aceptó "Continuar con internet" y no ha expirado)?
/// ESQUELETO: lo implementa el módulo de internet para invitados. Hoy siempre es `false`.
#[allow(dead_code)]
pub fn cliente_tiene_internet(_ip: std::net::IpAddr) -> bool {
    false
}
