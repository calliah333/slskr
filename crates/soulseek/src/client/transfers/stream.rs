use std::io::{self, SeekFrom};
use std::time::Duration;

use bytes::Buf;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use super::{
    ActiveTransfer, Connection, Direction, DownloadOutput, Error, Result, State, UploadInput,
};
use crate::net::rate::{RateLimiter, Reservation};

pub(super) async fn download(
    transfer: &mut ActiveTransfer,
    conn: Connection,
    output: DownloadOutput,
) -> Result<()> {
    let offset = transfer.snapshot.start_offset;
    match output {
        DownloadOutput::File(path) => {
            let mut file = OpenOptions::new()
                .write(true)
                .create(offset == 0)
                .truncate(offset == 0)
                .open(path)
                .await?;
            let existing = if offset == 0 {
                0
            } else {
                file.metadata().await?.len()
            };
            if existing < offset {
                return Err(Error::TransferStream(format!(
                    "Download file has {existing} bytes, fewer than the resume offset {offset}"
                )));
            }
            if offset != 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            receive_data(transfer, conn, &mut file).await?;
            if existing > transfer.snapshot.size {
                file.set_len(transfer.snapshot.size).await?;
            }
            Ok(())
        }
        DownloadOutput::Stream(factory) => {
            let mut output = factory().await?;
            if offset != 0 && transfer.options.seek_output_stream_automatically {
                output
                    .seek(SeekFrom::Start(offset))
                    .await
                    .map_err(stream_error)?;
            }
            receive_data(transfer, conn, &mut output).await
        }
    }
}

async fn receive_data(
    transfer: &mut ActiveTransfer,
    mut conn: Connection,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let offset = transfer.snapshot.start_offset;
    conn.write_all(&offset.to_le_bytes()).await?;
    let (mut socket, mut buffered) = conn.into_parts();
    let (idle, chunk_size) = settings(transfer, Direction::Download);
    let client = transfer.client.clone();
    let mut buffer = vec![
        0;
        chunk_size.min(
            (transfer.snapshot.size - offset)
                .try_into()
                .unwrap_or(usize::MAX)
        )
    ];
    transfer.state(State::IN_PROGRESS);
    transfer.progress(offset);

    while transfer.snapshot.bytes_transferred < transfer.snapshot.size {
        let attempted = remaining_chunk(transfer, buffer.len());
        let mut reservation = grant(transfer, &client.inner.download_limiter, attempted).await;
        let granted = reservation.remaining();
        let bytes = if buffered.is_empty() {
            let count = socket_io(idle, socket.read(&mut buffer[..granted])).await?;
            &buffer[..count]
        } else {
            &buffered[..buffered.len().min(granted)]
        };
        let actual = bytes.len();
        reservation.consume(actual);
        drop(reservation);
        if actual != 0 {
            output.write_all(bytes).await.map_err(stream_error)?;
            if !buffered.is_empty() {
                buffered.advance(actual);
            }
        }
        report(transfer, attempted, granted, actual);
        if actual == 0 {
            return Err(Error::Connection(format!(
                "Peer closed the download after {} of {} bytes",
                transfer.snapshot.bytes_transferred, transfer.snapshot.size
            )));
        }
    }
    output.flush().await.map_err(stream_error)
}

