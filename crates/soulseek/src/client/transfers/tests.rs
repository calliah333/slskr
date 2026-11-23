use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Buf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};

use super::*;
use crate::client::TransferInput;
use crate::net::test_support::{FakeServer, read_frame};
use crate::options::ClientOptions;
use crate::protocol::codec::MessageBuilder;
use crate::protocol::codes::ServerCode;
use crate::protocol::init::{InitIncoming, PierceFirewall};
use crate::resolvers::DefaultResolvers;

const PEER: &str = "remote";
const FILE: &str = "share\\fixture.bin";
const FIREWALL_TOKEN: u32 = 700;

async fn with_peer<F: Future<Output = ()>>(
    scenario: impl FnOnce(SoulseekClient, FakeServer, mpsc::UnboundedSender<()>) -> F,
) {
    let server = FakeServer::bind().await;
    let peer = FakeServer::bind().await;
    let port = server.addr().port();
    let peer_endpoint = peer.addr();
    let client = SoulseekClient::new(
        ClientOptions {
            enable_listener: false,
            enable_distributed_network: false,
            ..ClientOptions::default()
        },
        Arc::new(DefaultResolvers),
    );
    let complete = CancellationToken::new();
    let (reverse, mut reverse_requested) = mpsc::unbounded_channel();
    let central = async {
        let mut socket = server.accept().await;
        assert_eq!(
            read_frame(&mut socket).await.get_u32_le(),
            ServerCode::Login as u32
        );
        assert_eq!(
            read_frame(&mut socket).await.get_u32_le(),
            ServerCode::SetListenPort as u32
        );
        let login = MessageBuilder::new()
            .code_u32(ServerCode::Login as u32)
            .bool(true)
            .string("")
            .ip(Ipv4Addr::LOCALHOST)
            .string("")
            .bool(true)
            .build();
        socket.write_all(&login).await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let (addresses, mut address_requested) = mpsc::unbounded_channel();
        let incoming = async {
            loop {
                // This future stays alive if another branch needs to send a notice.
                let length = reader.read_u32_le().await.unwrap() as usize;
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                if body[..4] == (ServerCode::GetPeerAddress as u32).to_le_bytes() {
                    addresses.send(()).unwrap();
                }
            }
        };
        let outgoing = async {
            loop {
                let response = tokio::select! {
                    Some(()) = address_requested.recv() => MessageBuilder::new()
                        .code_u32(ServerCode::GetPeerAddress as u32)
                        .string(PEER)
                        .ip(Ipv4Addr::LOCALHOST)
                        .u32(u32::from(peer_endpoint.port()))
                        .build(),
                    Some(()) = reverse_requested.recv() => MessageBuilder::new()
                        .code_u32(ServerCode::ConnectToPeer as u32)
                        .string(PEER)
                        .string("F")
                        .ip(Ipv4Addr::LOCALHOST)
                        .u32(u32::from(peer_endpoint.port()))
                        .u32(FIREWALL_TOKEN)
                        .build(),
                };
                writer.write_all(&response).await.unwrap();
            }
        };
        tokio::select! {
            biased;
            _ = complete.cancelled() => {},
            _ = incoming => {},
            _ = outgoing => {},
        }
    };
    let caller = async {
        client
            .connect("127.0.0.1", port, "local", "password")
            .await
            .unwrap();
        scenario(client, peer, reverse).await;
        complete.cancel();
    };
    timeout(Duration::from_secs(5), async {
        tokio::join!(central, caller);
    })
    .await
    .unwrap();
}

async fn accept_init(peer: &FakeServer, kind: &str) -> TcpStream {
    let mut socket = peer.accept().await;
    let InitIncoming::PeerInit(init) = InitIncoming::decode(read_frame(&mut socket).await).unwrap()
    else {
        panic!("peer did not receive PeerInit");
    };
    assert_eq!(init.username, "local");
    assert_eq!(init.connection_type, kind);
    socket
}

async fn request(socket: &mut TcpStream) -> TransferRequest {
    let PeerIncoming::TransferRequest(request) =
        PeerIncoming::decode(read_frame(socket).await).unwrap()
    else {
        panic!("peer did not receive TransferRequest");
    };
    assert_eq!(request.filename, FILE);
    request
}

struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn queued_download_resumes_a_file_and_limits_output_to_the_reported_size() {
    with_peer(|client, peer, reverse| async move {
        let enqueued = Notify::new();
        let output = TempPath(std::env::temp_dir().join(format!("soulseek-transfer-{:016x}", rand::random::<u64>())));
        tokio::fs::write(&output.0, b"hello obsolete").await.unwrap();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let downloader = async {
            let options = TransferOptions {
                governor: Some(Arc::new(|_, attempted| Box::pin(async move { attempted.min(2) }))),
                reporter: Some(Arc::new({
                    let reports = reports.clone();
                    move |_, attempted, granted, actual| reports.lock().unwrap().push((attempted, granted, actual))
                })),
                ..TransferOptions::default()
            };
            let handle = client.enqueue_download(
                PEER, FILE, DownloadOutput::File(output.0.clone()), None, 6, Some(100), options, CancellationToken::new(),
            ).await.unwrap();
            enqueued.notify_one();
            let transfer = handle.wait().await.unwrap();
            assert_eq!(transfer.state, State::COMPLETED | State::SUCCEEDED);
            assert_eq!(transfer.remote_token, Some(42));
            assert_eq!(transfer.start_offset, 6);
            assert_eq!(transfer.bytes_transferred, 11);
            assert_eq!(tokio::fs::read(&output.0).await.unwrap(), b"hello world");
            let reports = reports.lock().unwrap();
            assert_eq!(reports.iter().map(|&(_, _, actual)| actual).sum::<usize>(), 5);
            assert!(reports.iter().all(|&(attempted, granted, actual)| actual <= granted && granted <= attempted.min(2)));
        };
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            assert_eq!(incoming.direction, Direction::Download);
            assert_eq!(incoming.token, 100);
            message.write_all(&TransferResponse::rejected(incoming.token, "Queued.").to_bytes()).await.unwrap();
            enqueued.notified().await;
            let offer = TransferRequest { direction: Direction::Upload, token: 42, filename: FILE.into(), file_size: 11 }.to_bytes();
            message.write_all(&offer).await.unwrap();
            message.write_all(&offer).await.unwrap();
            assert!(matches!(
                PeerIncoming::decode(read_frame(&mut message).await).unwrap(),
                PeerIncoming::TransferResponse(response) if response == TransferResponse::allowed(42, Some(11))
            ));
            reverse.send(()).unwrap();
            let mut file = peer.accept().await;
            assert_eq!(InitIncoming::decode(read_frame(&mut file).await).unwrap(), InitIncoming::PierceFirewall(PierceFirewall { token: FIREWALL_TOKEN }));
            // Deliberately pipeline some payload behind the raw token to exercise buffered handoff.
            let mut first = [0; 6];
            first[..4].copy_from_slice(&42_u32.to_le_bytes());
            first[4..].copy_from_slice(b"wo");
            file.write_all(&first).await.unwrap();
            assert_eq!(file.read_u64_le().await.unwrap(), 6);
            file.write_all(b"rldextra").await.unwrap();
        };
        tokio::join!(downloader, remote);
    }).await;
}

#[tokio::test]
async fn upload_enqueues_and_honors_a_full_width_resume_offset_without_seeking() {
    with_peer(|client, peer, _| async move {
        let offset = u64::from(u32::MAX) + 7;
        let uploader = async {
            let source = UploadInput::Stream {
                size: offset + 3,
                factory: Box::new(move |requested| {
                    Box::pin(async move {
                        assert_eq!(requested, offset);
                        Ok(Box::new(Cursor::new(b"newextra")) as Box<dyn TransferInput>)
                    })
                }),
            };
            let options = TransferOptions {
                seek_input_stream_automatically: false,
                ..TransferOptions::default()
            };
            let handle = client
                .enqueue_upload(
                    PEER,
                    FILE,
                    source,
                    Some(200),
                    options,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let transfer = handle.wait().await.unwrap();
            assert_eq!(transfer.state, State::COMPLETED | State::SUCCEEDED);
            assert_eq!(transfer.start_offset, offset);
            assert_eq!(transfer.bytes_transferred, offset + 3);
        };
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            assert_eq!(incoming.direction, Direction::Upload);
            assert_eq!(incoming.file_size, offset + 3);
            message
                .write_all(&TransferResponse::allowed(incoming.token, None).to_bytes())
                .await
                .unwrap();
            let mut file = accept_init(&peer, "F").await;
            assert_eq!(file.read_u32_le().await.unwrap(), 200);
            file.write_u64_le(offset).await.unwrap();
            let mut data = [0; 3];
            file.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"new");
            assert!(
                timeout(Duration::from_millis(20), file.read_u8())
                    .await
                    .is_err()
            );
            file.shutdown().await.unwrap();
        };
        tokio::join!(uploader, remote);
    })
    .await;
}

