//! Passive FTP and FTPS uploads for completed downloads.

use std::{
    net::SocketAddr,
    path::{Component, Path},
    pin::Pin,
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use cap_std::{ambient_authority, fs::Dir};
use openssl::{
    error::ErrorStack,
    ex_data::Index,
    ssl::{Ssl, SslConnector, SslMethod, SslSession, SslSessionCacheMode, SslVerifyMode},
};
use slskr_api::options::FtpOptions;
use tokio::{
    fs::File,
    io::{
        AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::TcpStream,
    time::{Instant, sleep, timeout, timeout_at},
};
use tokio_openssl::SslStream;

const IO_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REPLY_BYTES: usize = 64 * 1024;

pub async fn upload(options: &FtpOptions, download_root: &str, filename: &str) -> Result<()> {
    let address = options
        .address
        .as_deref()
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
        .context("FTP address is not configured")?;
    let host = address
        .strip_prefix('[')
        .and_then(|addr| addr.strip_suffix(']'))
        .unwrap_or(address);
    validate_argument(host)?;
    let port = u16::try_from(options.port)
        .ok()
        .filter(|&p| p != 0)
        .context("invalid FTP port")?;
    let mode = Mode::parse(&options.encryption_mode)?;
    let username = options.username.as_deref().unwrap_or("anonymous");
    let password = options.password.as_deref().unwrap_or("");
    validate_argument(username)?;
    validate_argument(password)?;
    let connection_timeout = Duration::from_millis(
        u64::try_from(options.connection_timeout).context("invalid FTP connection timeout")?,
    );
    let path = Path::new(filename);
    let remote = RemotePath::new(&options.remote_path, path)?;
    let tls = if mode == Mode::None {
        None
    } else {
        Some(Tls::new(host, options.ignore_certificate_errors)?)
    };
    let attempts = options.retry_attempts;
    ensure!(attempts > 0, "no FTP upload attempts configured");

    let root = download_root.to_owned();
    let path = path.to_owned();
    let mut file = File::from_std(
        tokio::task::spawn_blocking(move || open_download(Path::new(&root), &path)).await??,
    );
    let upload = Upload {
        host,
        port,
        mode,
        username,
        password,
        connection_timeout,
        remote,
        overwrite: options.overwrite_existing,
        tls,
    };
    for attempt in 1..=attempts {
        match upload.attempt(&mut file).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt == attempts => {
                return Err(error.context(format!("FTP upload failed after {attempt} attempts")));
            }
            Err(error) => {
                tracing::info!(attempt, error = %format_args!("{error:#}"), "FTP upload attempt failed")
            }
        }
        let seconds = ((1u64 << (attempt - 1).min(5)) - 1).min(30);
        sleep(Duration::from_millis(
            seconds * 1000 + rand::random_range(0..1000),
        ))
        .await;
        file.rewind().await.context("rewind completed download")?;
    }
    unreachable!("positive attempt count always returns a result")
}

fn open_download(root: &Path, filename: &Path) -> Result<std::fs::File> {
    ensure!(
        !root.as_os_str().is_empty() && !filename.as_os_str().is_empty(),
        "missing FTP download path"
    );
    let configured = std::path::absolute(root)?;
    let canonical = std::fs::canonicalize(&configured).context("resolve downloads directory")?;
    let filename = std::path::absolute(filename)?;
    // Transfer completion can emit a path under the canonical downloads directory.
    let relative = filename
        .strip_prefix(&configured)
        .or_else(|_| filename.strip_prefix(&canonical))
        .context("FTP source is outside the downloads directory")?;
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "invalid FTP download path"
    );
    let root = Dir::open_ambient_dir(&configured, ambient_authority())
        .context("open downloads directory")?;
    // Capability-relative open also confines symlink resolution during concurrent renames.
    ensure!(
        root.symlink_metadata(relative)?.is_file(),
        "FTP source is not a regular download file"
    );
    let file = root.open(relative).context("open completed download")?;
    ensure!(
        file.metadata()?.is_file(),
        "FTP source is not a regular download file"
    );
    Ok(file.into_std())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Explicit,
    Implicit,
    Auto,
}

impl Mode {
    fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "explicit" => Ok(Self::Explicit),
            "implicit" => Ok(Self::Implicit),
            "auto" => Ok(Self::Auto),
            _ => bail!("invalid FTP encryption mode"),
        }
    }
}

struct Tls<'a> {
    host: &'a str,
    connector: SslConnector,
    session_index: Index<Ssl, SslSession>,
    ignore_certificate_errors: bool,
}

