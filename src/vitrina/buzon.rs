//! Módulo "Buzon offline" — ESQUELETO, lo implementa su agente.
use super::{GuestReq, GuestResp, Vitrina};

pub struct Buzon;

impl Buzon {
    pub fn new() -> Self {
        Buzon
    }
}

impl Vitrina for Buzon {
    fn id(&self) -> &'static str {
        "buzon"
    }
    fn nombre(&self) -> &'static str {
        "Buzon offline"
    }
    fn descripcion(&self) -> &'static str {
        "Dejan mensajes y archivos sin internet"
    }
    fn ssid_por_defecto(&self) -> String {
        "Buzon offline".into()
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::html("<h1>Buzon offline</h1><p>En construccion</p>")
    }
    fn admin_html(&self) -> String {
        "<p>En construccion</p>".into()
    }
    fn admin_post(&self, _accion: &str, _form: &[(String, String)], _body: &[u8]) -> String {
        "En construccion".into()
    }
}
