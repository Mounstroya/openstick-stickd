//! Plugins: módulos externos en archivos .so que implementan la ABI C de stickd (ABI v1,
//! ver plugins/abi.h). Pueden escribirse en cualquier lenguaje que compile a shared object
//! (C, Go con -buildmode=c-shared, Zig…).
//!
//! - Al arrancar se cargan todos los .so de /var/lib/stickd/plugins (`cargar_todas`).
//! - El panel agrega más en caliente: subida (POST /plugins/subir) o copiar el archivo a la
//!   carpeta (también por scp) y «Recargar» (POST /plugins/recargar). El stick NO se reinicia.
//! - Se quitan (POST /plugins/quitar): se borra el archivo y el módulo desaparece de la lista;
//!   si era la vitrina al aire, se cae antes (lo hace portal.rs).
//!
//! Los datos persistentes de un plugin viven en /var/lib/stickd/<id>/ (misma convención que
//! los módulos internos); el plugin los gestiona solo.
//!
//! Reglas de la ABI v1:
//! - los strings que devuelve el plugin se liberan con stickd_free_string /
//!   stickd_free_respuesta (el loader siempre llama a la liberación correspondiente);
//! - el cuerpo de la petición llega como buffer crudo (puede tener NUL);
//! - guest() se llama siempre desde un solo hilo (el servidor de invitados);
//!   admin_html/admin_post/resumen/start/stop pueden llegar desde hilos distintos.

use crate::vitrina::{data_dir, GuestReq, GuestResp, Vitrina};
use crate::Vitrinas;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::Arc;

const ABI: u32 = 2;
const DIR: &str = "/var/lib/stickd/plugins";
const MAX_SO: u64 = 8 * 1024 * 1024;

/// Carpeta de plugins. Sobrescribible con STICKD_PLUGIN_DIR (solo para tests).
fn dir() -> std::path::PathBuf {
    std::env::var("STICKD_PLUGIN_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::PathBuf::from(DIR))
}

#[repr(C)]
struct Info {
    abi_version: u32,
    id: *const c_char,
    nombre: *const c_char,
    descripcion: *const c_char,
    usa_ap: u8,
    max_body: u32,
}

#[repr(C)]
struct Respuesta {
    estado: u16,
    cabeceras: *const c_char,
    cuerpo: *const u8,
    cuerpo_len: usize,
}

type FInit = unsafe extern "C" fn(u32, *const Host) -> bool;
type FInfo = unsafe extern "C" fn() -> *const Info;
type FGuest = unsafe extern "C" fn(usize, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char, *const c_char, *const u8, usize) -> *mut Respuesta;
type FFreeResp = unsafe extern "C" fn(*mut Respuesta);
type FHtml = unsafe extern "C" fn(usize) -> *mut c_char;
type FPost = unsafe extern "C" fn(usize, *const c_char, *const c_char, *const u8, usize) -> *mut c_char;
type FFreeStr = unsafe extern "C" fn(*mut c_char);
type FVoid = unsafe extern "C" fn(usize);
type FVoid0 = unsafe extern "C" fn();

struct Symb {
    init: FInit,
    info: FInfo,
    guest: FGuest,
    free_resp: FFreeResp,
    admin_html: FHtml,
    admin_post: FPost,
    resumen: FHtml,
    free_str: FFreeStr,
    start: FVoid,
    stop: FVoid,
    shutdown: FVoid0,
}

struct Plugin {
    archivo: &'static str,
    id: &'static str,
    nombre: &'static str,
    descripcion: &'static str,
    usa_ap: bool,
    max_body: usize,
    handle: *mut c_void,
    s: Symb,
}

// El handle de dlopen y los punteros a función son válidos para todo el proceso.
unsafe impl Send for Plugin {}
unsafe impl Sync for Plugin {}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe {
            (self.s.shutdown)();
            libc::dlclose(self.handle);
        }
    }
}

