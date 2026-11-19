//! Códigos para emparejar, al estilo croc: `<1-99>-palabra-palabra-palabra`, fáciles de dictar.
//! Palabras cortas, comunes, sin acentos ni ñ, y distintas "de oído": al normalizar se compara
//! una clave fonética (b/v, s/z/c, y/ll, h muda, k/qu/c), así "baca" o "bacA" encuentran "vaca".
//! Entropía: 99 × 168³ ≈ 4.7×10⁸ combinaciones (más el límite de 15 intentos/min por IP).

use super::aleatorio;

pub(super) const PALABRAS: &[&str] = &[
    "luna", "perro", "nube", "sol", "mar", "gato", "casa", "flor", "rio", "pan", "leche", "mesa", "silla", "lapiz", "libro",
    "tren", "barco", "avion", "playa", "monte", "selva", "lago", "isla", "nieve", "lluvia", "viento", "fuego", "piedra",
    "arena", "hoja", "arbol", "rama", "fruta", "pera", "uva", "limon", "mango", "coco", "fresa", "melon", "nuez", "maiz",
    "arroz", "queso", "miel", "sopa", "taco", "tamal", "mole", "chile", "oso", "lobo", "zorro", "tigre", "leon", "raton",
    "conejo", "pato", "gallo", "vaca", "toro", "burro", "mono", "rana", "sapo", "pez", "ballena", "delfin", "pulpo", "foca",
    "loro", "buho", "aguila", "abeja", "hormiga", "mosca", "llave", "reloj", "vela", "cama", "radio", "foto", "carta",
    "caja", "bolsa", "gorra", "bota", "zapato", "camisa", "falda", "guante", "anillo", "perla", "oro", "plata", "cobre",
    "rojo", "azul", "verde", "negro", "blanco", "gris", "rosa", "cafe", "morado", "dia", "noche", "cielo", "estrella",
    "cometa", "planeta", "tierra", "piano", "guitarra", "tambor", "flauta", "violin", "pelota", "globo", "dado", "mapa",
    "puerta", "ventana", "techo", "piso", "jardin", "parque", "calle", "puente", "torre", "faro", "volcan", "desierto",
    "bosque", "camino", "nopal", "cactus", "maceta", "cuchara", "tenedor", "plato", "vaso", "taza", "olla", "tortilla",
    "galleta", "pastel", "dulce", "chicle", "helado", "jugo", "agua", "hielo", "trueno", "rayo", "arcoiris", "pluma",
    "papel", "tijera", "martillo", "clavo", "cuerda", "escoba", "jabon", "toalla", "peine",
];

/// Clave "de oído" de una palabra ya en minúsculas ASCII.
pub(super) fn fonetica(p: &str) -> String {
    let p = p.replace("ll", "y").replace("qu", "k").replace("ce", "se").replace("ci", "si").replace("ge", "je").replace("gi", "ji");
    p.chars()
        .filter(|&c| c != 'h')
        .map(|c| match c {
            'v' => 'b',
            'z' => 's',
            'c' | 'q' => 'k',
            c => c,
        })
        .collect()
}

fn sin_acentos(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        c => c,
    }
}

/// Normaliza lo que escribió la persona ("7 Luna, perro-NUBE") a la forma canónica
/// ("7-luna-perro-nube"). `None` si no tiene la forma de un código.
pub(crate) fn normalizar(s: &str) -> Option<String> {
    if s.len() > 120 {
        return None;
    }
    let s: String = s.to_lowercase().chars().map(sin_acentos).collect();
    let partes: Vec<&str> = s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|p| !p.is_empty()).collect();
    if partes.len() != 4 {
        return None;
    }
    let n: u8 = partes[0].parse().ok()?;
    if !(1..=99).contains(&n) || partes[0].starts_with('0') {
        return None;
    }
    let mut o = n.to_string();
    for p in &partes[1..] {
        let f = fonetica(p);
        let w = PALABRAS.iter().find(|w| fonetica(w) == f)?;
        o.push('-');
        o.push_str(w);
    }
    Some(o)
}

pub(super) fn generar() -> String {
    let r = aleatorio(8);
    let num = |i: usize| u16::from_le_bytes([r[i], r[i + 1]]) as usize;
    format!(
        "{}-{}-{}-{}",
        1 + num(0) % 99,
        PALABRAS[num(2) % PALABRAS.len()],
        PALABRAS[num(4) % PALABRAS.len()],
        PALABRAS[num(6) % PALABRAS.len()]
    )
}