#[tokio::test]
async fn download_reports_eof_instead_of_succeeding_with_partial_data() {
    with_peer(|client, peer, _| async move {
        let downloader = async {
            let output = DownloadOutput::Stream(Box::new(|| Box::pin(async {
                Ok(Box::new(Cursor::new(Vec::<u8>::new())) as Box<dyn crate::client::TransferOutput>)
            })));
            let handle = client.enqueue_download(PEER, FILE, output, Some(3), 0, None, TransferOptions::default(), CancellationToken::new()).await.unwrap();
            assert!(matches!(handle.wait().await, Err(Error::Connection(message)) if message.contains("after 2 of 3 bytes")));
        };
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            message.write_all(&TransferResponse::allowed(incoming.token, Some(3)).to_bytes()).await.unwrap();
            let mut file = accept_init(&peer, "F").await;
            assert_eq!(file.read_u32_le().await.unwrap(), incoming.token);
            assert_eq!(file.read_u64_le().await.unwrap(), 0);
            file.write_all(b"no").await.unwrap();
            file.shutdown().await.unwrap();
        };
        tokio::join!(downloader, remote);
    }).await;
}

#[tokio::test]
async fn upload_seeks_before_reporting_an_early_end_of_source() {
    with_peer(|client, peer, _| async move {
        let uploader = async {
            let source = UploadInput::Stream {
                size: 6,
                factory: Box::new(|offset| Box::pin(async move {
                    assert_eq!(offset, 3);
                    Ok(Box::new(Cursor::new(b"oldok")) as Box<dyn TransferInput>)
                })),
            };
            let handle = client.enqueue_upload(
                PEER, FILE, source, None, TransferOptions::default(), CancellationToken::new(),
            ).await.unwrap();
            assert!(matches!(handle.wait().await, Err(Error::TransferStream(message)) if message.contains("after 5 of 6 bytes")));
        };
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            message.write_all(&TransferResponse::allowed(incoming.token, None).to_bytes()).await.unwrap();
            let mut file = accept_init(&peer, "F").await;
            assert_eq!(file.read_u32_le().await.unwrap(), incoming.token);
            file.write_u64_le(3).await.unwrap();
            let mut data = Vec::new();
            file.read_to_end(&mut data).await.unwrap();
            assert_eq!(data, b"ok");
        };
        tokio::join!(uploader, remote);
    }).await;
}

