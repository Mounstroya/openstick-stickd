//! Módulo "Tarjeta de presentacion" — ESQUELETO, lo implementa su agente.
use super::{GuestReq, GuestResp, Vitrina};

pub struct Tarjeta;

impl Tarjeta {
    pub fn new() -> Self {
        Tarjeta
    }
}

impl Vitrina for Tarjeta {
    fn id(&self) -> &'static str {
        "tarjeta"
    }
    fn nombre(&self) -> &'static str {
        "Tarjeta de presentacion"
    }
    fn descripcion(&self) -> &'static str {
        "Guardan tu contacto con un toque"
    }
    fn ssid_por_defecto(&self) -> String {
        "Tarjeta de presentacion".into()
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::html("<h1>Tarjeta de presentacion</h1><p>En construccion</p>")
    }
    fn admin_html(&self) -> String {
        "<p>En construccion</p>".into()
    }
    fn admin_post(&self, _accion: &str, _form: &[(String, String)], _body: &[u8]) -> String {
        "En construccion".into()
    }
}
