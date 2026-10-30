use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Normaliza para búsquedas sin distinguir mayúsculas ni acentos.
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
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

    #[test]
    fn conserva_normalizacion_ascii_y_unicode() {
        use unicode_normalization::UnicodeNormalization;
        use unicode_normalization::char::is_combining_mark;

        let ascii: String = (0u8..=127).map(char::from).collect();
        for s in [
            ascii.as_str(),
            "",
            "true false null -123.45e+6",
            "C:\\Datos\\ARCHIVO.json",
            "ÁÉÍÓÚ ñ Ç ü",
            "A\u{301} Jose\u{301}",
            "ΟΣ ΟΣΑ İ ẞ K 界 😀",
        ] {
            let expected: String = s
                .to_lowercase()
                .nfd()
                .filter(|c| !is_combining_mark(*c))
                .collect();
            assert_eq!(fold(s), expected, "{s:?}");
        }
    }
}