impl Vitrina for Plugin {
    fn id(&self) -> &'static str {
        self.id
    }
    fn nombre(&self) -> &'static str {
        self.nombre
    }
    fn descripcion(&self) -> &'static str {
        self.descripcion
    }
    fn ssid_por_defecto(&self) -> String {
        format!("Stick {}", self.nombre)
    }
    fn usa_ap(&self) -> bool {
        self.usa_ap
    }
    fn max_body(&self) -> usize {
        self.max_body
    }
    fn es_plugin(&self) -> bool {
        true
    }
    fn archivo(&self) -> Option<&str> {
        Some(self.archivo)
    }

    fn guest(&self, req: &GuestReq) -> GuestResp {
        let cm = cstr(req.method);
        let cp = cstr(req.path);
        let cq = cstr(req.query);
        let ch = cstr(req.host);
        let cua = cstr(req.user_agent);
        let cct = cstr(req.content_type);
        let cip = cstr(&req.client_ip.to_string());
        let body_p = if req.body.is_empty() {
            std::ptr::null()
        } else {
            req.body.as_ptr()
        };
        let r = unsafe { (self.s.guest)(0, cm.as_ptr(), cp.as_ptr(), cq.as_ptr(), ch.as_ptr(), cua.as_ptr(), cct.as_ptr(), cip.as_ptr(), body_p, req.body.len()) };
        if r.is_null() {
            return GuestResp::not_found();
        }
        let mut resp = GuestResp { status: 200, content_type: "text/plain; charset=utf-8".into(), headers: Vec::new(), body: Vec::new() };
        unsafe {
            let raw = &*r;
            resp.status = if raw.estado == 0 { 200 } else { raw.estado };
            if raw.cuerpo_len > 0 {
                resp.body = std::slice::from_raw_parts(raw.cuerpo, raw.cuerpo_len).to_vec();
            }
            if !raw.cabeceras.is_null() {
                for linea in CStr::from_ptr(raw.cabeceras).to_string_lossy().lines() {
                    if let Some((k, v)) = linea.split_once(':') {
                        let (k, v) = (k.trim(), v.trim());
                        if k.is_empty() {
                            continue;
                        }
                        if k.eq_ignore_ascii_case("Content-Type") {
                            resp.content_type = v.to_string();
                        } else {
                            resp.headers.push((k.to_string(), v.to_string()));
                        }
                    }
                }
            }
            (self.s.free_resp)(r);
        }
        resp
    }

    fn admin_html(&self) -> String {
        let p = unsafe { (self.s.admin_html)(0) };
        if p.is_null() {
            return String::new();
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.s.free_str)(p) };
        s
    }

    fn admin_post(&self, accion: &str, f: &[(String, String)], body: &[u8]) -> String {
        let form = form_encode(f);
        let ca = cstr(accion);
        let cf = cstr(&form);
        let body_p = if body.is_empty() {
            std::ptr::null()
        } else {
            body.as_ptr()
        };
        let p = unsafe { (self.s.admin_post)(0, ca.as_ptr(), cf.as_ptr(), body_p, body.len()) };
        if p.is_null() {
            return "El plugin no devolvió respuesta.".into();
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.s.free_str)(p) };
        s
    }

    fn start(&self) {
        unsafe {
            (self.s.start)(0)
        }
    }
    fn stop(&self) {
        unsafe {
            (self.s.stop)(0)
        }
    }
    fn resumen(&self) -> String {
        let p = unsafe { (self.s.resumen)(0) };
        if p.is_null() {
            return String::new();
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.s.free_str)(p) };
        s
    }
}

// ---------- helpers ----------

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default()
}

