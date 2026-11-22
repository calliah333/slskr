use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes};
use futures::future::{BoxFuture, ready};
use soulseek::client::{DownloadOutput, UploadInput};
use soulseek::resolvers::DefaultResolvers;
use soulseek::{
    BrowseOptions, BrowseResponse, ClientOptions, Directory, File, Resolvers, Result,
    SearchOptions, SearchQuery, SearchResponse, SearchScope, SearchState, SoulseekClient,
    TransferDirection, TransferOptions, TransferState,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const DOWNLOADER: &str = "downloader";
const UPLOADER: &str = "uploader";
const DIRECTORY: &str = "shared";
const BASENAME: &str = "resume sample.bin";
const REMOTE_FILE: &str = "shared\\resume sample.bin";
const QUERY: &str = "resume sample";
const CONTENT: &[u8] = b"existing prefix\x00\x01\x02\xff\x80remainder\x00";
const OFFSET: usize = 16;
const SEARCH_TOKEN: u32 = 234;
const DOWNLOAD_TOKEN: u32 = 345;
const UPLOAD_TOKEN: u32 = 456;
const MINOR_VERSION: u32 = 17;

struct Account {
    username: &'static str,
    password: &'static str,
    digest: &'static str,
    endpoint: SocketAddr,
}

// The central server uses the reference wire layout directly, without importing the
// library codec. Browse, search replies, and P/F transfer negotiation use the
// two real client listeners, never the server socket.
async fn read_body(socket: &mut (impl AsyncRead + Unpin)) -> Bytes {
    let size = socket.read_u32_le().await.expect("server frame length");
    assert!(
        (4..=4096).contains(&size),
        "invalid server frame length: {size}"
    );
    let mut body = vec![0; size as usize];
    socket
        .read_exact(&mut body)
        .await
        .expect("server frame body");
    body.into()
}

fn read_string(body: &mut Bytes) -> String {
    let size = body.get_u32_le() as usize;
    String::from_utf8(body.split_to(size).to_vec()).expect("fixture wire string")
}

fn write_string(body: &mut Vec<u8>, text: &str) {
    body.put_u32_le(text.len() as u32);
    body.extend_from_slice(text.as_bytes());
}

fn write_loopback(body: &mut Vec<u8>) {
    body.extend_from_slice(&[1, 0, 0, 127]); // IPv4 octets in reverse order.
}

fn frame(code: u32) -> Vec<u8> {
    let mut bytes = vec![0; 4]; // Length is filled in just before sending.
    bytes.put_u32_le(code);
    bytes
}

async fn write_frame(socket: &mut (impl tokio::io::AsyncWrite + Unpin), mut bytes: Vec<u8>) {
    let length = (bytes.len() - 4) as u32;
    bytes[..4].copy_from_slice(&length.to_le_bytes());
    socket
        .write_all(&bytes)
        .await
        .expect("writing server frame");
}

async fn accept_login(server: &TcpListener, account: &Account) -> TcpStream {
    let (mut socket, remote) = server.accept().await.unwrap();
    assert!(remote.ip().is_loopback());
    let mut login = read_body(&mut socket).await;
    assert_eq!(login.get_u32_le(), 1); // Login
    assert_eq!(read_string(&mut login), account.username);
    assert_eq!(read_string(&mut login), account.password);
    assert_eq!(login.get_u32_le(), 170);
    assert_eq!(read_string(&mut login), account.digest);
    assert_eq!(login.get_u32_le(), MINOR_VERSION);
    assert!(login.is_empty());
    let mut listen = read_body(&mut socket).await;
    assert_eq!(listen.get_u32_le(), 2); // SetListenPort
    assert_eq!(listen.get_u32_le(), u32::from(account.endpoint.port()));
    assert!(listen.is_empty());

    let mut reply = frame(1);
    reply.put_u8(1); // Login succeeded.
    write_string(&mut reply, "Welcome to the loopback fixture");
    write_loopback(&mut reply);
    write_string(&mut reply, account.digest);
    reply.put_u8(1); // Supporter.
    write_frame(&mut socket, reply).await;
    socket
}

async fn serve_session(
    mut reader: OwnedReadHalf,
    own_writer: &Mutex<OwnedWriteHalf>,
    peer_writer: &Mutex<OwnedWriteHalf>,
    account: &Account,
    peer: &Account,
) {
    loop {
        let mut request = read_body(&mut reader).await;
        match request.get_u32_le() {
            3 => {
                // GetPeerAddress: both clients must resolve the other listener via the server.
                assert_eq!(read_string(&mut request), peer.username);
                assert!(request.is_empty());
                let mut reply = frame(3);
                write_string(&mut reply, peer.username);
                write_loopback(&mut reply);
                reply.put_u32_le(u32::from(peer.endpoint.port()));
                write_frame(&mut *own_writer.lock().await, reply).await;
            }
            18 => {
                // Refuse the optional reverse solicitation so the test requires direct P/F.
                let token = request.get_u32_le();
                assert_eq!(read_string(&mut request), peer.username);
                assert!(matches!(read_string(&mut request).as_str(), "P" | "F"));
                assert!(request.is_empty());
                let mut reply = frame(1001); // CannotConnect
                reply.put_u32_le(token);
                write_string(&mut reply, peer.username);
                write_frame(&mut *own_writer.lock().await, reply).await;
            }
            26 => {
                // FileSearch: deliver the search to the other client as a distributed
                // SearchRequest inside the central server's EmbeddedMessage.
                assert_eq!(account.username, DOWNLOADER);
                let token = request.get_u32_le();
                let query = read_string(&mut request);
                assert_eq!(token, SEARCH_TOKEN);
                assert_eq!(query, QUERY);
                assert!(request.is_empty());
                let mut forwarded = frame(93);
                forwarded.put_u8(3); // Distributed SearchRequest
                forwarded.put_u32_le(0); // Reserved field.
                write_string(&mut forwarded, account.username);
                forwarded.put_u32_le(token);
                write_string(&mut forwarded, &query);
                write_frame(&mut *peer_writer.lock().await, forwarded).await;
            }
            // Presence, distributed status, and private-room preference reports.
            28 | 71 | 100 | 126 | 127 | 129 | 141 => {}
            code => panic!("unexpected server message {code} from {}", account.username),
        }
    }
}

async fn reference_server(server: TcpListener, downloader: &Account, uploader: &Account) {
    let (download_read, download_write) = accept_login(&server, downloader).await.into_split();
    let (upload_read, upload_write) = accept_login(&server, uploader).await.into_split();
    let download_write = Mutex::new(download_write);
    let upload_write = Mutex::new(upload_write);
    tokio::join!(
        serve_session(
            download_read,
            &download_write,
            &upload_write,
            downloader,
            uploader
        ),
        serve_session(
            upload_read,
            &upload_write,
            &download_write,
            uploader,
            downloader
        ),
    );
}

fn file(filename: &str) -> File {
    File {
        code: 1,
        filename: filename.into(),
        size: CONTENT.len() as u64,
        extension: "bin".into(),
        attributes: vec![],
    }
}

fn listing() -> BrowseResponse {
    BrowseResponse {
        directories: vec![Directory {
            name: DIRECTORY.into(),
            files: vec![file(BASENAME)],
        }],
        ..BrowseResponse::default()
    }
}

fn matching_share() -> SearchResponse {
    SearchResponse {
        files: vec![file(REMOTE_FILE)],
        has_free_upload_slot: true,
        upload_speed: 32_768,
        ..SearchResponse::default()
    }
}

struct UploadShare(Mutex<Option<oneshot::Sender<(String, String)>>>);

impl Resolvers for UploadShare {
    fn browse(
        &self,
        username: String,
        endpoint: SocketAddr,
    ) -> BoxFuture<'static, Result<BrowseResponse>> {
        assert_eq!(username, DOWNLOADER);
        assert!(endpoint.ip().is_loopback());
        Box::pin(ready(Ok(listing())))
    }

    fn search(
        &self,
        username: String,
        token: u32,
        query: SearchQuery,
    ) -> BoxFuture<'static, Option<SearchResponse>> {
        assert_eq!(username, DOWNLOADER);
        assert_eq!(token, SEARCH_TOKEN);
        assert_eq!(query.search_text, QUERY);
        Box::pin(ready(Some(matching_share())))
    }

    fn enqueue_download(
        &self,
        username: String,
        endpoint: SocketAddr,
        filename: String,
    ) -> BoxFuture<'static, Result<()>> {
        assert!(endpoint.ip().is_loopback());
        let queued = self
            .0
            .try_lock()
            .unwrap()
            .take()
            .expect("one download request");
        queued.send((username, filename)).unwrap();
        Box::pin(ready(Ok(())))
    }
}