#[tokio::test]
async fn stalled_download_uses_the_transfer_socket_idle_timeout() {
    with_peer(|client, peer, _| async move {
        client
            .inner
            .options
            .write()
            .unwrap()
            .transfer_connection_options
            .inactivity_timeout = Some(Duration::from_millis(100));
        let timed_out = Notify::new();
        let downloader = async {
            let output = DownloadOutput::Stream(Box::new(|| {
                Box::pin(async {
                    Ok(Box::new(Cursor::new(Vec::<u8>::new()))
                        as Box<dyn crate::client::TransferOutput>)
                })
            }));
            let handle = client
                .enqueue_download(
                    PEER,
                    FILE,
                    output,
                    Some(3),
                    0,
                    None,
                    TransferOptions::default(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(matches!(handle.wait().await, Err(Error::Timeout(_))));
            timed_out.notify_one();
        };
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            message
                .write_all(&TransferResponse::allowed(incoming.token, Some(3)).to_bytes())
                .await
                .unwrap();
            let mut file = accept_init(&peer, "F").await;
            assert_eq!(file.read_u32_le().await.unwrap(), incoming.token);
            assert_eq!(file.read_u64_le().await.unwrap(), 0);
            timed_out.notified().await;
            assert_eq!(file.read(&mut [0]).await.unwrap(), 0);
        };
        tokio::join!(downloader, remote);
    })
    .await;
}

#[tokio::test]
async fn re_offer_for_an_active_download_is_not_rejected() {
    with_peer(|client, peer, reverse| async move {
        let enqueued = Notify::new();
        let downloader = async {
            let output = DownloadOutput::Stream(Box::new(|| {
                Box::pin(async {
                    Ok(Box::new(Cursor::new(Vec::<u8>::new()))
                        as Box<dyn crate::client::TransferOutput>)
                })
            }));
            let handle = client
                .enqueue_download(
                    PEER,
                    FILE,
                    output,
                    Some(3),
                    0,
                    Some(100),
                    TransferOptions::default(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            enqueued.notify_one();
            let transfer = handle.wait().await.unwrap();
            assert_eq!(transfer.state, State::COMPLETED | State::SUCCEEDED);
            assert_eq!(transfer.remote_token, Some(42));
        };
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            message
                .write_all(&TransferResponse::rejected(incoming.token, "Queued").to_bytes())
                .await
                .unwrap();
            enqueued.notified().await;
            let offer = |token| {
                TransferRequest {
                    direction: Direction::Upload,
                    token,
                    filename: FILE.into(),
                    file_size: 3,
                }
                .to_bytes()
            };
            message.write_all(&offer(42)).await.unwrap();
            assert!(matches!(
                PeerIncoming::decode(read_frame(&mut message).await).unwrap(),
                PeerIncoming::TransferResponse(response) if response == TransferResponse::allowed(42, Some(3))
            ));
            // A re-offer under a new token while the download is still wanted must not be
            // answered with "Cancelled" (upstream only rejects unknown downloads).
            message.write_all(&offer(43)).await.unwrap();
            reverse.send(()).unwrap();
            let mut file = peer.accept().await;
            assert_eq!(
                InitIncoming::decode(read_frame(&mut file).await).unwrap(),
                InitIncoming::PierceFirewall(PierceFirewall {
                    token: FIREWALL_TOKEN
                })
            );
            file.write_all(&42_u32.to_le_bytes()).await.unwrap();
            assert_eq!(file.read_u64_le().await.unwrap(), 0);
            file.write_all(b"abc").await.unwrap();
            assert!(
                timeout(Duration::from_millis(200), message.read(&mut [0; 1]))
                    .await
                    .is_err(),
                "no reply may be sent for the re-offer"
            );
        };
        tokio::join!(downloader, remote);
    })
    .await;
}

#[tokio::test]
async fn download_slot_is_held_from_before_the_request() {
    with_peer(|client, peer, _| async move {
        client
            .inner
            .options
            .write()
            .unwrap()
            .maximum_concurrent_downloads = 1;
        let output = || {
            DownloadOutput::Stream(Box::new(|| {
                Box::pin(async {
                    Ok(Box::new(Cursor::new(Vec::<u8>::new()))
                        as Box<dyn crate::client::TransferOutput>)
                })
            }))
        };
        let first = client.enqueue_download(
            PEER,
            FILE,
            output(),
            Some(3),
            0,
            Some(100),
            TransferOptions::default(),
            CancellationToken::new(),
        );
        let remote = async {
            let mut message = accept_init(&peer, "P").await;
            let incoming = request(&mut message).await;
            message
                .write_all(&TransferResponse::rejected(incoming.token, "Queued").to_bytes())
                .await
                .unwrap();
            message
        };
        let (first, mut message) = tokio::join!(first, remote);
        let _first = first.unwrap();
        // The remotely queued download holds the only slot, so the next one is not requested.
        let cancel = CancellationToken::new();
        let second = client.enqueue_download(
            PEER,
            "share\\other.bin",
            output(),
            Some(3),
            0,
            Some(101),
            TransferOptions::default(),
            cancel.clone(),
        );
        let probe = async {
            assert!(
                timeout(Duration::from_millis(200), message.read(&mut [0; 1]))
                    .await
                    .is_err(),
                "the second download must wait for a slot before requesting"
            );
            cancel.cancel();
        };
        let (second, ()) = tokio::join!(second, probe);
        assert!(matches!(second, Err(Error::Cancelled)));
    })
    .await;
}
