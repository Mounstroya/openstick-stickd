use std::process::Command;

/// Ejecuta un programa SIN shell (los argumentos nunca se interpretan) y devuelve (éxito, stdout).
pub fn run(prog: &str, args: &[&str]) -> (bool, String) {
    match Command::new(prog).args(args).output() {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()),
        Err(_) => (false, String::new()),
    }
}

pub fn ok(prog: &str, args: &[&str]) -> bool {
    run(prog, args).0
}

pub fn log(msg: &str) {
    eprintln!("stickd: {msg}");
}

/// Escapa texto para HTML (los SSID escaneados son datos de terceros).
pub fn html(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

pub fn json(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Decodifica application/x-www-form-urlencoded.
pub fn form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut it = p.splitn(2, '=');
            (dec(it.next().unwrap_or("")), dec(it.next().unwrap_or("")))
        })
        .collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Divide una línea de `nmcli -t` respetando los `\:` escapados.
pub fn nm_fields(line: &str) -> Vec<String> {
    let mut f = vec![String::new()];
    let mut esc = false;
    for c in line.chars() {
        if esc {
            f.last_mut().unwrap().push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == ':' {
            f.push(String::new());
        } else {
            f.last_mut().unwrap().push(c);
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodifica_form() {
        let f = form("ssid=Mi+Red%21&psk=a%26b%3Dc&x=%");
        assert_eq!(f[0], ("ssid".into(), "Mi Red!".into()));
        assert_eq!(f[1], ("psk".into(), "a&b=c".into()));
        assert_eq!(f[2], ("x".into(), "%".into()));
    }
    #[test]
    fn campos_nmcli() {
        assert_eq!(nm_fields(r"Mi Red\:Casa:97:WPA2"), vec!["Mi Red:Casa", "97", "WPA2"]);
    }
    #[test]
    fn escapa_html() {
        assert_eq!(html("<a href='x'>"), "&lt;a href=&#39;x&#39;&gt;");
    }
}
