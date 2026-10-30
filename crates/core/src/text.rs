use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Normaliza para búsquedas sin distinguir mayúsculas ni acentos.
pub fn fold(s: &str) -> String {
    let lower = s.to_lowercase();
    lower
        .as_str()
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::fold;

    #[test]
    fn pliega_mayusculas_y_acentos() {
        assert_eq!(fold("Árbol ÑANDÚ"), "arbol nandu");
        assert_eq!(fold("MARÍA"), "maria");
        assert_eq!(fold("123"), "123");
    }
}
