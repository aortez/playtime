use crate::{Failure, FailureKind, Limits, Result};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint(reqwest::Url);
impl Endpoint {
    pub fn parse(text: &str) -> Result<Self> {
        let url = reqwest::Url::parse(text)
            .map_err(|_| Failure::new(FailureKind::Request, "invalid Ollama endpoint URL"))?;
        let authority = text
            .split_once("://")
            .map_or("", |(_, s)| s.split(['/', '?', '#']).next().unwrap_or(""));
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || authority.contains('@')
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Failure::new(
                FailureKind::Request,
                "Ollama endpoint requires HTTP(S), no userinfo/query/fragment",
            ));
        }
        Ok(Self(url))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
    /// The host when it is an IP address rather than a name.
    pub fn ip_literal(&self) -> Option<IpAddr> {
        let host = self.0.host_str()?;
        host.strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
            .parse()
            .ok()
    }
    pub fn url(&self, operation: Operation) -> reqwest::Url {
        let mut url = self.0.clone();
        url.set_path(&format!(
            "{}{}",
            url.path().trim_end_matches('/'),
            operation.path()
        ));
        url
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Version,
    Tags,
    Show,
    Loaded,
    Chat,
}
impl Operation {
    pub fn path(self) -> &'static str {
        match self {
            Self::Version => "/api/version",
            Self::Tags => "/api/tags",
            Self::Show => "/api/show",
            Self::Loaded => "/api/ps",
            Self::Chat => "/api/chat",
        }
    }
    pub fn method(self) -> reqwest::Method {
        match self {
            Self::Show | Self::Chat => reqwest::Method::POST,
            _ => reqwest::Method::GET,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Request {
    pub operation: Operation,
    pub body: Option<Vec<u8>>,
}
pub type ResponseBody = Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>;
pub struct HttpResponse {
    pub status: u16,
    pub content_encoding: Option<String>,
    pub body: ResponseBody,
}
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Future/body must release their owned transport when dropped. No replay,
    /// redirects or detached receives. Cancellation/deadline belongs to Adapter.
    async fn send(&self, request: Request) -> Result<HttpResponse>;
}
/// Owned certificate bytes, never a path/environment variable reread at request
/// time. Persist this configuration with frozen inputs when integrating O2.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trust {
    BundledWebPki,
    PemOnly { certificates: Vec<String> },
}
/// Explicit frozen addresses are useful for static workers whose system
/// resolver cannot provide the operator's LAN/mDNS name. They change only DNS
/// resolution, never the configured hostname, Host header or TLS identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resolution {
    System,
    Pinned { addresses: Vec<IpAddr> },
}
pub struct NativeTransport {
    endpoint: Endpoint,
    client: reqwest::Client,
    trust: Trust,
    resolution: Resolution,
}
impl NativeTransport {
    pub fn new(endpoint: Endpoint, trust: Trust, limits: &Limits) -> Result<Self> {
        Self::with_resolution(endpoint, trust, Resolution::System, limits)
    }
    pub fn with_resolution(
        endpoint: Endpoint,
        trust: Trust,
        resolution: Resolution,
        limits: &Limits,
    ) -> Result<Self> {
        limits.validate()?;
        let builder = reqwest::Client::builder();
        #[cfg(feature = "tls")]
        let builder = builder.tls_backend_preconfigured(tls_config(&trust)?);
        #[cfg(not(feature = "tls"))]
        if endpoint.0.scheme() == "https" {
            return Err(Failure::new(
                FailureKind::Request,
                "HTTPS endpoints require the tls feature",
            ));
        }
        let mut builder = builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(limits.connect_timeout);
        if let Resolution::Pinned { addresses } = &resolution {
            match (endpoint.0.domain(), endpoint.ip_literal()) {
                (Some(domain), _) if (1..=16).contains(&addresses.len()) => {
                    let port = endpoint
                        .0
                        .port_or_known_default()
                        .expect("HTTP(S) endpoint");
                    let addresses: Vec<_> = addresses
                        .iter()
                        .map(|ip| SocketAddr::new(*ip, port))
                        .collect();
                    builder = builder.resolve_to_addrs(domain, &addresses);
                }
                // Nothing resolves an IP literal, so it can only be pinned to itself.
                (None, Some(ip)) if addresses[..] == [ip] => {}
                _ => {
                    return Err(Failure::new(
                        FailureKind::Request,
                        "pinned resolution requires a DNS hostname and 1..16 explicit IP addresses, or an IP literal pinned to itself",
                    ));
                }
            }
        }
        let client = builder
            .build()
            .map_err(|e| Failure::new(FailureKind::Transport, e.to_string()))?;
        Ok(Self {
            endpoint,
            client,
            trust,
            resolution,
        })
    }
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub fn trust(&self) -> &Trust {
        &self.trust
    }
    pub fn resolution(&self) -> &Resolution {
        &self.resolution
    }
}
#[cfg(feature = "tls")]
fn tls_config(trust: &Trust) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    match trust {
        Trust::BundledWebPki => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Trust::PemOnly { certificates } => {
            use rustls::pki_types::{CertificateDer, pem::PemObject};
            if certificates.is_empty()
                || certificates.len() > 16
                || certificates
                    .iter()
                    .try_fold(0_usize, |size, pem| size.checked_add(pem.len()))
                    .is_none_or(|size| size > 65_536)
            {
                return Err(Failure::new(
                    FailureKind::Request,
                    "explicit TLS roots exceed frozen bounds",
                ));
            }
            for pem in certificates {
                let certificate = CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|_| {
                    Failure::new(FailureKind::Request, "invalid explicit TLS certificate")
                })?;
                roots
                    .add(certificate)
                    .map_err(|_| Failure::new(FailureKind::Request, "invalid explicit TLS root"))?;
            }
        }
    }
    Ok(
        rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| Failure::new(FailureKind::Request, e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}
#[async_trait::async_trait]
impl Transport for NativeTransport {
    async fn send(&self, request: Request) -> Result<HttpResponse> {
        let mut builder = self
            .client
            .request(
                request.operation.method(),
                self.endpoint.url(request.operation),
            )
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::CONNECTION, "close");
        // Hyper owns the socket driver. Fence its body producer when this
        // request future is dropped, rather than handing it a full replayable
        // buffer that can continue uploading after local cancellation. Bytes
        // already on the wire remain a potentially running remote effect.
        let upload_cancel = crate::CancellationToken::new();
        let _upload_guard = upload_cancel.clone().drop_guard();
        if let Some(body) = request.body {
            let length = body.len();
            let upload = futures_util::stream::unfold(
                (body, 0, upload_cancel),
                |(body, offset, cancel)| async move {
                    tokio::task::yield_now().await;
                    if cancel.is_cancelled() || offset == body.len() {
                        return None;
                    }
                    let end = offset.saturating_add(16 * 1024).min(body.len());
                    let chunk: std::io::Result<Vec<u8>> = Ok(body[offset..end].to_vec());
                    Some((chunk, (body, end, cancel)))
                },
            );
            builder = builder
                .header(reqwest::header::CONTENT_LENGTH, length)
                .body(reqwest::Body::wrap_stream(upload));
        }
        let response = builder
            .send()
            .await
            .map_err(|e| Failure::new(FailureKind::Transport, e.to_string()))?;
        let status = response.status().as_u16();
        let encodings: Vec<_> = response
            .headers()
            .get_all(reqwest::header::CONTENT_ENCODING)
            .iter()
            .map(|v| v.to_str().unwrap_or("invalid"))
            .collect();
        let content_encoding = (!encodings.is_empty()).then(|| encodings.join(","));
        let body = Box::pin(response.bytes_stream().map(|result| {
            result
                .map(|b| b.to_vec())
                .map_err(|e| Failure::new(FailureKind::Transport, e.to_string()))
        }));
        Ok(HttpResponse {
            status,
            content_encoding,
            body,
        })
    }
}