struct TransferDirectory(PathBuf);

impl TransferDirectory {
    async fn create() -> Self {
        let path =
            std::env::temp_dir().join(format!("soulseek-interop-{:016x}", rand::random::<u64>()));
        tokio::fs::create_dir(&path).await.unwrap();
        Self(path)
    }
}

impl Drop for TransferDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn client(account: &Account, resolvers: impl Resolvers) -> SoulseekClient {
    SoulseekClient::new(
        ClientOptions {
            minor_version: MINOR_VERSION,
            listen_ip_address: account.endpoint.ip(),
            listen_port: Some(account.endpoint.port()),
            ..ClientOptions::default()
        },
        Arc::new(resolvers),
    )
}

async fn connect(client: &SoulseekClient, port: u16, account: &Account, reserved: TcpListener) {
    // The client requires an explicit port. Keep both ephemeral allocations
    // reserved until each corresponding client is ready to bind its listener.
    drop(reserved);
    client
        .connect("127.0.0.1", port, account.username, account.password)
        .await
        .expect("loopback login");
    assert!(client.state().is_logged_in());
    assert_eq!(client.username().as_deref(), Some(account.username));
    assert_eq!(client.server_info().is_supporter, Some(true));
}

#[tokio::test]
async fn real_clients_log_in_browse_find_and_resume_a_file() {
    timeout(Duration::from_secs(15), async {
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let download_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let upload_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = server.local_addr().unwrap().port();
        let download_account = Account {
            username: DOWNLOADER,
            password: "download-test-password",
            digest: "1fd427228debd5a9c30703d462527202",
            endpoint: download_listener.local_addr().unwrap(),
        };
        let upload_account = Account {
            username: UPLOADER,
            password: "upload-test-password",
            digest: "6d6fde923f7a21aa477ab03b29ba6e42",
            endpoint: upload_listener.local_addr().unwrap(),
        };
        let (queued, enqueued) = oneshot::channel();
        let downloader = client(&download_account, DefaultResolvers);
        let uploader = client(&upload_account, UploadShare(Mutex::new(Some(queued))));
        let directory = TransferDirectory::create().await;
        let source = directory.0.join("source.bin");
        let destination = directory.0.join("partial.bin");
        tokio::fs::write(&source, CONTENT).await.unwrap();
        tokio::fs::write(&destination, &CONTENT[..OFFSET])
            .await
            .unwrap();

        let scenario = async {
            connect(&downloader, port, &download_account, download_listener).await;
            connect(&uploader, port, &upload_account, upload_listener).await;

            let shares = downloader
                .browse(UPLOADER, BrowseOptions::default(), CancellationToken::new())
                .await
                .expect("direct browse");
            assert_eq!(shares, listing());

            let (search, results) = downloader
                .search_collect(
                    SearchQuery::parse(QUERY),
                    SearchScope::network(),
                    Some(SEARCH_TOKEN),
                    SearchOptions {
                        response_limit: 1,
                        ..SearchOptions::default()
                    },
                    CancellationToken::new(),
                )
                .await
                .expect("forwarded search");
            assert_eq!(search.token, SEARCH_TOKEN);
            assert_eq!(
                search.state,
                SearchState::COMPLETED | SearchState::RESPONSE_LIMIT_REACHED
            );
            assert_eq!((search.response_count, search.file_count), (1, 1));
            assert_eq!(
                results,
                vec![SearchResponse {
                    username: UPLOADER.into(),
                    token: SEARCH_TOKEN,
                    ..matching_share()
                }]
            );

            let result = &results[0];
            let found = &result.files[0];
            let download = downloader
                .enqueue_download(
                    &result.username,
                    &found.filename,
                    DownloadOutput::File(destination.clone()),
                    Some(found.size),
                    OFFSET as u64,
                    Some(DOWNLOAD_TOKEN),
                    TransferOptions::default(),
                    CancellationToken::new(),
                )
                .await
                .expect("remote queue acknowledgment");
            // The download handle is returned only after the legacy Queued reply.
            // Start the actual upload from the request delivered to the resolver.
            let (username, filename) = enqueued.await.expect("upload queue request");
            assert_eq!((&*username, &*filename), (DOWNLOADER, REMOTE_FILE));
            let upload = uploader
                .enqueue_upload(
                    &username,
                    &filename,
                    UploadInput::File(source),
                    Some(UPLOAD_TOKEN),
                    TransferOptions::default(),
                    CancellationToken::new(),
                )
                .await
                .expect("enqueue upload");
            let (download, upload) = tokio::join!(download.wait(), upload.wait());
            let download = download.expect("resumed download completed");
            let upload = upload.expect("resumed upload completed");
            for transfer in [&download, &upload] {
                assert_eq!(
                    transfer.state,
                    TransferState::COMPLETED | TransferState::SUCCEEDED
                );
                assert_eq!(transfer.filename, REMOTE_FILE);
                assert_eq!(transfer.start_offset, OFFSET as u64);
                assert_eq!(transfer.size, CONTENT.len() as u64);
                assert_eq!(transfer.bytes_transferred, CONTENT.len() as u64);
            }
            assert_eq!(
                (download.direction, upload.direction),
                (TransferDirection::Download, TransferDirection::Upload)
            );
            assert_eq!(
                (download.token, download.remote_token, upload.token),
                (DOWNLOAD_TOKEN, Some(UPLOAD_TOKEN), UPLOAD_TOKEN)
            );
            assert_eq!(upload.ip_endpoint, Some(download_account.endpoint)); // Direct F socket.
            assert_eq!(tokio::fs::read(destination).await.unwrap(), CONTENT);
        };
        tokio::select! {
            biased;
            () = scenario => {},
            () = reference_server(server, &download_account, &upload_account) => {
                panic!("reference server stopped before the scenario ended");
            },
        }
        downloader.disconnect("test complete", None).await;
        uploader.disconnect("test complete", None).await;
    })
    .await
    .expect("loopback Soulseek scenario timed out");
}