impl<'a> Tls<'a> {
    fn new(host: &'a str, ignore_certificate_errors: bool) -> Result<Self> {
        static SESSION_INDEX: LazyLock<std::result::Result<Index<Ssl, SslSession>, ErrorStack>> =
            LazyLock::new(Ssl::new_ex_index);
        let session_index = *SESSION_INDEX.as_ref().map_err(|error| {
            anyhow::anyhow!("could not allocate FTP TLS session index: {error}")
        })?;
        let mut builder = SslConnector::builder(SslMethod::tls_client())?;
        if ignore_certificate_errors {
            builder.set_verify(SslVerifyMode::NONE);
        }
        // Some FTPS servers require the data connection to resume the control TLS session.
        // The callback also captures TLS 1.3 tickets received after the handshake.
        builder.set_session_cache_mode(
            SslSessionCacheMode::CLIENT | SslSessionCacheMode::NO_INTERNAL_STORE,
        );
        builder
            .set_new_session_callback(move |ssl, session| ssl.set_ex_data(session_index, session));
        Ok(Self {
            host,
            connector: builder.build(),
            session_index,
            ignore_certificate_errors,
        })
    }

    async fn connect(
        &self,
        tcp: TcpStream,
        control: Option<&SslStream<TcpStream>>,
    ) -> Result<SslStream<TcpStream>> {
        let mut config = self.connector.configure()?;
        config.set_verify_hostname(!self.ignore_certificate_errors);
        let mut ssl = config.into_ssl(self.host)?;
        if let Some(session) = control.and_then(|stream| stream.ssl().ex_data(self.session_index)) {
            // SAFETY: the data and control sessions use this same connector and TLS identity.
            unsafe {
                ssl.set_session(session)?;
            }
        }
        let mut stream = SslStream::new(ssl, tcp)?;
        Pin::new(&mut stream)
            .connect()
            .await
            .context("FTP TLS handshake failed")?;
        Ok(stream)
    }
}

struct RemotePath<'a> {
    root: &'a str,
    parent: Option<&'a str>,
    name: &'a str,
}

impl<'a> RemotePath<'a> {
    fn new(root: &'a str, path: &'a Path) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid FTP source filename")?;
        validate_component(name)?;
        let parent = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_str().context("invalid FTP source parent name"))
            .transpose()?;
        if let Some(parent) = parent {
            validate_component(parent)?;
        }
        validate_argument(root)?;
        for part in root.split(['/', '\\']).filter(|part| !part.is_empty()) {
            validate_component(part)?;
        }
        Ok(Self { root, parent, name })
    }
}

fn validate_component(value: &str) -> Result<()> {
    ensure!(
        !matches!(value, "" | "." | "..") && !value.contains(['/', '\\']),
        "invalid FTP path component"
    );
    validate_argument(value)
}

fn validate_argument(value: &str) -> Result<()> {
    ensure!(
        !value.bytes().any(|b| b.is_ascii_control()),
        "FTP argument contains a control character"
    );
    Ok(())
}

struct Upload<'a> {
    host: &'a str,
    port: u16,
    mode: Mode,
    username: &'a str,
    password: &'a str,
    connection_timeout: Duration,
    remote: RemotePath<'a>,
    overwrite: bool,
    tls: Option<Tls<'a>>,
}

