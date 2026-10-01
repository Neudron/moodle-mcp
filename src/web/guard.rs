//! Host / Origin / X-Neu checks (spec: Security). Blocks DNS rebinding and
//! cross-site form posts against the localhost server.
pub fn check(
    method: &str,
    path: &str,
    host: Option<&str>,
    origin: Option<&str>,
    x_neu: Option<&str>,
    port: u16,
    sec_fetch_site: Option<&str>,
) -> Result<(), &'static str> {
    if path.starts_with("/oo/") {
        return Ok(()); // OnlyOffice container; handlers verify a JWT
    }
    if sec_fetch_site == Some("cross-site") {
        return Err("cross-site");
    }
    let host = host.ok_or("missing host")?;
    if host != format!("localhost:{port}") && host != format!("127.0.0.1:{port}") {
        return Err("bad host");
    }
    if method == "GET" || method == "HEAD" {
        return Ok(());
    }
    if origin != Some(format!("http://{host}").as_str()) {
        return Err("bad origin");
    }
    if x_neu != Some("1") {
        return Err("missing x-neu");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check;
    const P: u16 = 8787;

    #[test]
    fn host_rules() {
        assert!(check(
            "GET",
            "/api/tasks",
            Some("localhost:8787"),
            None,
            None,
            P,
            None
        )
        .is_ok());
        assert!(check(
            "GET",
            "/api/tasks",
            Some("127.0.0.1:8787"),
            None,
            None,
            P,
            None
        )
        .is_ok());
        assert!(check("GET", "/api/tasks", Some("evil.com"), None, None, P, None).is_err());
        assert!(check(
            "GET",
            "/api/tasks",
            Some("localhost:9999"),
            None,
            None,
            P,
            None
        )
        .is_err());
        assert!(check("GET", "/api/tasks", None, None, None, P, None).is_err());
    }

    #[test]
    fn mutation_needs_origin_and_header() {
        let h = Some("localhost:8787");
        let o = Some("http://localhost:8787");
        assert!(check("POST", "/api/sync", h, o, Some("1"), P, None).is_ok());
        assert!(check("POST", "/api/sync", h, o, None, P, None).is_err());
        assert!(check("POST", "/api/sync", h, None, Some("1"), P, None).is_err());
        assert!(check(
            "POST",
            "/api/sync",
            h,
            Some("http://evil.com"),
            Some("1"),
            P,
            None
        )
        .is_err());
        assert!(check(
            "POST",
            "/api/sync",
            h,
            Some("http://127.0.0.1:8787"),
            Some("1"),
            P,
            None
        )
        .is_err());
    }

    #[test]
    fn cross_site_blocked() {
        let h = Some("localhost:8787");
        assert!(check("GET", "/api/raw", h, None, None, P, Some("cross-site")).is_err());
        for v in [Some("same-origin"), Some("none"), None] {
            assert!(check("GET", "/api/raw", h, None, None, P, v).is_ok());
        }
        assert!(check("GET", "/oo/x", h, None, None, P, Some("cross-site")).is_ok());
    }

    #[test]
    fn oo_routes_skip_host_check() {
        assert!(check(
            "POST",
            "/oo/callback",
            Some("host.docker.internal:8787"),
            None,
            None,
            P,
            None
        )
        .is_ok());
    }
}
