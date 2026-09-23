use regex::Regex;
use std::sync::OnceLock;

fn re_ra() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re_get(&RE, r"(?i)\bRA\s*-?\s*(\d{1,2})\b")
}
fn re_res() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re_get(&RE, r"(?i)resultats?\s+d'?aprenentatge\s+(\d{1,2})")
}
fn re_uf() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    re_get(&RE, r"(?i)^(?:uf|unitat|unit)\s*(\d{1,2})\b")
}
fn re_get(cell: &'static OnceLock<Regex>, pat: &'static str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("static regex"))
}

/// Cascade: section name -> module name -> module description.
/// None => caller picks 00-intro/99-misc.
pub fn detect(section_name: &str, module_texts: &[&str]) -> Option<String> {
    let caps = |r: &Regex, s: &str| {
        r.captures(s)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    };
    if let Some(n) = caps(re_ra(), section_name) {
        return Some(format!("RA{n:0>2}"));
    }
    if let Some(n) = caps(re_res(), section_name) {
        return Some(format!("RA{n:0>2}"));
    }
    if let Some(n) = caps(re_uf(), section_name) {
        return Some(format!("UF{n:0>2}"));
    }
    for t in module_texts {
        if let Some(n) = caps(re_ra(), t) {
            return Some(format!("RA{n:0>2}"));
        }
        if let Some(n) = caps(re_res(), t) {
            return Some(format!("RA{n:0>2}"));
        }
    }
    None
}

pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(['.', ' ']).to_string();
    let capped: String = trimmed.chars().take(80).collect();
    if capped.is_empty() {
        "unnamed".into()
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ra_variants() {
        assert_eq!(detect("RA1", &[]), Some("RA01".into()));
        assert_eq!(detect("RA 3 - titol", &[]), Some("RA03".into()));
        assert_eq!(detect("ra-12", &[]), Some("RA12".into()));
        assert_eq!(
            detect("Resultat d'Aprenentatge 2", &[]),
            Some("RA02".into())
        );
        assert_eq!(
            detect("Resultats d'aprenentatge 07", &[]),
            Some("RA07".into())
        );
        assert_eq!(detect("UF1 Introducció", &[]), Some("UF01".into()));
        assert_eq!(detect("Unitat 2: xarxes", &[]), Some("UF02".into()));
        assert_eq!(detect("General", &["RA5 practica"]), Some("RA05".into()));
        assert_eq!(detect("General", &[]), None);
    }

    #[test]
    fn sanitize_rules() {
        assert_eq!(sanitize("a<b>:c/d\\e|f?g*h"), "abcdefgh");
        assert_eq!(sanitize("  trim . dots. "), "trim . dots");
        assert_eq!(sanitize(""), "unnamed");
    }
}