impl Upload<'_> {
    async fn attempt(&self, file: &mut File) -> Result<()> {
        let deadline = Instant::now() + self.connection_timeout;
        let (socket, peer, tls) = timeout_at(deadline, async {
            let tcp = TcpStream::connect((self.host, self.port))
                .await
                .context("connect to FTP server")?;
            let peer = tcp.peer_addr()?;
            let mut control = Control::new(tcp);
            if self.mode == Mode::Implicit {
                return Ok::<_, anyhow::Error>((control, peer, true));
            }
            control.greet().await?;
            let tls = if self.mode == Mode::None {
                false
            } else {
                let reply = control.command("AUTH", Some("TLS")).await?;
                match reply.code {
                    234 => true,
                    // Auto falls back only for unsupported AUTH TLS, never a failed handshake or certificate check.
                    500 | 502 | 504 if self.mode == Mode::Auto => false,
                    _ => {
                        reply.expect("AUTH TLS", &[234])?;
                        true
                    }
                }
            };
            if !tls {
                self.login(&mut control, false).await?;
            }
            Ok((control, peer, tls))
        })
        .await
        .context("FTP connection timed out")??;
        if !tls {
            let mut control = socket;
            self.finish_upload(&mut control, peer, file, None).await
        } else {
            let tls = self.tls.as_ref().context("FTP TLS is not configured")?;
            // The client sends the first TLS message, so a buffered tail here is invalid.
            ensure!(
                socket.stream.buffer().is_empty(),
                "unexpected bytes before FTP TLS handshake"
            );
            let mut control = timeout_at(deadline, async {
                let mut control =
                    Control::new(tls.connect(socket.stream.into_inner(), None).await?);
                if self.mode == Mode::Implicit {
                    control.greet().await?;
                }
                self.login(&mut control, true).await?;
                Ok::<_, anyhow::Error>(control)
            })
            .await
            .context("FTP connection timed out")??;
            self.finish_upload(&mut control, peer, file, Some(tls))
                .await
        }
    }

    async fn login<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        control: &mut Control<S>,
        tls: bool,
    ) -> Result<()> {
        let reply = control.command("USER", Some(self.username)).await?;
        if reply.code == 331 {
            control
                .command("PASS", Some(self.password))
                .await?
                .expect("PASS", &[230, 202])?;
        } else {
            reply.expect("USER", &[230])?;
        }
        if tls {
            control
                .command("PBSZ", Some("0"))
                .await?
                .expect("PBSZ", &[200])?;
            control
                .command("PROT", Some("P"))
                .await?
                .expect("PROT", &[200])?;
        }
        Ok(())
    }

    async fn finish_upload<S: Transport>(
        &self,
        control: &mut Control<S>,
        mut peer: SocketAddr,
        file: &mut File,
        tls: Option<&Tls<'_>>,
    ) -> Result<()> {
        control
            .command("TYPE", Some("I"))
            .await?
            .expect("TYPE", &[200])?;
        let root = self.remote.root;
        if root.is_empty() || root.starts_with(['/', '\\']) {
            control
                .command("CWD", Some("/"))
                .await?
                .expect("CWD", &[250])?;
        }
        for part in root
            .split(['/', '\\'])
            .filter(|part| !part.is_empty())
            .chain(self.remote.parent)
        {
            // Existing directories commonly give 550 to MKD; successful CWD is authoritative.
            control
                .command("MKD", Some(part))
                .await?
                .expect("MKD", &[257, 550])?;
            control
                .command("CWD", Some(part))
                .await?
                .expect("CWD", &[250])?;
        }
        if !self.overwrite && control.file_exists(self.remote.name).await? {
            control.quit().await;
            return Ok(());
        }

        // A dual-stack listener may report IPv4 peers as IPv4-mapped IPv6 addresses.
        if let SocketAddr::V6(address) = peer {
            if let Some(ip) = address.ip().to_ipv4_mapped() {
                peer = SocketAddr::from((ip, address.port()));
            }
        }
        peer.set_port(control.passive_port(peer.is_ipv4()).await?);
        let mut data = timeout(IO_TIMEOUT, TcpStream::connect(peer))
            .await
            .context("FTP data connection timed out")?
            .context("connect FTP data stream")?;
        control
            .command("STOR", Some(self.remote.name))
            .await?
            .expect("STOR", &[125, 150])?;
        if let Some(tls) = tls {
            let mut data = timeout(
                IO_TIMEOUT,
                tls.connect(data, control.stream.get_ref().tls_stream()),
            )
            .await
            .context("FTP data TLS handshake timed out")??;
            send_file(file, &mut data).await?;
        } else {
            send_file(file, &mut data).await?;
            drop(data);
        }
        control
            .reply()
            .await?
            .expect("upload completion", &[226, 250])?;
        control.quit().await;
        Ok(())
    }
}

trait Transport: AsyncRead + AsyncWrite + Unpin {
    fn tls_stream(&self) -> Option<&SslStream<TcpStream>> {
        None
    }
}
impl Transport for TcpStream {}
impl Transport for SslStream<TcpStream> {
    fn tls_stream(&self) -> Option<&SslStream<TcpStream>> {
        Some(self)
    }
}

async fn send_file(file: &mut File, data: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .context("read completed download")?;
        if n == 0 {
            break;
        }
        timeout(IO_TIMEOUT, data.write_all(&buf[..n]))
            .await
            .context("FTP data write timed out")??;
    }
    timeout(IO_TIMEOUT, data.shutdown())
        .await
        .context("FTP data shutdown timed out")??;
    Ok(())
}

struct Control<S> {
    stream: BufReader<S>,
    line: Vec<u8>,
}

struct Reply<'a> {
    code: u16,
    line: &'a [u8],
}

