//! Optional second listener for one trusted PC on the home LAN (`WEB_LAN_BIND` + `WEB_LAN_PEER`).
//! Every request must come from the peer IP and carry the secret from `.moodle/lan-token`
//! (first visit: `/?k=<token>` sets an HttpOnly cookie). Admitted requests are rewritten to
//! the localhost Host/Origin so `guard::check` applies unchanged.
use super::App;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

const COOKIE: &str = "neu_lan";

/// Request extension: the request came in through the LAN listener (client is not this machine).
#[derive(Clone, Copy, Debug)]
pub struct Remote;

#[derive(Debug, PartialEq)]
pub enum Admit {
    Deny,
    SetCookie,
    Pass,
}

fn eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |x, (p, q)| x | (p ^ q)) == 0
}

pub fn admit(peer: IpAddr, allowed: IpAddr, cookies: &str, query: &str, token: &str) -> Admit {
    if peer != allowed || token.len() < 32 {
        return Admit::Deny;
    }
    let has = |s: &str, sep: char, k: &str| {
        s.split(sep)
            .filter_map(|kv| kv.trim().split_once('='))
            .any(|(n, v)| n == k && eq(v, token))
    };
    if has(cookies, ';', COOKIE) {
        Admit::Pass
    } else if has(query, '&', "k") {
        Admit::SetCookie
    } else {
        Admit::Deny
    }
}

/// Reads `.moodle/lan-token`, creating a random 128-bit one (mode 0600) on first use.
pub fn token(root: &std::path::Path) -> std::io::Result<String> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let p = root.join(".moodle/lan-token");
    if let Ok(t) = std::fs::read_to_string(&p) {
        return Ok(t.trim().into());
    }
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&p)?
        .write_all(t.as_bytes())?;
    Ok(t)
}

struct Lan {
    bind: String,
    peer: IpAddr,
    token: String,
    port: u16,
}

async fn mw(
    State(lan): State<Arc<Lan>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let h = |req: &Request, k| {
        req.headers()
            .get(k)
            .and_then(|v: &HeaderValue| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (cookies, query) = (
        h(&req, header::COOKIE),
        req.uri().query().unwrap_or("").to_string(),
    );
    match admit(addr.ip(), lan.peer, &cookies, &query, &lan.token) {
        Admit::Deny => (StatusCode::FORBIDDEN, "forbidden").into_response(),
        Admit::SetCookie => {
            let c = format!(
                "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000",
                lan.token
            );
            let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
            if let Ok(v) = HeaderValue::from_str(&c) {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            r
        }
        Admit::Pass => {
            // Same-origin on the LAN host becomes same-origin on localhost; any other Origin
            // is left as is and still rejected by guard::check.
            let lan_origin = format!("http://{}:{}", lan.bind, lan.port);
            let local = format!("localhost:{}", lan.port);
            let hs = req.headers_mut();
            if hs.get(header::ORIGIN).and_then(|v| v.to_str().ok()) == Some(lan_origin.as_str()) {
                hs.insert(
                    header::ORIGIN,
                    HeaderValue::from_str(&format!("http://{local}")).unwrap(),
                );
            }
            hs.insert(header::HOST, HeaderValue::from_str(&local).unwrap());
            req.extensions_mut().insert(Remote);
            next.run(req).await
        }
    }
}

/// Serves the full app on `bind` for `peer` only. Errors (address not on this machine, e.g.
/// on another network) are logged and the listener is skipped.
pub async fn spawn(app: Arc<App>, bind: &str, peer: &str, root: &std::path::Path) {
    let (Ok(peer), Ok(token)) = (peer.parse::<IpAddr>(), token(root)) else {
        tracing::warn!("WEB_LAN_PEER invalid or lan-token unreadable, LAN listener off");
        return;
    };
    let port = app.port;
    let l = match tokio::net::TcpListener::bind((bind, port)).await {
        Ok(l) => l,
        Err(e) => return tracing::warn!(error = %e, %bind, "LAN bind failed, LAN listener off"),
    };
    let lan = Arc::new(Lan {
        bind: bind.into(),
        peer,
        token,
        port,
    });
    let r = super::router(app).layer(axum::middleware::from_fn_with_state(lan, mw));
    tracing::info!("LAN access on http://{bind}:{port} for {peer} only");
    tokio::spawn(async move {
        axum::serve(l, r.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .ok()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn admit_needs_peer_and_token() {
        let me: IpAddr = "192.168.18.6".parse().unwrap();
        let other: IpAddr = "192.168.18.7".parse().unwrap();
        let ok = format!("a=1; neu_lan={T}");
        assert_eq!(admit(me, me, &ok, "", T), Admit::Pass);
        assert_eq!(admit(other, me, &ok, "", T), Admit::Deny);
        assert_eq!(admit(me, me, "neu_lan=wrong", "", T), Admit::Deny);
        assert_eq!(admit(me, me, "", "", T), Admit::Deny);
        assert_eq!(
            admit(me, me, "", &format!("x=1&k={T}"), T),
            Admit::SetCookie
        );
        assert_eq!(admit(me, me, "", &format!("k={T}x"), T), Admit::Deny);
        assert_eq!(admit(me, me, "neu_lan=", "k=", ""), Admit::Deny); // short token never admits
    }
}
