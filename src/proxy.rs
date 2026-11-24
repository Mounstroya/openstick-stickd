//! Proxy HTTP CONNECT del modo cliente: `stickd proxy <archivo>`.
//!
//! Corre como proceso APARTE y sin privilegios (cliente/stickd-proxy.service), nunca dentro del
//! stickd que es root. Deja salir a internet con la IP de donde está el stick, pero SOLO hacia los
//! dominios y puertos de su configuración, y nunca hacia redes privadas (la casa del cliente):
//!   - usuario y contraseña obligatorios; sin ellos, o sin dominios, no arranca;
//!   - solo el método CONNECT (HTTPS), así que no ve el contenido;
//!   - la IP se resuelve y se revisa aquí, y se conecta a ESA IP: un dominio permitido que apunte
//!     a una dirección privada se rechaza;
//!   - tope de conexiones y tiempos límite.
//! El firewall (cliente/guard.nft) repite el bloqueo de redes privadas por fuera.

use crate::util::log;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const MAX_CABECERA: usize = 8 * 1024;
const T_CABECERA: Duration = Duration::from_secs(10);
const T_CONECTAR: Duration = Duration::from_secs(10);
const T_INACTIVO: Duration = Duration::from_secs(120);
const PILA: usize = 128 * 1024;

pub struct Conf {
    escucha: SocketAddr,
    /// "usuario:contraseña"
    usuarios: Vec<String>,
    /// ".dominio.com" = el dominio y sus subdominios; "host.dominio.com" = solo ese
    permitir: Vec<String>,
    puertos: Vec<u16>,
    max: usize,
    ip_ok: fn(&IpAddr) -> bool,
    resolver: fn(&str, u16) -> Vec<SocketAddr>,
}

impl Conf {
    pub fn leer(texto: &str) -> Result<Conf, String> {
        let mut c = Conf {
            escucha: SocketAddr::from(([127, 0, 0, 1], 0)),
            usuarios: vec![],
            permitir: vec![],
            puertos: vec![443],
            max: 32,
            ip_ok: ip_publica,
            resolver,
        };
        let mut escucha = None;
        for (n, l) in texto.lines().enumerate() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mal = || format!("línea {}: no se entiende", n + 1);
            let (k, v) = l.split_once('=').ok_or_else(mal)?;
            let v = v.trim();
            match k.trim() {
                "escucha" => escucha = Some(v.parse().map_err(|_| mal())?),
                "usuario" => match v.split_once(':') {
                    Some((u, p)) if !u.is_empty() && p.len() >= 16 => c.usuarios.push(v.into()),
                    _ => return Err(format!("línea {}: usuario:contraseña, con 16 caracteres o más", n + 1)),
                },
                "permitir" => {
                    let d = v.trim_start_matches('.');
                    // se exige un punto interno para no permitir por error todo un ".com"
                    if !d.contains('.') || !d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') || d.parse::<IpAddr>().is_ok() {
                        return Err(format!("línea {}: dominio no válido", n + 1));
                    }
                    c.permitir.push(v.to_ascii_lowercase());
                }
                "puertos" => c.puertos = v.split(',').map(|p| p.trim().parse().map_err(|_| mal())).collect::<Result<_, _>>()?,
                "max" => c.max = v.parse().ok().filter(|m| (1..=64).contains(m)).ok_or_else(mal)?,
                _ => return Err(mal()),
            }
        }
        c.escucha = escucha.ok_or("falta «escucha»")?;
        if c.escucha.ip().is_unspecified() {
            return Err("«escucha» debe ser la IP del túnel, no 0.0.0.0".into());
        }
        if c.usuarios.is_empty() || c.permitir.is_empty() || c.puertos.is_empty() {
            return Err("faltan usuario, permitir o puertos".into());
        }
        Ok(c)
    }

    fn destino_ok(&self, host: &str, puerto: u16) -> bool {
        let h = host.trim_end_matches('.').to_ascii_lowercase();
        self.puertos.contains(&puerto)
            && self.permitir.iter().any(|p| match p.strip_prefix('.') {
                Some(d) => h == d || h.strip_suffix(d).is_some_and(|r| r.len() > 1 && r.ends_with('.')),
                None => h == *p,
            })
    }

    fn clave_ok(&self, cabecera: Option<&str>) -> bool {
        let Some((tipo, b64)) = cabecera.and_then(|h| h.trim().split_once(' ')) else { return false };
        let Some(dada) = base64(b64.trim()) else { return false };
        // se comparan todas, sin cortar antes, para no filtrar nada por el tiempo
        let igual = self.usuarios.iter().fold(false, |a, u| a | iguales(u.as_bytes(), &dada));
        tipo.eq_ignore_ascii_case("basic") && igual
    }
}

