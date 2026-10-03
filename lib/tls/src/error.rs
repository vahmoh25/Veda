//! Errors, with messages a user can act on.

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr};

use rustls::pki_types::{IpAddr, ServerName, UnixTime};
use rustls::{AlertDescription, CertificateError, PeerIncompatible};
use vnet::NetError;

/// Why a TLS operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsError {
    /// The connection under TLS failed (name lookup, connecting, reading or
    /// writing).
    Io(String),
    /// The TLS protocol failed: the server refused the connection, sent
    /// something invalid, or offers nothing this client supports.
    Tls(String),
    /// The server's certificate was rejected.
    BadCertificate(String),
    /// The connection is closed (it was closed with [`close`], or failed
    /// earlier).
    ///
    /// [`close`]: crate::TlsStream::close
    Closed,
    /// The transport's time limit ran out.
    TimedOut,
    /// The server name is neither a DNS name nor an IP address.
    InvalidServerName(String),
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsError::Io(e) => write!(f, "network error: {e}"),
            TlsError::Tls(e) => write!(f, "secure connection failed: {e}"),
            TlsError::BadCertificate(e) => write!(f, "the server's certificate is not trusted: {e}"),
            TlsError::Closed => f.write_str("the secure connection is closed"),
            TlsError::TimedOut => f.write_str("the secure connection timed out"),
            TlsError::InvalidServerName(name) => write!(f, "{name:?} is not a valid server name"),
        }
    }
}

impl From<NetError> for TlsError {
    fn from(e: NetError) -> TlsError {
        match e {
            NetError::TimedOut => TlsError::TimedOut,
            e => TlsError::Io(e.description().to_string()),
        }
    }
}

impl From<rustls::Error> for TlsError {
    fn from(e: rustls::Error) -> TlsError {
        use rustls::Error as E;
        match e {
            E::InvalidCertificate(e) => TlsError::BadCertificate(certificate_problem(&e)),
            E::AlertReceived(alert) => TlsError::Tls(alert_meaning(alert)),
            E::PeerIncompatible(e) => TlsError::Tls(incompatibility(&e)),
            E::FailedToGetCurrentTime => {
                TlsError::Tls("the system clock is not set, so certificates cannot be checked".into())
            }
            E::FailedToGetRandomBytes => TlsError::Tls("no random numbers available".into()),
            E::DecryptError => TlsError::Tls("data from the server failed its integrity check".into()),
            E::NoApplicationProtocol => {
                TlsError::Tls("the server supports none of the requested application protocols".into())
            }
            E::NoCertificatesPresented => TlsError::BadCertificate("the server sent no certificate".into()),
            e => TlsError::Tls(format!("protocol error ({e})")),
        }
    }
}

/// A date as `YYYY-MM-DD` (UTC).
fn date(t: &UnixTime) -> String {
    let d = vrt::time::DateTime::from_unix(t.as_secs());
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// A server name as text.
fn server_name(name: &ServerName<'_>) -> String {
    match name {
        ServerName::DnsName(dns) => dns.as_ref().to_string(),
        ServerName::IpAddress(IpAddr::V4(ip)) => Ipv4Addr::from(*ip.as_ref()).to_string(),
        ServerName::IpAddress(IpAddr::V6(ip)) => Ipv6Addr::from(*ip.as_ref()).to_string(),
        name => format!("{name:?}"),
    }
}

// `UnsupportedSignatureAlgorithm` is deprecated (rustls reports the variants
// with context), but may still be produced by verifiers.
#[allow(deprecated)]
fn certificate_problem(e: &CertificateError) -> String {
    use CertificateError as C;
    match e {
        C::BadEncoding => "it is malformed".into(),
        C::Expired => "it has expired".into(),
        C::ExpiredContext { time, not_after } => {
            format!("it expired on {} (the system clock reads {})", date(not_after), date(time))
        }
        C::NotValidYet => "it is not valid yet (is the system clock right?)".into(),
        C::NotValidYetContext { time, not_before } => {
            format!("it is only valid from {} (the system clock reads {}; is it right?)", date(not_before), date(time))
        }
        C::Revoked => "it has been revoked".into(),
        C::UnknownIssuer => "it was not issued by a trusted certificate authority".into(),
        C::BadSignature => "its signature is invalid".into(),
        C::UnsupportedSignatureAlgorithm
        | C::UnsupportedSignatureAlgorithmContext { .. }
        | C::UnsupportedSignatureAlgorithmForPublicKeyContext { .. } => {
            "it is signed with an unsupported algorithm".into()
        }
        C::NotValidForName => "it is for a different name".into(),
        C::NotValidForNameContext { expected, presented } => {
            let expected = server_name(expected);
            match presented.as_slice() {
                [] => format!("it is not valid for {expected}"),
                names => format!("it is not valid for {expected} (only for {})", names.join(", ")),
            }
        }
        C::InvalidPurpose | C::InvalidPurposeContext { .. } => "it is not meant for TLS servers".into(),
        C::UnhandledCriticalExtension => "it has an unsupported critical extension".into(),
        e => format!("{e:?}"),
    }
}

fn alert_meaning(alert: AlertDescription) -> String {
    use AlertDescription as A;
    match alert {
        A::HandshakeFailure | A::InsufficientSecurity => {
            "the server refused the connection: no security parameters in common".into()
        }
        A::ProtocolVersion => "the server refused the connection: it does not support TLS 1.2 or 1.3".into(),
        A::UnrecognisedName => "the server does not serve this name".into(),
        A::AccessDenied => "the server denied access".into(),
        A::InternalError => "the server reported an internal error".into(),
        A::CloseNotify => "the server closed the connection".into(),
        A::NoApplicationProtocol => "the server supports none of the requested application protocols".into(),
        alert => format!("the server sent the alert \"{}\"", alert.as_str().unwrap_or("unknown")),
    }
}

fn incompatibility(e: &PeerIncompatible) -> String {
    use PeerIncompatible as P;
    match e {
        P::NoCipherSuitesInCommon => "the server supports none of our cipher suites".into(),
        P::NoKxGroupsInCommon => "the server supports none of our key exchange groups".into(),
        P::NoSignatureSchemesInCommon => "the server supports none of our signature schemes".into(),
        P::ServerDoesNotSupportTls12Or13 => "the server does not support TLS 1.2 or 1.3".into(),
        e => format!("the server is incompatible ({e:?})"),
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    #[test]
    fn messages() {
        let expired = rustls::Error::InvalidCertificate(CertificateError::ExpiredContext {
            time: UnixTime::since_unix_epoch(Duration::from_secs(1_790_000_000)),
            not_after: UnixTime::since_unix_epoch(Duration::from_secs(1_735_689_600)),
        });
        assert_eq!(
            TlsError::from(expired).to_string(),
            "the server's certificate is not trusted: it expired on 2025-01-01 (the system clock reads 2026-09-21)"
        );
        let name = rustls::Error::InvalidCertificate(CertificateError::NotValidForNameContext {
            expected: ServerName::try_from("example.com").unwrap(),
            presented: alloc::vec![String::from("DnsName(\"other.org\")")],
        });
        assert!(TlsError::from(name).to_string().contains("not valid for example.com"));
        assert_eq!(
            TlsError::from(rustls::Error::AlertReceived(AlertDescription::HandshakeFailure)),
            TlsError::Tls("the server refused the connection: no security parameters in common".into())
        );
        assert_eq!(TlsError::from(NetError::TimedOut), TlsError::TimedOut);
        assert_eq!(TlsError::from(NetError::ConnectionRefused).to_string(), "network error: connection refused");
    }
}