unsafe fn cstr_to(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

unsafe fn sym<T: Copy>(h: *mut c_void, n: &str) -> Result<T, String> {
    let cn = cstr(n);
    let p = unsafe { libc::dlsym(h, cn.as_ptr()) };
    if p.is_null() {
        return Err(format!("falta el símbolo {n} (¿compilado contra plugins/abi.h?)"));
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

// ============================================================================
// API del host: funciones del stick que el plugin puede llamar (ABI v2).
// Se pasan en una tabla `Host` al `stickd_init`. Deben ser infalibles: con
// panic=abort un panic aquí tumbaría al daemon, así que nada de unwrap/índices.
// Los `char*` que devuelven se liberan con host->free_string.
// ============================================================================

#[repr(C)]
pub struct Host {
    pub abi_version: u32,
    pub log: extern "C" fn(*const c_char),
    pub free_string: extern "C" fn(*mut c_char),
    pub uplink_json: extern "C" fn() -> *mut c_char,
    pub wifi_scan_json: extern "C" fn() -> *mut c_char,
    pub http_get: extern "C" fn(*const c_char, *mut c_int) -> *mut c_char,
    pub sms_send: extern "C" fn(*const c_char, *const c_char) -> bool,
    pub sms_list_json: extern "C" fn() -> *mut c_char,
    pub led: extern "C" fn(*const c_char, *const c_char),
    pub guests_json: extern "C" fn() -> *mut c_char,
}

// El host es estático e inmutable; sus punteros son funciones del proceso.
unsafe impl Sync for Host {}

pub static HOST: Host = Host {
    abi_version: ABI,
    log: host_log,
    free_string: host_free_string,
    uplink_json: host_uplink_json,
    wifi_scan_json: host_wifi_scan_json,
    http_get: host_http_get,
    sms_send: host_sms_send,
    sms_list_json: host_sms_list_json,
    led: host_led,
    guests_json: host_guests_json,
};

/// String de Rust → char* que el plugin libera con host->free_string.
fn a_c(s: String) -> *mut c_char {
    match CString::new(s.replace('\0', "")) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}
/// char* del plugin → &str (None si NULL o no-UTF8).
unsafe fn de_c<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().ok()
    }
}

extern "C" fn host_log(msg: *const c_char) {
    if let Some(s) = unsafe { de_c(msg) } {
        crate::util::log(&format!("plugin: {}", s.chars().take(500).collect::<String>()));
    }
}

extern "C" fn host_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

extern "C" fn host_uplink_json() -> *mut c_char {
    use crate::util::json;
    let internet = crate::net::internet();
    let dev = crate::net::uplink_dev().unwrap_or_default();
    let salida = if dev.starts_with("usb") {
        "usb"
    } else if dev.starts_with("wlan") {
        "wifi"
    } else if dev.starts_with("wwan") || dev.starts_with("wwp") || dev.contains("mbim") || dev.contains("qmi") {
        "lte"
    } else if dev.is_empty() {
        "-"
    } else {
        "otro"
    };
    let ssid = crate::net::current_ssid();
    let lte = crate::net::lte();
    let s = format!(
        "{{\"internet\":{},\"salida\":{},\"dispositivo\":{},\"ssid\":{},\"lte\":{{\"sim\":{},\"estado\":{},\"senal\":{},\"tecnologia\":{},\"operador\":{}}},\"wireguard\":{}}}",
        internet,
        json(salida),
        json(&dev),
        json(&ssid),
        lte.sim,
        json(&lte.state),
        json(&lte.signal),
        json(&lte.tech),
        json(&lte.operator),
        json(&crate::net::wireguard()),
    );
    a_c(s)
}

extern "C" fn host_wifi_scan_json() -> *mut c_char {
    use crate::util::json;
    let items: Vec<String> = crate::net::scan()
        .into_iter()
        .take(60)
        .map(|(ssid, sig, sec)| format!("{{\"ssid\":{},\"senal\":{},\"seguridad\":{}}}", json(&ssid), sig, json(&sec)))
        .collect();
    a_c(format!("[{}]", items.join(",")))
}

extern "C" fn host_http_get(url: *const c_char, status: *mut c_int) -> *mut c_char {
    if !status.is_null() {
        unsafe { *status = 0 };
    }
    let Some(u) = (unsafe { de_c(url) }) else { return std::ptr::null_mut() };
    // solo http/https, nunca file:// ni otros esquemas
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return std::ptr::null_mut();
    }
    let out = match std::process::Command::new("curl")
        .args(["-sS", "-L", "--proto", "=http,https", "--proto-redir", "=http,https", "--max-filesize", "8M", "--max-time", "30", "--max-redirs", "5", "-w", "\n%{http_code}", "--url", u])
        .output()
    {
        Ok(o) => o,
        Err(_) => return std::ptr::null_mut(),
    };
    let texto = String::from_utf8_lossy(&out.stdout);
    // la última línea es el código (por el -w); el resto es el cuerpo
    let (cuerpo, codigo) = match texto.rfind('\n') {
        Some(i) => (&texto[..i], texto[i + 1..].trim().parse::<c_int>().unwrap_or(0)),
        None => (texto.as_ref(), 0),
    };
    if !status.is_null() {
        unsafe { *status = codigo };
    }
    a_c(cuerpo.to_string())
}

