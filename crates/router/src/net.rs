//! Outbound HTTP to endpoints that tenants and users choose.
//!
//! Endpoints from `lighthouse.toml` are trusted. Endpoints that a tenant or a
//! person sets through the API are not: they could point at the server's own
//! network, a cloud metadata service or another tenant's internal service. The
//! checks here allow only `https` URLs whose host resolves to public
//! addresses. The guarded client checks every address again when it connects,
//! so a name that later resolves to a private address (DNS rebinding) is
//! refused too, and it follows redirects only to public `https` URLs.

pub use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Host names that always point to the machine or its cloud environment.
const BLOCKED_HOSTS: &[&str] = &[
    "localhost",
    "metadata",
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
];

/// Why an endpoint is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EndpointError {
    /// The text is not a URL.
    #[error("`{0}` is not a valid URL")]
    Invalid(String),
    /// Only `https` is allowed.
    #[error("the URL must use https")]
    NotHttps,
    /// The URL carries a user name or password.
    #[error("the URL must not contain credentials")]
    Credentials,
    /// The host is private, loopback, link-local or otherwise not public.
    #[error("`{0}` is not a public address")]
    Private(String),
    /// The host name does not resolve.
    #[error("cannot resolve `{0}`")]
    Unresolved(String),
}

/// Whether an address is reachable on the public internet.
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_documentation()
        || a == 0
        || a >= 240
        // Shared address space (carrier-grade NAT).
        || (a == 100 && (64..=127).contains(&b))
        // Benchmarking.
        || (a == 198 && (b == 18 || b == 19))
        // IETF protocol assignments.
        || (a == 192 && b == 0 && c == 0))
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(embedded) = embedded_v4(address) {
        return is_public_v4(embedded);
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        // Link-local.
        || (segments[0] & 0xffc0) == 0xfe80
        // Unique local.
        || (segments[0] & 0xfe00) == 0xfc00
        // Site-local, deprecated.
        || (segments[0] & 0xffc0) == 0xfec0
        // Documentation.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // Teredo and other IETF assignments.
        || (segments[0] == 0x2001 && segments[1] < 0x0200)
        // Local-use NAT64.
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1))
}

/// The IPv4 address inside an IPv4-mapped, IPv4-compatible, NAT64 or 6to4 address.
fn embedded_v4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let [a, b, c, d, e, f, g, h] = address.segments();
    let low = Ipv4Addr::from((u32::from(g) << 16) | u32::from(h));
    match [a, b, c, d, e, f] {
        [0, 0, 0, 0, 0, 0xffff] | [0x64, 0xff9b, 0, 0, 0, 0] => Some(low),
        [0, 0, 0, 0, 0, 0] if !address.is_loopback() && !address.is_unspecified() => Some(low),
        [0x2002, ..] => Some(Ipv4Addr::from((u32::from(b) << 16) | u32::from(c))),
        _ => None,
    }
}

fn blocked_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    BLOCKED_HOSTS.contains(&host.as_str())
        || host.ends_with(".localhost")
        || host.ends_with(".internal")
        || host.ends_with(".local")
}

/// Parses a URL and checks it without resolving its host: `https`, no
/// credentials, and no host that is private by its name or address.
pub fn check_url(text: &str) -> Result<Url, EndpointError> {
    let url = Url::parse(text.trim()).map_err(|_| EndpointError::Invalid(text.to_owned()))?;
    if url.scheme() != "https" {
        return Err(EndpointError::NotHttps);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EndpointError::Credentials);
    }
    match url.host() {
        None => Err(EndpointError::Invalid(text.to_owned())),
        Some(url::Host::Domain(domain)) if blocked_name(domain) => {
            Err(EndpointError::Private(domain.to_owned()))
        }
        Some(url::Host::Ipv4(address)) if !is_public_v4(address) => {
            Err(EndpointError::Private(address.to_string()))
        }
        Some(url::Host::Ipv6(address)) if !is_public_v6(address) => {
            Err(EndpointError::Private(address.to_string()))
        }
        Some(_) => Ok(url),
    }
}

/// Checks a URL and resolves its host: every address must be public.
pub async fn check_endpoint(text: &str) -> Result<Url, EndpointError> {
    let url = check_url(text)?;
    let host = url
        .host_str()
        .ok_or_else(|| EndpointError::Invalid(text.to_owned()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| EndpointError::Unresolved(host.clone()))?
        .collect();
    if addresses.is_empty() {
        return Err(EndpointError::Unresolved(host));
    }
    if let Some(private) = addresses.iter().find(|address| !is_public(address.ip())) {
        return Err(EndpointError::Private(format!("{host} ({})", private.ip())));
    }
    Ok(url)
}

/// Resolves names like the system and drops every address that is not public.
#[derive(Debug, Default)]
pub struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            if blocked_name(&host) {
                return Err(Box::new(EndpointError::Private(host)) as _);
            }
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|address| is_public(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(Box::new(EndpointError::Private(host)) as _);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// A client builder that only reaches public `https` endpoints: names resolve
/// through [`PublicResolver`] and redirects must pass [`check_url`].
pub fn guarded_builder() -> reqwest::ClientBuilder {
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if let Err(error) = check_url(attempt.url().as_str()) {
            attempt.error(error)
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicResolver))
        .redirect(redirects)
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(true)
}

/// A client from [`guarded_builder`].
pub fn guarded_client() -> reqwest::Client {
    guarded_builder().build().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_reserved_addresses_are_not_public() {
        for address in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2002:a00:1::",
            "2001:db8::1",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn urls_need_https_a_public_host_and_no_credentials() {
        assert!(check_url("https://api.example.com/v1").is_ok());
        assert_eq!(
            check_url("http://api.example.com"),
            Err(EndpointError::NotHttps)
        );
        assert_eq!(
            check_url("https://user:pw@api.example.com"),
            Err(EndpointError::Credentials)
        );
        for private in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://169.254.169.254/latest",
            "https://metadata.google.internal/",
            "https://printer.local/",
            "https://10.0.0.5:8443/mcp",
        ] {
            assert!(
                matches!(check_url(private), Err(EndpointError::Private(_))),
                "{private}"
            );
        }
        assert!(matches!(
            check_url("not a url"),
            Err(EndpointError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn names_that_resolve_to_private_addresses_are_refused() {
        let error = check_endpoint("https://127.0.0.1.nip.invalid/").await;
        assert!(
            matches!(error, Err(EndpointError::Unresolved(_))),
            "{error:?}"
        );
        let error = check_endpoint("https://[::ffff:127.0.0.1]/").await;
        assert!(matches!(error, Err(EndpointError::Private(_))), "{error:?}");
    }

    #[tokio::test]
    async fn the_resolver_drops_private_addresses() {
        let resolver = PublicResolver;
        let name: Name = "localhost".parse().unwrap();
        assert!(resolver.resolve(name).await.is_err());
        let client = guarded_client();
        let error = client.get("https://localhost:9/").send().await.unwrap_err();
        assert!(error.is_connect() || error.is_request(), "{error}");
    }
}