pub(super) async fn upload(
    transfer: &mut ActiveTransfer,
    mut conn: Connection,
    source: UploadInput,
) -> Result<()> {
    let mut raw_offset = [0; 8];
    conn.read_exact(&mut raw_offset)
        .await
        .map_err(|error| match error {
            Error::Connection(message) => Error::Connection(format!(
                "Could not read the upload resume offset from the peer: {message}"
            )),
            error => error,
        })?;
    let offset = u64::from_le_bytes(raw_offset);
    let size = transfer.snapshot.size;
    if offset > size {
        return Err(Error::Transfer(format!(
            "Requested start offset of {offset} bytes exceeds file length of {size} bytes"
        )));
    }
    transfer.snapshot.start_offset = offset;
    match source {
        UploadInput::File(path) => {
            let mut file = File::open(path).await?;
            if offset != 0 {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            transmit_data(transfer, conn, file).await
        }
        UploadInput::Stream { factory, .. } => {
            let mut input = factory(offset).await?;
            if offset != 0 && transfer.options.seek_input_stream_automatically {
                input
                    .seek(SeekFrom::Start(offset))
                    .await
                    .map_err(stream_error)?;
            }
            transmit_data(transfer, conn, input).await
        }
    }
}

async fn transmit_data(
    transfer: &mut ActiveTransfer,
    conn: Connection,
    mut input: impl AsyncRead + Unpin,
) -> Result<()> {
    let size = transfer.snapshot.size;
    let offset = transfer.snapshot.start_offset;
    let (mut socket, _) = conn.into_parts();
    let (idle, chunk_size) = settings(transfer, Direction::Upload);
    let client = transfer.client.clone();
    let mut buffer = vec![0; chunk_size.min((size - offset).try_into().unwrap_or(usize::MAX))];
    transfer.state(State::IN_PROGRESS);
    transfer.progress(offset);

    while transfer.snapshot.bytes_transferred < size {
        let attempted = remaining_chunk(transfer, buffer.len());
        let mut reservation = grant(transfer, &client.inner.upload_limiter, attempted).await;
        let granted = reservation.remaining();
        let actual = input
            .read(&mut buffer[..granted])
            .await
            .map_err(stream_error)?;
        let mut sent = 0;
        while sent < actual {
            let count = socket_io(idle, socket.write(&buffer[sent..actual])).await?;
            if count == 0 {
                return Err(Error::Connection(
                    "Peer socket stopped accepting upload data".into(),
                ));
            }
            sent += count;
            reservation.consume(count);
            transfer.progress(transfer.snapshot.bytes_transferred + count as u64);
        }
        drop(reservation);
        if let Some(reporter) = &transfer.options.reporter {
            reporter(&transfer.snapshot, attempted, granted, actual);
        }
        if actual == 0 {
            return Err(Error::TransferStream(format!(
                "Upload source ended after {} of {size} bytes",
                transfer.snapshot.bytes_transferred
            )));
        }
    }
    drop(input);
    linger(&mut socket, transfer.options.maximum_linger_time).await;
    Ok(())
}

fn settings(transfer: &ActiveTransfer, direction: Direction) -> (Option<Duration>, usize) {
    let options = transfer
        .client
        .inner
        .options
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let conn = &options.transfer_connection_options;
    let chunk_size = match direction {
        Direction::Download => conn.read_buffer_size,
        Direction::Upload => conn.write_buffer_size,
    };
    (conn.inactivity_timeout, chunk_size.max(1))
}

fn remaining_chunk(transfer: &ActiveTransfer, capacity: usize) -> usize {
    (transfer.snapshot.size - transfer.snapshot.bytes_transferred).min(capacity as u64) as usize
}

async fn grant<'a>(
    transfer: &ActiveTransfer,
    limiter: &'a RateLimiter,
    attempted: usize,
) -> Reservation<'a> {
    let requested = if let Some(governor) = &transfer.options.governor {
        loop {
            let count = governor(&transfer.snapshot, attempted).await.min(attempted);
            if count != 0 {
                break count;
            }
            // A governor may temporarily deny all bytes. Avoid spinning if it returns immediately.
            sleep(Duration::from_millis(1)).await;
        }
    } else {
        attempted
    };
    limiter.acquire(requested).await
}

fn report(transfer: &mut ActiveTransfer, attempted: usize, granted: usize, actual: usize) {
    if actual != 0 {
        transfer.progress(transfer.snapshot.bytes_transferred + actual as u64);
    }
    if let Some(reporter) = &transfer.options.reporter {
        reporter(&transfer.snapshot, attempted, granted, actual);
    }
}

async fn socket_io<T>(
    idle: Option<Duration>,
    io: impl Future<Output = io::Result<T>>,
) -> Result<T> {
    let result = match idle {
        Some(limit) => timeout(limit, io).await.map_err(|_| {
            Error::Timeout(format!(
                "Transfer stalled: the peer moved no data for {} s",
                limit.as_secs()
            ))
        })?,
        None => io.await,
    };
    result.map_err(|error| Error::Connection(format!("Transfer socket: {error}")))
}

fn stream_error(error: io::Error) -> Error {
    Error::TransferStream(error.to_string())
}

async fn linger(socket: &mut TcpStream, limit: Duration) {
    if limit.is_zero() {
        return;
    }
    let _ = timeout(limit, async {
        let mut scratch = [0; 256];
        while let Ok(1..) = socket.read(&mut scratch).await {}
    })
    .await;
}
