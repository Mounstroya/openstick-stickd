//! "Vitrinas": modos en los que el stick crea su PROPIO WiFi y atiende a quien se conecta
//! (portal cautivo). El núcleo (este archivo + main.rs + portal.rs + captive.rs) se encarga de:
//!   - levantar el AP con el SSID configurado (conexión NM "vitrina", abierta, 192.168.4.1/24)
//!   - secuestrar DNS (todo resuelve a 192.168.4.1) y HTTP del WiFi hacia el servidor de invitados
//!   - responder las URLs de detección de portal cautivo (Android/iOS/Windows) para que el
//!     teléfono abra la página solo
//!   - enrutar cada petición de invitado al módulo activo (`guest`)
//!   - mostrar la sección de cada módulo en el panel de administración (`admin_html` / `admin_post`)
//!
//! Cada módulo vive en su propio archivo y SOLO implementa el trait `Vitrina`.
//! Datos persistentes de cada módulo: `data_dir(id)` = /var/lib/stickd/<id>/ (ya creada).

use std::net::IpAddr;
use std::path::PathBuf;

pub mod afluencia;
pub mod buzon;
pub mod chat;
pub mod pasalo;
pub mod portafolio;
pub mod tarjeta;

/// Petición de alguien conectado al WiFi de la vitrina (servidor de invitados, puerto 8080,
/// al que se redirige todo el tráfico HTTP del AP).
pub struct GuestReq<'a> {
    pub method: &'a str,
    /// Ruta sin query, p.ej. "/" o "/contacto.vcf"
    pub path: &'a str,
    /// Query cruda sin '?', p.ej. "a=1&b=2"
    pub query: &'a str,
    /// Header Host original (puede ser cualquier dominio por el secuestro DNS)
    pub host: &'a str,
    pub user_agent: &'a str,
    pub content_type: &'a str,
    /// Cuerpo (máx. `max_body()` bytes del módulo)
    pub body: &'a [u8],
    pub client_ip: IpAddr,
}

pub struct GuestResp {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl GuestResp {
    pub fn html(s: impl Into<String>) -> Self {
        GuestResp { status: 200, content_type: "text/html; charset=utf-8".into(), headers: vec![], body: s.into().into_bytes() }
    }
    pub fn bytes(ct: &str, b: Vec<u8>) -> Self {
        GuestResp { status: 200, content_type: ct.into(), headers: vec![], body: b }
    }
    pub fn redirect(to: &str) -> Self {
        GuestResp { status: 302, content_type: "text/plain".into(), headers: vec![("Location".into(), to.into())], body: vec![] }
    }
    pub fn not_found() -> Self {
        GuestResp { status: 404, content_type: "text/plain; charset=utf-8".into(), headers: vec![], body: b"no encontrado".to_vec() }
    }
}

pub trait Vitrina: Send + Sync {
    /// Identificador corto y estable: "portafolio", "tarjeta", "buzon", "afluencia"
    fn id(&self) -> &'static str;
    /// Nombre para mostrar en el panel
    fn nombre(&self) -> &'static str;
    /// Descripción de una línea para el selector de modo
    fn descripcion(&self) -> &'static str;
    /// SSID sugerido si el usuario no configuró uno
    fn ssid_por_defecto(&self) -> String;
    /// ¿Necesita el AP + portal cautivo? (afluencia NO: usa el radio en modo monitor)
    fn usa_ap(&self) -> bool {
        true
    }
    /// Tamaño máximo de cuerpo aceptado en peticiones de invitados
    fn max_body(&self) -> usize {
        16 * 1024
    }
    /// Atiende a un invitado. Se llama para TODA petición HTTP del AP excepto las URLs de
    /// detección de portal cautivo, que el núcleo ya maneja (redirigen a "/").
    fn guest(&self, req: &GuestReq) -> GuestResp;
    /// HTML (fragmento, sin <html>) de la sección de este modo en el panel de admin.
    /// Los formularios deben enviar a "/vitrina/<id>/<accion>" (POST).
    fn admin_html(&self) -> String;
    /// Maneja POST /vitrina/<id>/<accion>. `body` es el cuerpo crudo (hasta 25 MB, para
    /// subir archivos con fetch(...,{body:file})); `form` es el cuerpo decodificado si era
    /// x-www-form-urlencoded, o la query string si no. Devuelve un mensaje para el usuario.
    fn admin_post(&self, accion: &str, form: &[(String, String)], body: &[u8]) -> String;
    /// GET /vitrina/<id>/<ruta> en el panel de ADMIN (p.ej. descargar un archivo del buzón,
    /// ver una imagen subida). `None` = 404.
    fn admin_get(&self, _ruta: &str) -> Option<GuestResp> {
        None
    }
    /// Se llama al activar / desactivar el modo (afluencia arranca/para la captura aquí).
    fn start(&self) {}
    fn stop(&self) {}
    /// ¿Mostrar su sección del panel ya abierta? (p.ej. hay una descarga en curso)
    fn abierta(&self) -> bool {
        false
    }
    /// Una línea de estado para el panel (p.ej. "12 mensajes", "34 visitas hoy")
    fn resumen(&self) -> String {
        String::new()
    }
}

pub fn data_dir(id: &str) -> PathBuf {
    let p = PathBuf::from(format!("/var/lib/stickd/{id}"));
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Todos los módulos registrados, en el orden en que aparecen en el panel.
pub fn todas() -> Vec<Box<dyn Vitrina>> {
    vec![
        Box::new(portafolio::Portafolio::new()),
        Box::new(tarjeta::Tarjeta::new()),
        Box::new(buzon::Buzon::new()),
        Box::new(chat::Chat::new()),
        Box::new(pasalo::Pasalo::new()),
        Box::new(afluencia::Afluencia::new()),
    ]
}