fn iguales(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |x, (p, q)| x | (p ^ q)) == 0
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let (mut acum, mut bits, mut o) = (0u32, 0, Vec::with_capacity(s.len() * 3 / 4));
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acum = (acum << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            o.push((acum >> bits) as u8);
            acum &= (1 << bits) - 1;
        }
    }
    Some(o)
}

/// Solo direcciones de internet. Todo lo privado, local, de CGNAT, multicast o reservado se rechaza.
fn ip_publica(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v4_publica(v),
        IpAddr::V6(v) => v6_publica(v),
    }
}

fn v4_publica(v: &Ipv4Addr) -> bool {
    let o = v.octets();
    !(o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || (o[0] == 100 && (64..128).contains(&o[1]))
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..32).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || o[0] >= 224)
}

fn v6_publica(v: &Ipv6Addr) -> bool {
    let s = v.segments();
    // 2000::/3, sin 6to4 (2002::/16), Teredo y especiales (2001:0::/23) ni documentación:
    // podrían esconder una IPv4 privada. Las ::ffff:a.b.c.d quedan fuera de 2000::/3.
    s[0] & 0xe000 == 0x2000 && s[0] != 0x2002 && !(s[0] == 0x2001 && s[1] < 0x0200) && !(s[0] == 0x2001 && s[1] == 0x0db8)
}

fn resolver(host: &str, puerto: u16) -> Vec<SocketAddr> {
    (host, puerto).to_socket_addrs().map(|i| i.collect()).unwrap_or_default()
}

/// "CONNECT host:puerto HTTP/1.1" + cabeceras → (host, puerto, Proxy-Authorization)
fn peticion(cab: &str) -> Option<(String, u16, Option<String>)> {
    let mut l = cab.split("\r\n");
    let mut p = l.next()?.split(' ');
    let (m, dest, v) = (p.next()?, p.next()?, p.next()?);
    if m != "CONNECT" || !v.starts_with("HTTP/1.") || p.next().is_some() {
        return None;
    }
    let (host, puerto) = dest.rsplit_once(':')?;
    let auth = l.find_map(|x| {
        let (k, v) = x.split_once(':')?;
        k.eq_ignore_ascii_case("proxy-authorization").then(|| v.trim().to_string())
    });
    Some((host.to_string(), puerto.parse().ok()?, auth))
}

/// Lee hasta la línea en blanco, con un límite de tamaño y de tiempo TOTAL.
fn leer_cabecera(s: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let fin = Instant::now() + T_CABECERA;
    let mut b: Vec<u8> = Vec::new();
    let mut t = [0u8; 1024];
    loop {
        let resta = fin.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
        s.set_read_timeout(Some(resta)).ok()?;
        let n = s.read(&mut t).ok().filter(|n| *n > 0)?;
        b.extend_from_slice(&t[..n]);
        if let Some(i) = b.windows(4).position(|w| w == b"\r\n\r\n") {
            let sobra = b.split_off(i + 4);
            return Some((String::from_utf8(b).ok()?, sobra));
        }
        if b.len() > MAX_CABECERA {
            return None;
        }
    }
}

fn copiar(mut de: TcpStream, mut a: TcpStream) {
    let mut t = [0u8; 16 * 1024];
    loop {
        match de.read(&mut t) {
            Ok(0) => break,
            Ok(n) if a.write_all(&t[..n]).is_ok() => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // inactividad o corte: se cierran las dos puntas para soltar también al otro hilo
            _ => {
                let _ = de.shutdown(Shutdown::Both);
                let _ = a.shutdown(Shutdown::Both);
                return;
            }
        }
    }
    let _ = a.shutdown(Shutdown::Write);
}

