mod circuit;
mod constants;
mod enums;
mod miner_response;
mod protocol;
mod request;

pub use circuit::*;
pub use constants::*;
pub use enums::*;
pub use miner_response::*;
pub use protocol::*;
pub use request::*;

pub fn format_http_url(ip: &str, port: u16, path: &str) -> String {
    let host = if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    format!("http://{}:{}/{}", host, port, path.trim_start_matches('/'))
}

pub fn signing_message(nonce: &str, hotkey: &str, payload_hash: &str) -> String {
    format!("{nonce}:{hotkey}:{payload_hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_http_url_ipv4() {
        assert_eq!(
            format_http_url("1.2.3.4", 8080, "health"),
            "http://1.2.3.4:8080/health"
        );
    }

    #[test]
    fn format_http_url_ipv6() {
        assert_eq!(format_http_url("::1", 9090, "api"), "http://[::1]:9090/api");
    }

    #[test]
    fn format_http_url_strips_leading_slash() {
        assert_eq!(
            format_http_url("10.0.0.1", 80, "/path"),
            "http://10.0.0.1:80/path"
        );
    }

    #[test]
    fn format_http_url_already_bracketed_ipv6() {
        assert_eq!(
            format_http_url("[::1]", 9090, "api"),
            "http://[::1]:9090/api"
        );
    }

    #[test]
    fn signing_message_format() {
        assert_eq!(signing_message("123", "0xabc", "def"), "123:0xabc:def");
    }
}