extern "C" fn host_sms_send(numero: *const c_char, texto: *const c_char) -> bool {
    let (Some(num), Some(txt)) = (unsafe { de_c(numero) }, unsafe { de_c(texto) }) else { return false };
    // número: solo dígitos, + y espacios; evita inyección de opciones
    if num.is_empty() || num.len() > 20 || !num.chars().all(|c| c.is_ascii_digit() || c == '+' || c == ' ') {
        return false;
    }
    let txt: String = txt.chars().take(1000).filter(|c| *c != '\n' && *c != '\r').collect();
    let crear = format!("number={num},text='{}'", txt.replace('\'', ""));
    let (ok, out) = crate::util::run("mmcli", &["-m", "any", &format!("--messaging-create-sms={crear}")]);
    if !ok {
        return false;
    }
    // la salida trae la ruta del SMS: /org/.../SMS/N
    let Some(ruta) = out.split_whitespace().find(|w| w.contains("/SMS/")) else { return false };
    let ruta = ruta.trim_end_matches(['\'', '"', '.']);
    let enviado = crate::util::ok("mmcli", &["-s", ruta, "--send"]);
    crate::util::log(&format!("plugin: SMS a {num} ({} car.): {}", txt.len(), if enviado { "enviado" } else { "falló el envío" }));
    enviado
}

extern "C" fn host_sms_list_json() -> *mut c_char {
    use crate::util::json;
    let (_, lista) = crate::util::run("mmcli", &["-m", "any", "--messaging-list-sms"]);
    let rutas: Vec<&str> = lista.split_whitespace().filter(|w| w.contains("/SMS/")).take(50).collect();
    let mut items = Vec::new();
    for ruta in rutas {
        let (_, kv) = crate::util::run("mmcli", &["-s", ruta, "--output-keyvalue"]);
        let get = |k: &str| {
            kv.lines()
                .find(|l| l.trim_start().starts_with(k))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        items.push(format!(
            "{{\"numero\":{},\"texto\":{},\"hora\":{},\"estado\":{}}}",
            json(&get("sms.content.number")),
            json(&get("sms.content.text")),
            json(&get("sms.properties.timestamp")),
            json(&get("sms.properties.state")),
        ));
    }
    a_c(format!("[{}]", items.join(",")))
}

extern "C" fn host_led(cual: *const c_char, patron: *const c_char) {
    let nombre = match unsafe { de_c(cual) } {
        Some("rojo") | Some("red") => "red:power",
        Some("verde") | Some("green") => "green:wlan",
        Some("azul") | Some("blue") => "blue:wan",
        _ => return,
    };
    let base = format!("/sys/class/leds/{nombre}");
    match unsafe { de_c(patron) } {
        Some("on") => {
            let _ = std::fs::write(format!("{base}/trigger"), "none");
            let _ = std::fs::write(format!("{base}/brightness"), "255");
        }
        Some("off") => {
            let _ = std::fs::write(format!("{base}/trigger"), "none");
            let _ = std::fs::write(format!("{base}/brightness"), "0");
        }
        Some(t @ ("timer" | "heartbeat")) => {
            let _ = std::fs::write(format!("{base}/trigger"), t);
        }
        _ => {}
    }
}

extern "C" fn host_guests_json() -> *mut c_char {
    use crate::util::json;
    // vecinos en la interfaz del AP, filtrados a la subred de la vitrina (192.168.4.x)
    let (_, out) = crate::util::run("ip", &["-4", "neigh", "show", "dev", "wlan0"]);
    let mut items = Vec::new();
    for l in out.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let Some(ip) = w.first() else { continue };
        if !ip.starts_with("192.168.4.") {
            continue;
        }
        let mac = w.iter().position(|x| *x == "lladdr").and_then(|i| w.get(i + 1)).copied().unwrap_or("");
        let con_internet = ip.parse().map(crate::invitados::tiene_internet).unwrap_or(false);
        items.push(format!("{{\"ip\":{},\"mac\":{},\"internet\":{}}}", json(ip), json(mac), con_internet));
    }
    a_c(format!("[{}]", items.join(",")))
}

