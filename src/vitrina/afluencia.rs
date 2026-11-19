//! Módulo "Afluencia" — ESQUELETO, lo implementa su agente.
use super::{GuestReq, GuestResp, Vitrina};

pub struct Afluencia;

impl Afluencia {
    pub fn new() -> Self {
        Afluencia
    }
}

impl Vitrina for Afluencia {
    fn id(&self) -> &'static str {
        "afluencia"
    }
    fn nombre(&self) -> &'static str {
        "Afluencia"
    }
    fn descripcion(&self) -> &'static str {
        "Cuenta celulares cercanos (anonimo)"
    }
    fn ssid_por_defecto(&self) -> String {
        "Afluencia".into()
    }
    fn guest(&self, _req: &GuestReq) -> GuestResp {
        GuestResp::html("<h1>Afluencia</h1><p>En construccion</p>")
    }
    fn admin_html(&self) -> String {
        "<p>En construccion</p>".into()
    }
    fn admin_post(&self, _accion: &str, _form: &[(String, String)], _body: &[u8]) -> String {
        "En construccion".into()
    }
}