impl Reply<'_> {
    fn expect(self, command: &str, accepted: &[u16]) -> Result<()> {
        // A server can echo credentials in any reply, including multiline errors.
        ensure!(
            accepted.contains(&self.code),
            "FTP {command} returned status {}",
            self.code
        );
        Ok(())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Control<S> {
    fn new(stream: S) -> Self {
        Self {
            stream: BufReader::new(stream),
            line: Vec::new(),
        }
    }

    async fn greet(&mut self) -> Result<()> {
        let reply = self.reply().await?;
        if reply.code == 120 {
            self.reply().await?.expect("greeting", &[220])
        } else {
            reply.expect("greeting", &[220])
        }
    }

    async fn file_exists(&mut self, name: &str) -> Result<bool> {
        let reply = self.command("SIZE", Some(name)).await?;
        match reply.code {
            213 => Ok(true),
            550 => Ok(false),
            500 | 501 | 502 | 504 => {
                let reply = self.command("MLST", Some(name)).await?;
                if reply.code == 550 {
                    return Ok(false);
                }
                reply.expect("MLST", &[250])?;
                Ok(true)
            }
            _ => {
                reply.expect("SIZE", &[213])?;
                Ok(true)
            }
        }
    }

    async fn passive_port(&mut self, ipv4: bool) -> Result<u16> {
        let reply = self.command("EPSV", None).await?;
        if reply.code == 229 {
            return epsv_port(reply.line);
        }
        if ipv4 && matches!(reply.code, 500 | 501 | 502 | 504 | 522) {
            let reply = self.command("PASV", None).await?;
            let line = reply.line;
            reply.expect("PASV", &[227])?;
            return pasv_port(line);
        }
        bail!("FTP EPSV returned status {}", reply.code)
    }

    async fn quit(&mut self) {
        let _ = self.write_command("QUIT", None).await;
    }

    async fn command(&mut self, verb: &'static str, arg: Option<&str>) -> Result<Reply<'_>> {
        self.write_command(verb, arg).await?;
        self.reply().await
    }

    async fn write_command(&mut self, verb: &'static str, arg: Option<&str>) -> Result<()> {
        self.line.clear();
        self.line.extend_from_slice(verb.as_bytes());
        if let Some(arg) = arg {
            validate_argument(arg)?;
            self.line.push(b' ');
            self.line.extend_from_slice(arg.as_bytes());
        }
        self.line.extend_from_slice(b"\r\n");
        timeout(IO_TIMEOUT, self.stream.get_mut().write_all(&self.line))
            .await
            .context("FTP command timed out")?
            .context("write FTP command")
    }

    async fn reply(&mut self) -> Result<Reply<'_>> {
        let code = timeout(IO_TIMEOUT, self.read_reply())
            .await
            .context("FTP reply timed out")??;
        Ok(Reply {
            code,
            line: &self.line[..self.line.len() - 2],
        })
    }

    async fn read_reply(&mut self) -> Result<u16> {
        let mut remaining = MAX_REPLY_BYTES;
        self.read_line(&mut remaining).await?;
        let [
            a @ b'1'..=b'5',
            b @ b'0'..=b'9',
            c @ b'0'..=b'9',
            separator @ (b' ' | b'-'),
            ..,
        ] = self.line.as_slice()
        else {
            bail!("invalid FTP reply header");
        };
        let code = u16::from(*a - b'0') * 100 + u16::from(*b - b'0') * 10 + u16::from(*c - b'0');
        if *separator == b'-' {
            let end = [*a, *b, *c, b' '];
            loop {
                self.read_line(&mut remaining).await?;
                if self.line.starts_with(&end) {
                    break;
                }
            }
        }
        Ok(code)
    }

    async fn read_line(&mut self, remaining: &mut usize) -> Result<()> {
        self.line.clear();
        ensure!(*remaining > 0, "FTP reply exceeds size limit");
        let n = (&mut self.stream)
            .take(*remaining as u64)
            .read_until(b'\n', &mut self.line)
            .await?;
        *remaining -= n;
        ensure!(n != 0, "FTP server closed the control connection");
        ensure!(
            self.line.ends_with(b"\r\n"),
            "unterminated or oversized FTP reply line"
        );
        Ok(())
    }
}

fn epsv_port(line: &[u8]) -> Result<u16> {
    let params = reply_parameters(line).context("invalid FTP EPSV response")?;
    let [delimiter, second, third, port @ .., last] = params else {
        bail!("invalid FTP EPSV response");
    };
    ensure!(
        (b'!'..=b'~').contains(delimiter)
            && delimiter == second
            && delimiter == third
            && delimiter == last,
        "invalid FTP EPSV delimiter"
    );
    parse_number::<u16>(port)
        .filter(|&p| p != 0)
        .context("invalid FTP EPSV port")
}

fn pasv_port(line: &[u8]) -> Result<u16> {
    let params = reply_parameters(line)
        .or_else(|| {
            // RFC 1123 allows old servers to omit parentheses around the address tuple.
            let message = line.get(4..)?;
            let start = message.iter().position(u8::is_ascii_digit)?;
            let number = &message[start..];
            let end = number
                .iter()
                .position(|&c| !(c.is_ascii_digit() || c == b','))
                .unwrap_or(number.len());
            Some(&number[..end])
        })
        .context("invalid FTP PASV response")?;
    let mut fields = params.split(|&c| c == b',');
    let mut component = || {
        fields
            .next()
            .and_then(parse_number::<u8>)
            .context("invalid FTP PASV tuple")
    };
    // Validate the address syntax but never connect to a host supplied by PASV.
    for _ in 0..4 {
        component()?;
    }
    let port = u16::from_be_bytes([component()?, component()?]);
    ensure!(
        port != 0 && fields.next().is_none(),
        "invalid FTP PASV port"
    );
    Ok(port)
}

fn reply_parameters(line: &[u8]) -> Option<&[u8]> {
    let open = line.iter().position(|&c| c == b'(')?;
    let close = line.iter().rposition(|&c| c == b')')?;
    line.get(open + 1..close)
}

fn parse_number<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_valid_passive_ports() {
        assert_eq!(
            epsv_port(b"229 Entering Extended Passive Mode (|||49152|)").unwrap(),
            49152
        );
        assert_eq!(epsv_port(b"229 (!!!65535!)").unwrap(), 65535);
        assert!(epsv_port(b"229 (|1|49152|)").is_err());
        assert!(epsv_port(b"229 (|||65536|)").is_err());
        assert!(epsv_port(b"229 (|||0|)").is_err());

        assert_eq!(
            pasv_port(b"227 Entering Passive Mode (192,0,2,1,192,0)").unwrap(),
            49152
        );
        assert_eq!(
            pasv_port(b"227 Passive Mode 192,0,2,1,192,0.").unwrap(),
            49152
        );
        assert!(pasv_port(b"227 (192,0,256,1,192,0)").is_err());
        assert!(pasv_port(b"227 (192,0,2,1,192,0,1)").is_err());
        assert!(pasv_port(b"227 (192,0,2,1,0,0)").is_err());
    }

    #[tokio::test]
    async fn reply_requires_matching_terminator_and_never_displays_server_text() {
        let wire = &b"530-authentication failed\r\n230 false terminator\r\n530 actual-secret\r\n220 ready\r\n"[..];
        let (mut server, client) = tokio::io::duplex(wire.len());
        server.write_all(wire).await.unwrap();
        let mut control = Control::new(client);
        let error = control
            .reply()
            .await
            .unwrap()
            .expect("PASS", &[230])
            .unwrap_err();
        assert_eq!(error.to_string(), "FTP PASS returned status 530");
        control.greet().await.unwrap();
    }

    #[tokio::test]
    async fn reply_size_limit_applies_across_all_lines() {
        let mut wire = b"220-welcome\r\n".to_vec();
        for _ in 0..=MAX_REPLY_BYTES / 3 {
            wire.extend_from_slice(b" \r\n");
        }
        let (mut server, client) = tokio::io::duplex(wire.len());
        server.write_all(&wire).await.unwrap();
        let mut control = Control::new(client);
        assert!(control.reply().await.is_err());
    }

    #[test]
    fn confines_ftp_source_to_download_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("downloads");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("track.flac");
        std::fs::write(&file, b"audio").unwrap();
        let outside = temp.path().join("outside.flac");
        std::fs::write(&outside, b"outside").unwrap();
        assert!(open_download(&root, &file).is_ok());
        assert!(open_download(&root, &root).is_err());
        assert!(open_download(&root, &outside).is_err());
        assert!(open_download(&root, &root.join("../outside.flac")).is_err());
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(temp.path(), &link).unwrap();
            assert!(open_download(&root, &link.join("outside.flac")).is_err());
        }
    }

    #[test]
    fn rejects_ftp_command_injection() {
        assert!(validate_argument("secret\r\nSTOR file").is_err());
        assert!(validate_argument("invalid\0argument").is_err());
        assert!(RemotePath::new("/uploads/../other", Path::new("/downloads/record.flac")).is_err());
        assert!(RemotePath::new("/uploads", Path::new("/downloads/recording\nSTOR file")).is_err());
    }
}
