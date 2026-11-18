//! Connecting to the ladder at an address of our choosing.
//!
//! The ladder sits behind several edge addresses, and on 2026-11-18 one of them was fast
//! (about 100 ms charged per turn) while another was slow for a connection's whole life
//! (230 to 290 ms, with turns lost to the one-second limit). So the client resolves the
//! host itself, tries addresses in an order it chooses (preferred ones first, ones that
//! were just slow last), and keeps the hostname for TLS and the handshake.

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;

pub type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// The addresses of `host` in the order to try them.
pub fn candidates(host: &str, port: u16, prefer: &[IpAddr], avoid: &[IpAddr]) -> Result<Vec<SocketAddr>, String> {
    let mut found: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("cannot resolve {host}: {e}"))?.collect();
    found.dedup();
    if found.is_empty() {
        return Err(format!("{host} has no address"));
    }
    // Stable sort: avoided last, then preferred in the order given, then as resolved.
    found.sort_by_key(|a| {
        let ip = a.ip();
        (avoid.contains(&ip), prefer.iter().position(|p| *p == ip).unwrap_or(usize::MAX))
    });
    Ok(found)
}

/// Opens the WebSocket. Returns it with the address it ended up on.
pub fn connect(url: &str, token: &str, prefer: &[IpAddr], avoid: &[IpAddr]) -> Result<(Socket, IpAddr), String> {
    let probe = url.into_client_request().map_err(|e| format!("bad url: {e}"))?;
    let uri = probe.uri().clone();
    let host = uri.host().ok_or("the url has no host")?.to_string();
    let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
    let mut last = String::new();
    for addr in candidates(&host, port, prefer, avoid)? {
        let stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(s) => s,
            Err(e) => {
                last = format!("connect to {addr} failed: {e}");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        // The handshake must not hang on a dead edge.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(8)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(8)));
        let mut req = url.into_client_request().map_err(|e| format!("bad url: {e}"))?;
        let header = format!("Bearer {token}").parse().map_err(|_| "token has characters a header cannot carry".to_string())?;
        req.headers_mut().insert("Authorization", header);
        match tungstenite::client_tls(req, stream) {
            Ok((ws, _)) => return Ok((ws, addr.ip())),
            Err(e) => last = format!("connect failed: {e}"),
        }
    }
    Err(last)
}

/// A comma-separated list of addresses; anything that is not one is left out.
pub fn parse_addresses(text: &str) -> Vec<IpAddr> {
    text.split(',').filter_map(|a| a.trim().parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_of_addresses() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let got = candidates("127.0.0.1", 80, &[], &[]).unwrap();
        assert_eq!(got, vec![SocketAddr::new(ip("127.0.0.1"), 80)]);
        // An avoided address is still tried when it is the only one.
        let got = candidates("127.0.0.1", 80, &[], &[ip("127.0.0.1")]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(parse_addresses("104.21.89.84, nonsense,172.67.157.96"), vec![ip("104.21.89.84"), ip("172.67.157.96")]);
    }
}