fn leer_simbolos(h: *mut c_void) -> Result<Symb, String> {
    Ok(Symb {
        init: unsafe { sym(h, "stickd_init") }?,
        info: unsafe { sym(h, "stickd_get_info") }?,
        guest: unsafe { sym(h, "stickd_guest") }?,
        free_resp: unsafe { sym(h, "stickd_free_respuesta") }?,
        admin_html: unsafe { sym(h, "stickd_admin_html") }?,
        admin_post: unsafe { sym(h, "stickd_admin_post") }?,
        resumen: unsafe { sym(h, "stickd_resumen") }?,
        free_str: unsafe { sym(h, "stickd_free_string") }?,
        start: unsafe { sym(h, "stickd_start") }?,
        stop: unsafe { sym(h, "stickd_stop") }?,
        shutdown: unsafe { sym(h, "stickd_shutdown") }?,
    })
}

/// Codifica el formulario como x-www-form-urlencoded (la ABI no tiene structs de listas).
fn form_encode(f: &[(String, String)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(b as char),
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    f.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

fn nombre_valido(n: &str) -> bool {
    if n.len() < 4 || n.len() > 120 || !n.ends_with(".so") {
        return false;
    }
    let base = &n[..n.len() - 3];
    !base.is_empty() && base.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn listar_so() -> Vec<std::path::PathBuf> {
    let mut v = match std::fs::read_dir(dir()) {
        Ok(rd) => rd.filter_map(|e| e.ok()).filter_map(|e| e.path().extension().filter(|x| *x == "so").map(|_| e.path())).collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

// ---------- carga ----------

/// Abre un .so concreto, valida la ABI y lo registra en `vit`. Devuelve el nombre a mostrar.
fn cargar_path(vit: &Vitrinas, path: &Path) -> Result<String, String> {
    let cpath = cstr(path.to_str().ok_or("ruta no-UTF-8")?);
    let h = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        let e = unsafe { libc::dlerror() };
        let msg = if e.is_null() { "desconocido".to_string() } else { unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned() };
        return Err(format!("no se pudo abrir el .so (¿architectura aarch64?): {msg}"));
    }
    let s = match leer_simbolos(h) {
        Ok(s) => s,
        Err(e) => {
            unsafe { libc::dlclose(h) };
            return Err(e);
        }
    };
    let ip = unsafe { (s.info)() };
    if ip.is_null() {
        unsafe { libc::dlclose(h) };
        return Err("stickd_info() devolvió null".into());
    }
    let info = unsafe { &*ip };
    if info.abi_version != ABI {
        unsafe { libc::dlclose(h) };
        return Err(format!("ABI no soportada: el plugin declara v{} y el stick soporta v{ABI}", info.abi_version));
    }
    let id = unsafe { cstr_to(info.id) }
        .filter(|x| !x.is_empty() && x.len() <= 64 && x.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .ok_or("el id del plugin no es válido (minúsculas, dígitos y guiones, ≤ 64)")?;
    let nombre = unsafe { cstr_to(info.nombre) }.filter(|x| !x.is_empty() && x.len() <= 64).unwrap_or_else(|| id.clone());
    let desc = unsafe { cstr_to(info.descripcion) }.filter(|x| !x.is_empty() && x.len() <= 200).unwrap_or_default();
    {
        let g = vit.lock().unwrap();
        if g.iter().any(|v| v.id() == id) {
            unsafe { libc::dlclose(h) };
            return Err(format!("ya hay un módulo con el id «{id}»"));
        }
    }
    if !unsafe { (s.init)(ABI, &HOST) } {
        unsafe { libc::dlclose(h) };
        return Err("el plugin rechazó la inicialización (stickd_init devolvió false)".into());
    }
    let archivo = path.file_name().and_then(|x| x.to_str()).unwrap_or("plugin.so");
    let p = Plugin {
        archivo: Box::leak(archivo.to_string().into_boxed_str()),
        id: Box::leak(id.into_boxed_str()),
        nombre: Box::leak(nombre.into_boxed_str()),
        descripcion: Box::leak(desc.into_boxed_str()),
        usa_ap: info.usa_ap != 0,
        max_body: if info.max_body == 0 { 16 * 1024 } else { info.max_body as usize }.min(25 * 1024 * 1024),
        handle: h,
        s,
    };
    let _ = data_dir(p.id);
    let id_static = p.id;
    let nombre_out = p.nombre.to_string();
    vit.lock().unwrap().push(Arc::new(p) as Arc<dyn Vitrina>);
    crate::util::log(&format!("plugin: cargado «{nombre_out}» (id {id_static}, archivo {archivo})"));
    Ok(nombre_out)
}

/// Al arrancar: carga todos los .so que haya en la carpeta de plugins.
pub fn cargar_todas(vit: &Vitrinas) {
    let _ = std::fs::create_dir_all(dir());
    for p in listar_so() {
        let nombre = p.file_name().and_then(|x| x.to_str()).unwrap_or("?");
        if let Err(e) = cargar_path(vit, &p) {
            crate::util::log(&format!("plugin: no se pudo cargar {nombre}: {e}"));
        }
    }
}

/// Desde el panel: carga los .so de la carpeta que aún no estén cargados (puestos por scp, p.ej.).
pub fn cargar_nuevos(vit: &Vitrinas) -> String {
    let _ = std::fs::create_dir_all(dir());
    let cargados: Vec<String> = {
        let g = vit.lock().unwrap();
        g.iter().filter(|v| v.es_plugin()).filter_map(|v| v.archivo()).map(|a| a.to_string()).collect()
    };
    let mut n = 0usize;
    for p in listar_so() {
        let Some(nombre) = p.file_name().and_then(|x| x.to_str()).map(str::to_string) else {
            continue;
        };
        if cargados.iter().any(|c| c == &nombre) {
            continue;
        }
        if let Err(e) = cargar_path(vit, &p) {
            crate::util::log(&format!("plugin: no se pudo cargar {nombre}: {e}"));
        } else {
            n += 1;
        }
    }
    if n == 0 {
        "No hay plugins nuevos en la carpeta".into()
    } else {
        format!("Cargado(s) {n} plugin(s) nuevo(s): ya aparecen en la lista de vitrinas")
    }
}

/// Desde el panel: valida y guarda el .so subido (cuerpo crudo) y lo carga en caliente.
pub fn subir(vit: &Vitrinas, nombre: &str, bytes: &[u8]) -> Result<String, String> {
    let nombre = nombre.trim();
    if !nombre_valido(nombre) {
        return Err("nombre no válido: usa letras, dígitos, guiones bajos o - y termina en .so".into());
    }
    if bytes.is_empty() || bytes.len() as u64 > MAX_SO {
        return Err(format!("el archivo debe pesar entre 1 byte y {MAX_SO} bytes"));
    }
    if !bytes.starts_with(b"\x7fELF") {
        return Err("no parece un ejecutable ELF (¿un .so de Linux aarch64?)".into());
    }
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    let destino = dir().join(nombre);
    let tmp = dir().join(format!(".{nombre}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    if std::fs::rename(&tmp, &destino).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err("no se pudo guardar el archivo en el stick".into());
    }
    match cargar_path(vit, &destino) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = std::fs::remove_file(&destino);
            Err(e)
        }
    }
}

/// Desde el panel: saca el módulo del vector y borra el archivo.
pub fn quitar(vit: &Vitrinas, nombre: &str) -> Result<String, String> {
    if !nombre_valido(nombre) {
        return Err("nombre no válido".into());
    }
    {
        let mut g = vit.lock().unwrap();
        let Some(pos) = g.iter().position(|v| v.archivo().as_deref() == Some(nombre)) else {
            return Err("no hay ningún plugin cargado con ese archivo".into());
        };
        g.remove(pos);
    }
    let _ = std::fs::remove_file(dir().join(nombre));
    crate::util::log(&format!("plugin: quitado {nombre}"));
    Ok(nombre.to_string())
}

/// Id del módulo cuyo archivo es `nombre` (para caerlo antes de quitarlo, lo hace portal.rs).
pub fn id_de(vit: &Vitrinas, nombre: &str) -> Option<String> {
    let g = vit.lock().unwrap();
    g.iter().find(|v| v.archivo().as_deref() == Some(nombre)).map(|v| v.id().to_string())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Mutex;

    /// Compila plugin-ejemplo/hola.c con cc y lo carga como lo haría el stick:
    /// recargar → guest → admin → resumen → quitar. Se omite si no hay cc.
    #[test]
    fn carga_el_plugin_de_ejemplo() {
        let tmp = std::env::temp_dir().join(format!("stickd-plugin-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe { std::env::set_var("STICKD_PLUGIN_DIR", &tmp) };

        let src = format!("{}/plugin-ejemplo/hola.c", env!("CARGO_MANIFEST_DIR"));
        let out = tmp.join("hola.so");
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let st = std::process::Command::new(&cc)
            .args(["-shared", "-fPIC", "-O2", "-o"])
            .arg(&out)
            .arg(&src)
            .output();
        let Ok(st) = st else {
            eprintln!("plugin-test: no hay cc, se omite");
            return;
        };
        if !st.status.success() {
            panic!("cc falló: {}", String::from_utf8_lossy(&st.stderr));
        }

        // /var/lib/stickd/hola lo crea data_dir() al cargar; se limpia al final si no existía
        let datos = std::path::Path::new("/var/lib/stickd/hola");
        let datos_nuevo = !datos.exists();
        let _ = std::fs::create_dir_all(datos);

        let vit: Vitrinas = Arc::new(Mutex::new(Vec::new()));
        let msg = cargar_nuevos(&vit);
        assert!(msg.contains("Cargado(s) 1"), "mensaje: {msg}");

        let v = {
            let g = vit.lock().unwrap();
            assert_eq!(g.len(), 1);
            g[0].clone()
        };
        assert_eq!(v.id(), "hola");
        assert!(v.es_plugin());
        assert_eq!(v.archivo(), Some("hola.so"));

        let r = v.guest(&GuestReq {
            method: "GET",
            path: "/",
            query: "",
            host: "192.168.4.1",
            user_agent: "prueba",
            content_type: "",
            body: b"",
            client_ip: std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 4, 23)),
        });
        assert_eq!(r.status, 200);
        let cuerpo = String::from_utf8_lossy(&r.body);
        assert!(cuerpo.contains("Visita n.º"), "cuerpo: {cuerpo}");

        let r404 = v.guest(&GuestReq {
            method: "GET",
            path: "/otra",
            query: "",
            host: "h",
            user_agent: "prueba",
            content_type: "",
            body: b"",
            client_ip: std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 4, 23)),
        });
        assert_eq!(r404.status, 404);

        assert!(v.admin_html().contains("hola.c"));
        assert!(v.resumen().contains("visitas"));
        assert!(v.admin_post("saludo", &[], b"").contains("sin reiniciar"));
        assert!(v.admin_post("otra", &[], b"").contains("no válida"));

        assert_eq!(id_de(&vit, "hola.so").as_deref(), Some("hola"));
        assert!(quitar(&vit, "hola.so").is_ok());
        assert!(vit.lock().unwrap().is_empty());
        assert!(!out.exists(), "el archivo debió borrarse");

        if datos_nuevo {
            let _ = std::fs::remove_dir_all(datos);
        }
        unsafe { std::env::remove_var("STICKD_PLUGIN_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
