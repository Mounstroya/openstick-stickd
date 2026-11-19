//! Contador de visitas: únicas por IP por día (día en hora local del stick).
//! Archivo pequeño de texto: `dia N`, `hoy N`, `total N`, `h <dia> <n>` (historial) e
//! `ip <ip>` (IPs ya contadas hoy, para no recontar si el daemon se reinicia).

use std::collections::HashSet;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const DIAS_HISTORIAL: usize = 30;

pub struct Visitas {
    archivo: PathBuf,
    pub dia: i64,
    pub hoy: u64,
    pub total: u64,
    ips: HashSet<IpAddr>,
    /// (día, visitas) de días anteriores, el más reciente al final
    pub historial: Vec<(i64, u64)>,
}

impl Visitas {
    pub fn cargar(archivo: PathBuf) -> Self {
        let mut v = Visitas { archivo, dia: 0, hoy: 0, total: 0, ips: HashSet::new(), historial: Vec::new() };
        if let Ok(t) = fs::read_to_string(&v.archivo) {
            for l in t.lines() {
                let p: Vec<&str> = l.split_whitespace().collect();
                match p.as_slice() {
                    ["dia", n] => v.dia = n.parse().unwrap_or(0),
                    ["hoy", n] => v.hoy = n.parse().unwrap_or(0),
                    ["total", n] => v.total = n.parse().unwrap_or(0),
                    ["h", d, n] => {
                        if let (Ok(d), Ok(n)) = (d.parse(), n.parse()) {
                            v.historial.push((d, n));
                        }
                    }
                    ["ip", ip] => {
                        if let Ok(ip) = ip.parse() {
                            v.ips.insert(ip);
                        }
                    }
                    _ => {}
                }
            }
        }
        v
    }

    fn guardar(&self) {
        let mut s = format!("dia {}\nhoy {}\ntotal {}\n", self.dia, self.hoy, self.total);
        for (d, n) in &self.historial {
            s.push_str(&format!("h {d} {n}\n"));
        }
        for ip in &self.ips {
            s.push_str(&format!("ip {ip}\n"));
        }
        let tmp = self.archivo.with_extension("tmp");
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, &self.archivo);
        }
    }

    /// Cambia de día si hace falta. Devuelve true si hubo cambio (hay que guardar).
    fn avanzar(&mut self, dia: i64) -> bool {
        if dia == self.dia {
            return false;
        }
        if self.hoy > 0 && self.dia > 0 {
            self.historial.push((self.dia, self.hoy));
            let sobra = self.historial.len().saturating_sub(DIAS_HISTORIAL);
            self.historial.drain(..sobra);
        }
        self.dia = dia;
        self.hoy = 0;
        self.ips.clear();
        true
    }

    /// Registra una visita de `ip` el día `dia`. Devuelve true si era nueva.
    pub fn registrar(&mut self, ip: IpAddr, dia: i64) -> bool {
        let cambio = self.avanzar(dia);
        let nueva = self.ips.insert(ip);
        if nueva {
            self.hoy += 1;
            self.total += 1;
        }
        if nueva || cambio {
            self.guardar();
        }
        nueva
    }

    /// Visitas de hoy (0 si el último registro fue otro día).
    pub fn de_hoy(&self, dia: i64) -> u64 {
        if self.dia == dia { self.hoy } else { 0 }
    }

    pub fn reiniciar(&mut self) {
        self.hoy = 0;
        self.total = 0;
        self.ips.clear();
        self.historial.clear();
        self.guardar();
    }
}

// ───────────── Fecha/hora local sin dependencias ─────────────

fn desfase() -> i64 {
    static O: OnceLock<i64> = OnceLock::new();
    *O.get_or_init(|| {
        let (ok, s) = crate::util::run("date", &["+%z"]);
        let s = s.trim();
        if !ok || s.len() != 5 {
            return 0;
        }
        let signo = if s.starts_with('-') { -1 } else { 1 };
        let h: i64 = s[1..3].parse().unwrap_or(0);
        let m: i64 = s[3..5].parse().unwrap_or(0);
        signo * (h * 3600 + m * 60)
    })
}

/// Segundos Unix ajustados a la hora local.
pub fn ahora_local() -> i64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    t + desfase()
}

pub fn dia_local() -> i64 {
    ahora_local().div_euclid(86400)
}

/// (año, mes, día) del número de días desde 1970-01-01 (algoritmo de H. Hinnant).
pub fn civil(dias: i64) -> (i64, u32, u32) {
    let z = dias + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// "18/11 14:05" a partir de segundos locales.
pub fn fecha_hora(seg: i64) -> String {
    let (y, m, d) = civil(seg.div_euclid(86400));
    let s = seg.rem_euclid(86400);
    format!("{d:02}/{m:02}/{y} {:02}:{:02}", s / 3600, (s % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fechas() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20410), (2025, 11, 18));
        assert_eq!(fecha_hora(20410 * 86400 + 14 * 3600 + 5 * 60), "18/11/2025 14:05");
    }

    #[test]
    fn cuenta_unicas_por_dia() {
        let dir = std::env::temp_dir().join(format!("pf-visitas-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("visitas.txt");
        let _ = fs::remove_file(&f);
        let a: IpAddr = "192.168.4.10".parse().unwrap();
        let b: IpAddr = "192.168.4.11".parse().unwrap();
        let mut v = Visitas::cargar(f.clone());
        assert!(v.registrar(a, 100));
        assert!(!v.registrar(a, 100));
        assert!(v.registrar(b, 100));
        assert_eq!((v.hoy, v.total), (2, 2));
        // tras reiniciar el daemon no recuenta
        let mut v = Visitas::cargar(f.clone());
        assert!(!v.registrar(a, 100));
        // día siguiente: la misma IP cuenta de nuevo
        assert!(v.registrar(a, 101));
        assert_eq!((v.de_hoy(101), v.total), (1, 3));
        assert_eq!(v.historial, vec![(100, 2)]);
        assert_eq!(v.de_hoy(102), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
