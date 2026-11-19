//! Prueba en el hardware (dentro de `sudo flock /run/stickd-radio.lock -c '...'`):
//!   afluencia_prueba modulo <segundos>      → start() del módulo (monitor + saltos), diagnóstico cada 5 s, stop()
//!   afluencia_prueba crudo <iface> <seg>    → solo lee el socket de <iface> y cuenta tramas por tipo
//! Usa un directorio de datos temporal (no toca /var/lib/stickd). Nunca imprime MACs.
#![allow(dead_code)]
#[path = "../src/util.rs"]
mod util;
#[path = "../src/vitrina/mod.rs"]
mod vitrina;
/// Sustitutos: el ejemplo no compila el núcleo (portal cautivo ni red)
mod captive {
    pub fn cliente_tiene_internet(_ip: std::net::IpAddr) -> bool {
        false
    }
}
mod net {
    pub fn internet() -> bool {
        false
    }
}

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use vitrina::afluencia as af;
use vitrina::Vitrina;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(|s| s.as_str()) {
        Some("crudo") => crudo(a.get(2).map(|s| s.as_str()).unwrap_or("wlan0"), a.get(3).and_then(|s| s.parse().ok()).unwrap_or(10)),
        _ => modulo(a.get(2).and_then(|s| s.parse().ok()).unwrap_or(20)),
    }
}

fn crudo(iface: &str, secs: u64) {
    let fd = af::abrir_socket(iface).expect("socket");
    let mut buf = vec![0u8; 4096];
    let (mut n_tot, mut tipos, mut rssis, mut macs, mut aleat) = (0u64, BTreeMap::new(), Vec::new(), HashSet::new(), 0);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if let Some(n) = af::leer(&fd, &mut buf) {
            n_tot += 1;
            *tipos.entry(af::fc0(&buf[..n]).unwrap_or(0xff)).or_insert(0u32) += 1;
            if let Some(p) = af::parse_probe(&buf[..n]) {
                if let Some(r) = p.rssi {
                    rssis.push(r);
                }
                if p.aleatoria() {
                    aleat += 1;
                }
                macs.insert(p.mac); // solo para contar, no se imprime
            }
        }
    }
    let nombres = |f: u8| match f {
        0x40 => "probe_req",
        0x50 => "probe_resp",
        0x80 => "beacon",
        0xd0 => "action",
        0xb0 => "auth",
        0xc0 => "deauth",
        f if f & 0x0c == 0x08 => "datos",
        f if f & 0x0c == 0x04 => "control",
        _ => "otro",
    };
    let mut agg: BTreeMap<&str, u32> = BTreeMap::new();
    for (f, c) in &tipos {
        *agg.entry(nombres(*f)).or_insert(0) += c;
    }
    rssis.sort();
    println!("{iface} {secs}s: {n_tot} tramas {agg:?}");
    println!(
        "probe requests: {} tramas, {} MACs distintas ({} tramas con MAC aleatoria), RSSI min/med/max = {:?}/{:?}/{:?}",
        rssis.len(),
        macs.len(),
        aleat,
        rssis.first(),
        rssis.get(rssis.len() / 2),
        rssis.last()
    );
}

fn modulo(secs: u64) {
    af::SIN_LOCK.store(true, Ordering::Relaxed); // ya corremos dentro de flock
    let dir = std::path::PathBuf::from("/tmp/afluencia-prueba");
    let _ = std::fs::create_dir_all(&dir);
    let a = af::Afluencia::con_dir(dir);
    a.start();
    for i in 0..secs.div_ceil(5) {
        std::thread::sleep(Duration::from_secs(5));
        println!("[{:>3}s] {}", (i + 1) * 5, a.diagnostico());
    }
    println!("resumen: {}", a.resumen());
    a.stop();
    println!("detenido; wlan0 restaurado a managed");
}
