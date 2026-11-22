use std::{net::SocketAddr, path::Path, pin::Pin, time::Duration};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    extract::Request,
    http::{HeaderValue, StatusCode, header, uri::Authority},
    response::IntoResponse,
    serve::Listener,
};
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    hash::MessageDigest,
    pkcs12::Pkcs12,
    pkey::{PKey, Private},
    rsa::Rsa,
    ssl::{Ssl, SslAcceptor, SslMethod},
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
    },
};
use slskr_api::options::CertificateOptions;
use tokio::net::{TcpListener, TcpStream};
use tokio_openssl::SslStream;

const MAX_HANDSHAKES: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

type Connection = (SslStream<TcpStream>, SocketAddr);

pub(super) struct TlsListener {
    tcp: TcpListener,
    acceptor: SslAcceptor,
    pending: FuturesUnordered<BoxFuture<'static, Result<Connection>>>,
}

impl TlsListener {
    pub(super) fn new(tcp: TcpListener, acceptor: SslAcceptor) -> Self {
        Self {
            tcp,
            acceptor,
            pending: FuturesUnordered::new(),
        }
    }
}

impl Listener for TlsListener {
    type Io = SslStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> Connection {
        loop {
            let has_pending = !self.pending.is_empty();
            let can_accept = self.pending.len() < MAX_HANDSHAKES;
            tokio::select! {
                handshake = self.pending.next(), if has_pending => {
                    match handshake.expect("pending TLS handshake") {
                        Ok(connection) => return connection,
                        Err(error) => tracing::debug!(%error, "TLS handshake failed"),
                    }
                }
                (tcp, addr) = Listener::accept(&mut self.tcp), if can_accept => {
                    let ssl = match Ssl::new(self.acceptor.context()) {
                        Ok(ssl) => ssl,
                        Err(error) => { tracing::warn!(%error, "could not create TLS session"); continue; }
                    };
                    self.pending.push(Box::pin(async move {
                        let mut stream = SslStream::new(ssl, tcp)?;
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, Pin::new(&mut stream).accept()).await??;
                        Ok((stream, addr))
                    }));
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

pub(super) fn acceptor(options: &CertificateOptions) -> Result<SslAcceptor> {
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
    let (key, certificate) =
        if let Some(path) = options.pfx.as_deref().filter(|path| !path.is_empty()) {
            let archive =
                std::fs::read(path).with_context(|| format!("reading TLS certificate {path}"))?;
            let archive = Pkcs12::from_der(&archive)?
                .parse2(options.password.as_deref().unwrap_or_default())
                .context("opening configured TLS PKCS#12 certificate")?;
            if let Some(chain) = archive.ca {
                for certificate in chain {
                    acceptor.add_extra_chain_cert(certificate)?;
                }
            }
            (
                archive
                    .pkey
                    .context("TLS certificate archive is missing a private key")?,
                archive
                    .cert
                    .context("TLS certificate archive is missing its leaf certificate")?,
            )
        } else {
            self_signed()?
        };
    acceptor.set_private_key(&key)?;
    acceptor.set_certificate(&certificate)?;
    acceptor.check_private_key()?;
    Ok(acceptor.build())
}

pub(super) fn generate_pfx(path: &Path, password: &str) -> Result<()> {
    use std::{fs::OpenOptions, io::Write};
    let (key, certificate) = self_signed()?;
    let bytes = Pkcs12::builder()
        .name("slskd")
        .pkey(&key)
        .cert(&certificate)
        .build2(password)?
        .to_der()?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&bytes)?;
    Ok(())
}

fn self_signed() -> Result<(PKey<Private>, X509)> {
    let key = PKey::from_rsa(Rsa::generate(2048)?)?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", "localhost")?;
    let name = name.build();
    let mut serial = BigNum::new()?;
    serial.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let mut certificate = X509::builder()?;
    certificate.set_version(2)?;
    let serial = serial.to_asn1_integer()?;
    certificate.set_serial_number(&serial)?;
    certificate.set_subject_name(&name)?;
    certificate.set_issuer_name(&name)?;
    certificate.set_pubkey(&key)?;
    let now = Asn1Time::days_from_now(0)?;
    let expiry = Asn1Time::days_from_now(365)?;
    certificate.set_not_before(&now)?;
    certificate.set_not_after(&expiry)?;
    certificate.append_extension(BasicConstraints::new().critical().build()?)?;
    certificate.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;
    certificate.append_extension(ExtendedKeyUsage::new().server_auth().build()?)?;
    let alt = SubjectAlternativeName::new()
        .dns("localhost")
        .ip("127.0.0.1")
        .ip("::1")
        .build(&certificate.x509v3_context(None, None))?;
    certificate.append_extension(alt)?;
    certificate.sign(&key, MessageDigest::sha256())?;
    Ok((key, certificate.build()))
}

pub(super) fn redirect(port: u16) -> Router {
    Router::new().fallback(move |request: Request| async move {
        match https_location(&request, port) {
            Ok(location) => (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response(),
            Err(_) => StatusCode::BAD_REQUEST.into_response(),
        }
    })
}

fn https_location(request: &Request, port: u16) -> Result<HeaderValue> {
    let authority = request
        .uri()
        .authority()
        .cloned()
        .or_else(|| {
            request
                .headers()
                .get(header::HOST)?
                .to_str()
                .ok()?
                .parse::<Authority>()
                .ok()
        })
        .filter(|authority| !authority.as_str().contains('@'))
        .ok_or_else(|| anyhow!("missing or invalid host"))?;
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    let host = authority.host();
    let location = if port == 443 {
        format!("https://{host}{path}")
    } else {
        format!("https://{host}:{port}{path}")
    };
    Ok(location.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_keeps_ipv6_path_and_query() {
        let request = axum::http::Request::builder()
            .uri("/client/api/v0/application?x=1")
            .header(header::HOST, "[::1]:5030")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            https_location(&request, 8443).unwrap(),
            "https://[::1]:8443/client/api/v0/application?x=1"
        );
        assert_eq!(
            https_location(&request, 443).unwrap(),
            "https://[::1]/client/api/v0/application?x=1"
        );
    }

    #[tokio::test]
    async fn serves_https_using_configured_pfx() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.pfx");
        generate_pfx(&file, "test-password").unwrap();
        let tls = acceptor(&CertificateOptions {
            pfx: Some(file.to_str().unwrap().into()),
            password: Some("test-password".into()),
        })
        .unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        super::super::serve(
            &mut tasks,
            TlsListener::new(tcp, tls),
            Router::new().route("/health", axum::routing::get(|| async { "ready" })),
            token.clone(),
        );
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .get(format!("https://{addr}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ready");
        drop(client);
        token.cancel();
        tasks.join_next().await.unwrap().unwrap().unwrap();
    }
}
