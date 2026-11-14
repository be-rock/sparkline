//! Spark Connect connection strings.
//!
//! Format (same as the official clients):
//! `sc://host[:port]/;param1=value;param2=value`
//!
//! Recognised params: `token` (bearer token, implies TLS), `use_ssl`,
//! `user_id`, `user_agent`, `session_id`. Any other param is sent as a gRPC
//! metadata header (e.g. `x-databricks-cluster-id`).

use std::fmt;

pub const DEFAULT_PORT: u16 = 15002;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConnError {
    #[error("connection string must start with sc:// (got {0:?})")]
    Scheme(String),
    #[error("connection string has no host")]
    NoHost,
    #[error("invalid port {0:?}")]
    Port(String),
    #[error("invalid parameter {0:?}: expected key=value")]
    Param(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionString {
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
    pub use_ssl: bool,
    pub user_id: Option<String>,
    pub user_agent: Option<String>,
    pub session_id: Option<String>,
    /// Extra gRPC metadata headers.
    pub headers: Vec<(String, String)>,
}

impl ConnectionString {
    pub fn parse(s: &str) -> Result<Self, ConnError> {
        let s = s.trim();
        let rest = s
            .strip_prefix("sc://")
            .ok_or_else(|| ConnError::Scheme(s.to_string()))?;
        let (authority, params) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };

        let (host, port) = split_host_port(authority)?;
        let mut cs = ConnectionString {
            host,
            port,
            token: None,
            use_ssl: false,
            user_id: None,
            user_agent: None,
            session_id: None,
            headers: Vec::new(),
        };

        for param in params.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = param
                .split_once('=')
                .ok_or_else(|| ConnError::Param(param.to_string()))?;
            let (k, v) = (k.trim(), v.trim().to_string());
            match k {
                "token" => cs.token = Some(v),
                "use_ssl" => cs.use_ssl = v.eq_ignore_ascii_case("true"),
                "user_id" => cs.user_id = Some(v),
                "user_agent" => cs.user_agent = Some(v),
                "session_id" => cs.session_id = Some(v),
                _ => cs.headers.push((k.to_ascii_lowercase(), v)),
            }
        }
        // Matches the reference Python client: a token always means TLS.
        if cs.token.is_some() {
            cs.use_ssl = true;
        }
        Ok(cs)
    }

    /// The HTTP/2 endpoint URI for tonic.
    pub fn endpoint(&self) -> String {
        let scheme = if self.use_ssl { "https" } else { "http" };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{scheme}://{host}:{}", self.port)
    }
}

fn split_host_port(authority: &str) -> Result<(String, u16), ConnError> {
    if authority.is_empty() {
        return Err(ConnError::NoHost);
    }
    // IPv6 literal: [::1]:15002
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or(ConnError::NoHost)?;
        let host = rest[..end].to_string();
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None => DEFAULT_PORT,
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => Ok((h.to_string(), parse_port(p)?)),
        Some(_) => Err(ConnError::NoHost),
        None => Ok((authority.to_string(), DEFAULT_PORT)),
    }
}

fn parse_port(p: &str) -> Result<u16, ConnError> {
    p.parse().map_err(|_| ConnError::Port(p.to_string()))
}

/// Displays the connection string with the token redacted.
impl fmt::Display for ConnectionString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        write!(f, "sc://{host}:{}", self.port)
    }
}

impl fmt::Debug for ConnectionString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionString")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("use_ssl", &self.use_ssl)
            .field("user_id", &self.user_id)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_only_uses_default_port() {
        let cs = ConnectionString::parse("sc://localhost").unwrap();
        assert_eq!(cs.host, "localhost");
        assert_eq!(cs.port, DEFAULT_PORT);
        assert!(!cs.use_ssl);
        assert_eq!(cs.endpoint(), "http://localhost:15002");
    }

    #[test]
    fn params_and_headers() {
        let cs = ConnectionString::parse(
            "sc://example.com:443/;token=abc;user_id=me;x-databricks-cluster-id=0123",
        )
        .unwrap();
        assert_eq!(cs.port, 443);
        assert_eq!(cs.token.as_deref(), Some("abc"));
        assert!(cs.use_ssl, "token implies TLS");
        assert_eq!(cs.user_id.as_deref(), Some("me"));
        assert_eq!(cs.headers, vec![("x-databricks-cluster-id".into(), "0123".into())]);
        assert_eq!(cs.endpoint(), "https://example.com:443");
    }

    #[test]
    fn ipv6() {
        let cs = ConnectionString::parse("sc://[::1]:15003").unwrap();
        assert_eq!(cs.host, "::1");
        assert_eq!(cs.port, 15003);
        assert_eq!(cs.endpoint(), "http://[::1]:15003");
    }

    #[test]
    fn errors() {
        assert!(matches!(ConnectionString::parse("http://x"), Err(ConnError::Scheme(_))));
        assert_eq!(ConnectionString::parse("sc://"), Err(ConnError::NoHost));
        assert!(matches!(ConnectionString::parse("sc://h:abc"), Err(ConnError::Port(_))));
        assert!(matches!(ConnectionString::parse("sc://h/;novalue"), Err(ConnError::Param(_))));
    }

    #[test]
    fn display_redacts_token() {
        let cs = ConnectionString::parse("sc://h:1/;token=secret").unwrap();
        assert!(!cs.to_string().contains("secret"));
        assert!(!format!("{cs:?}").contains("secret"));
    }
}