fn responder(s: &mut TcpStream, linea: &str) {
    let extra = if linea.starts_with("407") { "Proxy-Authenticate: Basic realm=\"stick\"\r\n" } else { "" };
    let _ = s.write_all(format!("HTTP/1.1 {linea}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
}

fn atender(mut s: TcpStream, c: &Conf) -> Option<()> {
    s.set_write_timeout(Some(T_INACTIVO)).ok()?;
    let (cab, sobra) = leer_cabecera(&mut s)?;
    let Some((host, puerto, auth)) = peticion(&cab) else {
        responder(&mut s, "405 Method Not Allowed");
        return None;
    };
    if !c.clave_ok(auth.as_deref()) {
        responder(&mut s, "407 Proxy Authentication Required");
        return None;
    }
    if !c.destino_ok(&host, puerto) {
        log(&format!("proxy: destino no permitido (puerto {puerto})"));
        responder(&mut s, "403 Forbidden");
        return None;
    }
    let ips: Vec<SocketAddr> = (c.resolver)(&host, puerto);
    let buenas: Vec<&SocketAddr> = ips.iter().filter(|a| (c.ip_ok)(&a.ip())).collect();
    if !ips.is_empty() && buenas.is_empty() {
        log("proxy: un dominio permitido apunta a una IP privada; rechazado");
        responder(&mut s, "403 Forbidden");
        return None;
    }
    let Some(mut r) = buenas.iter().take(3).find_map(|a| TcpStream::connect_timeout(a, T_CONECTAR).ok()) else {
        responder(&mut s, "502 Bad Gateway");
        return None;
    };
    for x in [&s, &r] {
        x.set_read_timeout(Some(T_INACTIVO)).ok()?;
        x.set_nodelay(true).ok()?;
    }
    r.set_write_timeout(Some(T_INACTIVO)).ok()?;
    s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").ok()?;
    r.write_all(&sobra).ok()?;
    let (s2, r2) = (s.try_clone().ok()?, r.try_clone().ok()?);
    match std::thread::Builder::new().stack_size(PILA).spawn(move || copiar(r2, s2)) {
        Ok(h) => {
            copiar(s, r);
            let _ = h.join();
        }
        Err(_) => {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
    Some(())
}

/// Cuenta de conexiones vivas; se descuenta sola pase lo que pase.
struct Turno(Arc<AtomicUsize>);
impl Drop for Turno {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn servir(l: TcpListener, c: Arc<Conf>) {
    let vivas = Arc::new(AtomicUsize::new(0));
    for s in l.incoming() {
        let Ok(mut s) = s else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let turno = Turno(vivas.clone());
        if vivas.fetch_add(1, Ordering::SeqCst) >= c.max {
            let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
            responder(&mut s, "503 Service Unavailable");
            continue;
        }
        let c = c.clone();
        let _ = std::thread::Builder::new().stack_size(PILA).spawn(move || {
            let _turno = turno;
            atender(s, &c);
        });
    }
}

/// Punto de entrada de `stickd proxy <archivo>`; devuelve el código de salida.
pub fn main(archivo: &str) -> i32 {
    if unsafe { libc::geteuid() } == 0 {
        log("proxy: no se corre como root (usa stickd-proxy.service)");
        return 2;
    }
    let c = match std::fs::read_to_string(archivo).map_err(|e| e.to_string()).and_then(|t| Conf::leer(&t)) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("proxy: {archivo}: {e}"));
            return 2;
        }
    };
    match TcpListener::bind(c.escucha) {
        Ok(l) => {
            log(&format!("proxy: {} dominios, puertos {:?}, hasta {} conexiones", c.permitir.len(), c.puertos, c.max));
            servir(l, Arc::new(c));
            0
        }
        Err(e) => {
            log(&format!("proxy: no se pudo escuchar en {}: {e}", c.escucha));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "escucha = 10.9.0.2:8888\nusuario = demo:0123456789abcdef\npermitir = .operador.com\npuertos = 443, 4475\n";
    const AUTH: &str = "Basic ZGVtbzowMTIzNDU2Nzg5YWJjZGVm";

    #[test]
    fn configuracion_falla_cerrada() {
        assert!(Conf::leer(BASE).is_ok());
        for quitar in ["escucha", "usuario", "permitir"] {
            let t: String = BASE.lines().filter(|l| !l.starts_with(quitar)).map(|l| format!("{l}\n")).collect();
            assert!(Conf::leer(&t).is_err(), "{quitar}");
        }
        for mal in ["permitir = .com", "permitir = 192.168.1.1", "usuario = a:corta", "max = 0", "otra = 1", "permitir = *"] {
            assert!(Conf::leer(&format!("{BASE}{mal}\n")).is_err(), "{mal}");
        }
        assert!(Conf::leer(&BASE.replace("10.9.0.2", "0.0.0.0")).is_err());
    }

    #[test]
    fn destinos() {
        let c = Conf::leer(&format!("{BASE}permitir = uno.exacto.mx\n")).unwrap();
        for (h, p) in [("operador.com", 443), ("www.Operador.com", 4475), ("a.b.operador.com.", 443), ("uno.exacto.mx", 443)] {
            assert!(c.destino_ok(h, p), "{h}");
        }
        for (h, p) in [
            ("www.operador.com", 80), ("malooperador.com", 443), (".operador.com", 443), ("operador.com.malo.net", 443),
            ("otro.uno.exacto.mx", 443), ("192.168.1.1", 443), ("[::1]", 443), ("localhost", 443), ("", 443),
        ] {
            assert!(!c.destino_ok(h, p), "{h}");
        }
    }

    #[test]
    fn ips_privadas() {
        for ip in [
            "10.8.0.1", "127.0.0.1", "192.168.100.1", "172.16.0.1", "172.31.255.1", "100.64.0.1", "100.127.0.1", "169.254.169.254",
            "0.0.0.0", "224.0.0.1", "255.255.255.255", "198.18.0.1", "::1", "fe80::1", "fd00::1", "::ffff:192.168.1.1", "2002:c0a8:101::1",
            "64:ff9b::a00:1", "2001:0:1::1", "2001:db8::1", "ff02::1",
        ] {
            assert!(!ip_publica(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.63.0.1", "100.128.0.1", "172.32.0.1", "192.169.0.1", "2606:4700::1111"] {
            assert!(ip_publica(&ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn claves() {
        let c = Conf::leer(BASE).unwrap();
        assert!(c.clave_ok(Some(AUTH)) && c.clave_ok(Some(&AUTH.replace("Basic", "basic"))));
        for mal in [None, Some(""), Some("Basic"), Some("Basic ZGVtbzptYWxh"), Some("Basic ***"), Some("Bearer ZGVtbzowMTIzNDU2Nzg5YWJjZGVm")] {
            assert!(!c.clave_ok(mal));
        }
    }

    fn levantar(ip_ok: fn(&IpAddr) -> bool, resolver: fn(&str, u16) -> Vec<SocketAddr>, max: usize) -> SocketAddr {
        let mut c = Conf::leer(BASE).unwrap();
        (c.ip_ok, c.resolver, c.max) = (ip_ok, resolver, max);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        std::thread::spawn(move || servir(l, Arc::new(c)));
        a
    }

    fn pedir(a: SocketAddr, txt: &str) -> (TcpStream, String) {
        let mut s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(txt.as_bytes()).unwrap();
        let mut b = [0u8; 512];
        let n = s.read(&mut b).unwrap();
        (s, String::from_utf8_lossy(&b[..n]).into_owned())
    }

    fn connect(dest: &str, auth: bool) -> String {
        format!("CONNECT {dest} HTTP/1.1\r\n{}\r\n", if auth { format!("Proxy-Authorization: {AUTH}\r\n") } else { String::new() })
    }

    static ECO: std::sync::OnceLock<SocketAddr> = std::sync::OnceLock::new();
    fn al_eco(_: &str, _: u16) -> Vec<SocketAddr> {
        vec![*ECO.get_or_init(|| {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let a = l.local_addr().unwrap();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    std::thread::spawn(move || {
                        let (mut r, mut w) = (s.try_clone().unwrap(), s);
                        let _ = std::io::copy(&mut r, &mut w);
                    });
                }
            });
            a
        })]
    }

    #[test]
    fn tunel_completo_y_rechazos() {
        let a = levantar(|_| true, al_eco, 8);
        let (mut s, r) = pedir(a, &connect("www.operador.com:443", true));
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        s.write_all(b"hola").unwrap();
        s.shutdown(Shutdown::Write).unwrap();
        let mut o = String::new();
        s.read_to_string(&mut o).unwrap();
        assert_eq!(o, "hola");
        assert!(pedir(a, &connect("www.operador.com:443", false)).1.starts_with("HTTP/1.1 407"));
        assert!(pedir(a, &connect("otro.com:443", true)).1.starts_with("HTTP/1.1 403"));
        assert!(pedir(a, &connect("www.operador.com:22", true)).1.starts_with("HTTP/1.1 403"));
        assert!(pedir(a, "GET http://www.operador.com/ HTTP/1.1\r\n\r\n").1.starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn dominio_permitido_que_apunta_a_red_privada() {
        let a = levantar(ip_publica, al_eco, 8);
        assert!(pedir(a, &connect("www.operador.com:443", true)).1.starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn tope_de_conexiones() {
        let a = levantar(|_| true, al_eco, 1);
        let (_viva, r) = pedir(a, &connect("www.operador.com:443", true));
        assert!(r.starts_with("HTTP/1.1 200"));
        let mut s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 64];
        let n = s.read(&mut b).unwrap();
        assert!(String::from_utf8_lossy(&b[..n]).starts_with("HTTP/1.1 503"));
    }
}
