//! Peer messages.

use bytes::Bytes;

use super::codec::{MessageBuilder, MessageReader, ProtocolError};
use super::codes::PeerCode;
use crate::model::{
    BrowseResponse, Directory, File, FileAttribute, SearchResponse, TransferDirection, UserInfo,
};

fn builder(code: PeerCode) -> MessageBuilder {
    let mut b = MessageBuilder::new();
    b.code_u32(code.as_u32());
    b
}

/// Writes a file entry.
pub fn write_file(b: &mut MessageBuilder, file: &File) {
    b.u8(file.code)
        .string(&file.filename)
        .u64(file.size)
        .string(&file.extension)
        .u32(file.attributes.len() as u32);
    for a in &file.attributes {
        b.u32(a.kind.into()).u32(a.value);
    }
}

/// Reads a file entry.
pub fn read_file(r: &mut MessageReader) -> Result<File, ProtocolError> {
    let code = r.u8()?;
    let filename = r.string()?;
    let mut size = r.u64()?;
    // Some clients write a sign-extended 32-bit size; recover the low half.
    if size >> 32 == 0xFFFF_FFFF {
        size &= 0xFFFF_FFFF;
    }
    let extension = r.string()?;
    let count = r.u32()?;
    let mut attributes = Vec::with_capacity(count.min(64) as usize);
    for _ in 0..count {
        let kind = r.u32()?.into();
        let value = r.u32()?;
        attributes.push(FileAttribute { kind, value });
    }
    Ok(File {
        code,
        filename,
        size,
        extension,
        attributes,
    })
}

/// Maximum file entries parsed from one message (all directories and lists combined).
pub const MAX_PARSED_FILES: usize = 5_000_000;
/// Maximum directory entries parsed from one message (all lists combined).
pub const MAX_PARSED_DIRECTORIES: usize = 1_000_000;

/// Per-message budget bounding how many entries a decompressed payload may expand into.
#[derive(Debug, Clone, Copy)]
struct EntryBudget {
    files: usize,
    directories: usize,
}

impl EntryBudget {
    const fn new() -> Self {
        Self {
            files: MAX_PARSED_FILES,
            directories: MAX_PARSED_DIRECTORIES,
        }
    }

    fn take_files(&mut self, count: u32) -> Result<(), ProtocolError> {
        self.files = self.files.checked_sub(count as usize).ok_or_else(|| {
            ProtocolError::Invalid(format!(
                "message exceeds the limit of {MAX_PARSED_FILES} file entries"
            ))
        })?;
        Ok(())
    }

    fn take_directories(&mut self, count: u32) -> Result<(), ProtocolError> {
        self.directories = self
            .directories
            .checked_sub(count as usize)
            .ok_or_else(|| {
                ProtocolError::Invalid(format!(
                    "message exceeds the limit of {MAX_PARSED_DIRECTORIES} directory entries"
                ))
            })?;
        Ok(())
    }
}

fn read_files(
    r: &mut MessageReader,
    count: u32,
    budget: &mut EntryBudget,
) -> Result<Vec<File>, ProtocolError> {
    budget.take_files(count)?;
    let mut files = Vec::with_capacity(count.min(1024) as usize);
    for _ in 0..count {
        files.push(read_file(r)?);
    }
    Ok(files)
}

/// Writes a directory entry.
pub fn write_directory(b: &mut MessageBuilder, dir: &Directory) {
    b.string(&dir.name).u32(dir.files.len() as u32);
    for f in &dir.files {
        write_file(b, f);
    }
}

/// Reads a directory entry.
pub fn read_directory(r: &mut MessageReader) -> Result<Directory, ProtocolError> {
    read_directory_budgeted(r, &mut EntryBudget::new())
}

fn read_directory_budgeted(
    r: &mut MessageReader,
    budget: &mut EntryBudget,
) -> Result<Directory, ProtocolError> {
    let name = r.string()?;
    let count = r.u32()?;
    Ok(Directory {
        name,
        files: read_files(r, count, budget)?,
    })
}

fn read_directories(
    r: &mut MessageReader,
    count: u32,
    budget: &mut EntryBudget,
) -> Result<Vec<Directory>, ProtocolError> {
    budget.take_directories(count)?;
    let mut dirs = Vec::with_capacity(count.min(1024) as usize);
    for _ in 0..count {
        dirs.push(read_directory_budgeted(r, budget)?);
    }
    Ok(dirs)
}

/// Requests a peer's shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BrowseRequest;

impl BrowseRequest {
    pub fn to_bytes(&self) -> Bytes {
        builder(PeerCode::BrowseRequest).build()
    }

    pub fn decode(_r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self)
    }
}

impl BrowseResponse {
    /// Encodes as a compressed SharesReply.
    pub fn to_bytes(&self) -> Bytes {
        self.payload().compress().build()
    }

    /// Encodes as a SharesReply at the highest compression level: slower, a few
    /// percent smaller. For a frame that is cached and sent to many peers.
    pub fn to_bytes_best(&self) -> Bytes {
        self.payload().compress_best().build()
    }

    fn payload(&self) -> MessageBuilder {
        let mut b = builder(PeerCode::BrowseResponse);
        b.u32(self.directories.len() as u32);
        for d in &self.directories {
            write_directory(&mut b, d);
        }
        b.u32(0).u32(self.locked_directories.len() as u32);
        for d in &self.locked_directories {
            write_directory(&mut b, d);
        }
        b
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        r.decompress()?;
        let count = r.u32()?;
        let mut budget = EntryBudget::new();
        let directories = read_directories(r, count, &mut budget)?;
        let mut locked_directories = Vec::new();
        if r.has_remaining() {
            r.u32()?;
            if r.has_remaining() {
                let count = r.u32()?;
                locked_directories = read_directories(r, count, &mut budget)?;
            }
        }
        Ok(Self {
            directories,
            locked_directories,
        })
    }
}

/// Search request sent directly by a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSearchRequest {
    pub token: u32,
    pub query: String,
}

impl PeerSearchRequest {
    pub fn to_bytes(&self) -> Bytes {
        builder(PeerCode::SearchRequest)
            .u32(self.token)
            .string(&self.query)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            token: r.u32()?,
            query: r.string()?,
        })
    }
}

impl SearchResponse {
    /// Encodes as a compressed FileSearchResponse.
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(PeerCode::SearchResponse);
        b.string(&self.username)
            .u32(self.token)
            .u32(self.files.len() as u32);
        for f in &self.files {
            write_file(&mut b, f);
        }
        b.bool(self.has_free_upload_slot)
            .u32(self.upload_speed)
            .u32(self.queue_length.min(i32::MAX as u64) as u32)
            .u32(0)
            .u32(self.locked_files.len() as u32);
        for f in &self.locked_files {
            write_file(&mut b, f);
        }
        b.compress().build()
    }

    /// Decodes; the trailing unknown `u32` and locked files are optional.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        r.decompress()?;
        let username = r.string()?;
        let token = r.u32()?;
        let count = r.u32()?;
        let mut budget = EntryBudget::new();
        let files = read_files(r, count, &mut budget)?;
        let has_free_upload_slot = r.u8()? > 0;
        let upload_speed = r.u32()?;
        let queue_length = r.u32()? as u64;
        if r.has_remaining() {
            r.u32()?;
        }
        let mut locked_files = Vec::new();
        if r.has_remaining() {
            let count = r.u32()?;
            locked_files = read_files(r, count, &mut budget)?;
        }
        Ok(Self {
            username,
            token,
            files,
            locked_files,
            has_free_upload_slot,
            upload_speed,
            queue_length,
        })
    }
}

/// Requests a peer's user info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserInfoRequest;

impl UserInfoRequest {
    pub fn to_bytes(&self) -> Bytes {
        builder(PeerCode::InfoRequest).build()
    }

    pub fn decode(_r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self)
    }
}

impl UserInfo {
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(PeerCode::InfoResponse);
        b.string(&self.description).bool(self.picture.is_some());
        if let Some(p) = &self.picture {
            b.u32(p.len() as u32).bytes(p);
        }
        b.u32(self.upload_slots)
            .u32(self.queue_length)
            .bool(self.has_free_upload_slot);
        if let Some(p) = self.upload_permissions {
            b.u32(p);
        }
        b.build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let description = r.string()?;
        let picture = if r.u8()? > 0 {
            let len = r.u32()? as usize;
            Some(r.bytes(len)?.to_vec())
        } else {
            None
        };
        let upload_slots = r.u32()?;
        let queue_length = r.u32()?;
        let has_free_upload_slot = r.u8()? > 0;
        let upload_permissions = if r.remaining() >= 4 {
            Some(r.u32()?)
        } else {
            None
        };
        Ok(Self {
            description,
            picture,
            upload_slots,
            queue_length,
            has_free_upload_slot,
            upload_permissions,
        })
    }
}

/// Requests the contents of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderContentsRequest {
    pub token: u32,
    pub directory_name: String,
}

impl FolderContentsRequest {
    pub fn to_bytes(&self) -> Bytes {
        builder(PeerCode::FolderContentsRequest)
            .u32(self.token)
            .string(&self.directory_name)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            token: r.u32()?,
            directory_name: r.string()?,
        })
    }
}

/// Directory contents (compressed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderContentsResponse {
    pub token: u32,
    pub directory_name: String,
    pub directories: Vec<Directory>,
}

impl FolderContentsResponse {
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(PeerCode::FolderContentsResponse);
        b.u32(self.token)
            .string(&self.directory_name)
            .u32(self.directories.len() as u32);
        for d in &self.directories {
            write_directory(&mut b, d);
        }
        b.compress().build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        r.decompress()?;
        let token = r.u32()?;
        let directory_name = r.string()?;
        let count = r.u32()?;
        Ok(Self {
            token,
            directory_name,
            directories: read_directories(r, count, &mut EntryBudget::new())?,
        })
    }
}

/// Requests a transfer. `Download` asks the peer to send; `Upload` announces an upload
/// to the peer (the size is only meaningful for uploads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub direction: TransferDirection,
    pub token: u32,
    pub filename: String,
    pub file_size: u64,
}

impl TransferRequest {
    pub fn to_bytes(&self) -> Bytes {
        let dir = match self.direction {
            TransferDirection::Download => 0,
            TransferDirection::Upload => 1,
        };
        builder(PeerCode::TransferRequest)
            .u32(dir)
            .u32(self.token)
            .string(&self.filename)
            .u64(self.file_size)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let direction = match r.u32()? {
            0 => TransferDirection::Download,
            1 => TransferDirection::Upload,
            other => {
                return Err(ProtocolError::Invalid(format!(
                    "invalid transfer direction {other}"
                )));
            }
        };
        let token = r.u32()?;
        let filename = r.string()?;
        let file_size = if r.has_remaining() { r.u64()? } else { 0 };
        Ok(Self {
            direction,
            token,
            filename,
            file_size,
        })
    }
}

/// Response to a [`TransferRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferResponse {
    pub token: u32,
    pub is_allowed: bool,
    /// Present when allowed and the peer included a size.
    pub file_size: Option<u64>,
    /// Rejection reason when not allowed.
    pub message: Option<String>,
}

impl TransferResponse {
    pub fn allowed(token: u32, file_size: Option<u64>) -> Self {
        Self {
            token,
            is_allowed: true,
            file_size,
            message: None,
        }
    }

    pub fn rejected(token: u32, message: impl Into<String>) -> Self {
        Self {
            token,
            is_allowed: false,
            file_size: None,
            message: Some(message.into()),
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(PeerCode::TransferResponse);
        b.u32(self.token).bool(self.is_allowed);
        if self.is_allowed {
            b.u64(self.file_size.unwrap_or(0));
        } else {
            b.string(self.message.as_deref().unwrap_or(""));
        }
        b.build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let token = r.u32()?;
        let allowed = r.u8()? == 1;
        Ok(if allowed {
            let size = if r.has_remaining() {
                Some(r.u64()?)
            } else {
                None
            };
            Self::allowed(token, size)
        } else {
            Self::rejected(token, r.string()?)
        })
    }
}

macro_rules! filename_message {
    ($(#[$m:meta])* $name:ident, $code:expr) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            pub filename: String,
        }

        impl $name {
            pub fn to_bytes(&self) -> Bytes {
                builder($code).string(&self.filename).build()
            }

            pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
                Ok(Self { filename: r.string()? })
            }
        }
    };
}

filename_message!(
    /// Asks a peer to queue a file for download.
    QueueDownloadRequest,
    PeerCode::QueueDownload
);
filename_message!(
    /// Asks for the place in queue of a file.
    PlaceInQueueRequest,
    PeerCode::PlaceInQueueRequest
);
filename_message!(
    /// Reports that an upload failed.
    UploadFailed,
    PeerCode::UploadFailed
);

/// Place in queue of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceInQueueResponse {
    pub filename: String,
    pub place_in_queue: u32,
}

impl PlaceInQueueResponse {
    pub fn to_bytes(&self) -> Bytes {
        builder(PeerCode::PlaceInQueueResponse)
            .string(&self.filename)
            .u32(self.place_in_queue)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            filename: r.string()?,
            place_in_queue: r.u32()?,
        })
    }
}

/// Rejects a queued download with a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadDenied {
    pub filename: String,
    pub message: String,
}

impl UploadDenied {
    pub fn to_bytes(&self) -> Bytes {
        builder(PeerCode::UploadDenied)
            .string(&self.filename)
            .string(&self.message)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            filename: r.string()?,
            message: r.string()?,
        })
    }
}

/// Incoming peer message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerIncoming {
    BrowseRequest(BrowseRequest),
    BrowseResponse(BrowseResponse),
    SearchRequest(PeerSearchRequest),
    SearchResponse(SearchResponse),
    InfoRequest(UserInfoRequest),
    InfoResponse(UserInfo),
    FolderContentsRequest(FolderContentsRequest),
    FolderContentsResponse(FolderContentsResponse),
    TransferRequest(TransferRequest),
    TransferResponse(TransferResponse),
    QueueDownload(QueueDownloadRequest),
    PlaceInQueueRequest(PlaceInQueueRequest),
    PlaceInQueueResponse(PlaceInQueueResponse),
    UploadFailed(UploadFailed),
    UploadDenied(UploadDenied),
    Unknown { code: u32, body: Bytes },
}

impl PeerIncoming {
    /// Decodes a frame body (code included, length prefix excluded).
    pub fn decode(body: Bytes) -> Result<Self, ProtocolError> {
        let mut r = MessageReader::new(body.clone());
        let code = r.code_u32()?;
        let r = &mut r;
        Ok(match PeerCode::try_from(code) {
            Ok(PeerCode::BrowseRequest) => Self::BrowseRequest(BrowseRequest::decode(r)?),
            Ok(PeerCode::BrowseResponse) => Self::BrowseResponse(BrowseResponse::decode(r)?),
            Ok(PeerCode::SearchRequest) => Self::SearchRequest(PeerSearchRequest::decode(r)?),
            Ok(PeerCode::SearchResponse) => Self::SearchResponse(SearchResponse::decode(r)?),
            Ok(PeerCode::InfoRequest) => Self::InfoRequest(UserInfoRequest::decode(r)?),
            Ok(PeerCode::InfoResponse) => Self::InfoResponse(UserInfo::decode(r)?),
            Ok(PeerCode::FolderContentsRequest) => {
                Self::FolderContentsRequest(FolderContentsRequest::decode(r)?)
            }
            Ok(PeerCode::FolderContentsResponse) => {
                Self::FolderContentsResponse(FolderContentsResponse::decode(r)?)
            }
            Ok(PeerCode::TransferRequest) => Self::TransferRequest(TransferRequest::decode(r)?),
            Ok(PeerCode::TransferResponse) => Self::TransferResponse(TransferResponse::decode(r)?),
            Ok(PeerCode::QueueDownload) => Self::QueueDownload(QueueDownloadRequest::decode(r)?),
            Ok(PeerCode::PlaceInQueueRequest) => {
                Self::PlaceInQueueRequest(PlaceInQueueRequest::decode(r)?)
            }
            Ok(PeerCode::PlaceInQueueResponse) => {
                Self::PlaceInQueueResponse(PlaceInQueueResponse::decode(r)?)
            }
            Ok(PeerCode::UploadFailed) => Self::UploadFailed(UploadFailed::decode(r)?),
            Ok(PeerCode::UploadDenied) => Self::UploadDenied(UploadDenied::decode(r)?),
            _ => Self::Unknown { code, body },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileAttributeType;

    fn decode(frame: Bytes) -> PeerIncoming {
        PeerIncoming::decode(frame.slice(4..)).unwrap()
    }

    fn file(name: &str) -> File {
        File {
            code: 1,
            filename: name.into(),
            size: 12345,
            extension: "mp3".into(),
            attributes: vec![
                FileAttribute {
                    kind: FileAttributeType::BitRate,
                    value: 320,
                },
                FileAttribute {
                    kind: FileAttributeType::Unknown(42),
                    value: 7,
                },
            ],
        }
    }

    fn dir(name: &str) -> Directory {
        Directory {
            name: name.into(),
            files: vec![file("a.mp3"), file("b.mp3")],
        }
    }

    #[test]
    fn empty_requests_bytes() {
        assert_eq!(&BrowseRequest.to_bytes()[..], &[4, 0, 0, 0, 4, 0, 0, 0]);
        assert_eq!(&UserInfoRequest.to_bytes()[..], &[4, 0, 0, 0, 15, 0, 0, 0]);
        assert_eq!(
            decode(BrowseRequest.to_bytes()),
            PeerIncoming::BrowseRequest(BrowseRequest)
        );
        assert_eq!(
            decode(UserInfoRequest.to_bytes()),
            PeerIncoming::InfoRequest(UserInfoRequest)
        );
    }

    #[test]
    fn browse_response_round_trip() {
        let m = BrowseResponse {
            directories: vec![dir("x"), dir("y")],
            locked_directories: vec![dir("z")],
        };
        assert_eq!(
            decode(m.to_bytes()),
            PeerIncoming::BrowseResponse(m.clone())
        );
        assert_eq!(decode(m.to_bytes_best()), PeerIncoming::BrowseResponse(m));
    }

    #[test]
    fn browse_response_without_locked() {
        let mut b = MessageBuilder::new();
        b.code_u32(5).u32(1);
        write_directory(&mut b, &dir("x"));
        let f = b.compress().build();
        let PeerIncoming::BrowseResponse(m) = decode(f) else {
            panic!()
        };
        assert_eq!(m.directory_count(), 1);
        assert_eq!(m.locked_directory_count(), 0);
    }

    #[test]
    fn file_negative_size_recovered() {
        let mut b = MessageBuilder::new();
        b.code_u8(0)
            .u8(1)
            .string("f")
            .u64(0xFFFF_FFFF_8000_0000)
            .string("")
            .u32(0);
        let mut r = MessageReader::new(b.build().slice(5..));
        assert_eq!(read_file(&mut r).unwrap().size, 0x8000_0000);
    }

    #[test]
    fn search_request_bytes() {
        let m = PeerSearchRequest {
            token: 1,
            query: "ab".into(),
        };
        assert_eq!(
            &m.to_bytes()[..],
            &[14, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, b'a', b'b']
        );
        assert_eq!(decode(m.to_bytes()), PeerIncoming::SearchRequest(m));
    }

    #[test]
    fn search_response_round_trip() {
        let m = SearchResponse {
            username: "u".into(),
            token: 9,
            files: vec![file("a"), file("b")],
            locked_files: vec![file("c")],
            has_free_upload_slot: true,
            upload_speed: 100,
            queue_length: 3,
        };
        assert_eq!(decode(m.to_bytes()), PeerIncoming::SearchResponse(m));
    }

    #[test]
    fn search_response_queue_length_clamps_to_i32() {
        let m = SearchResponse {
            username: "u".into(),
            token: 9,
            files: vec![],
            locked_files: vec![],
            has_free_upload_slot: false,
            upload_speed: 1,
            queue_length: u64::MAX,
        };
        let PeerIncoming::SearchResponse(d) = decode(m.to_bytes()) else {
            panic!()
        };
        assert_eq!(d.queue_length, i32::MAX as u64);
    }

    fn assert_invalid(frame: Bytes) {
        assert!(matches!(
            PeerIncoming::decode(frame.slice(4..)),
            Err(ProtocolError::Invalid(_))
        ));
    }

    #[test]
    fn entry_budget_rejects_oversized_counts() {
        let mut b = MessageBuilder::new();
        b.code_u32(PeerCode::BrowseResponse.as_u32())
            .u32(MAX_PARSED_DIRECTORIES as u32 + 1);
        assert_invalid(b.compress().build());

        let mut b = MessageBuilder::new();
        b.code_u32(PeerCode::BrowseResponse.as_u32())
            .u32(1)
            .string("d")
            .u32(MAX_PARSED_FILES as u32 + 1);
        assert_invalid(b.compress().build());

        let mut b = MessageBuilder::new();
        b.code_u32(PeerCode::SearchResponse.as_u32())
            .string("u")
            .u32(1)
            .u32(MAX_PARSED_FILES as u32 + 1);
        assert_invalid(b.compress().build());

        let mut b = MessageBuilder::new();
        b.code_u32(PeerCode::FolderContentsResponse.as_u32())
            .u32(1)
            .string("d")
            .u32(MAX_PARSED_DIRECTORIES as u32 + 1);
        assert_invalid(b.compress().build());
    }

    #[test]
    fn entry_budget_is_shared_across_lists() {
        let mut budget = EntryBudget::new();
        budget.take_files(MAX_PARSED_FILES as u32 - 1).unwrap();
        budget
            .take_directories(MAX_PARSED_DIRECTORIES as u32)
            .unwrap();
        assert!(matches!(
            budget.take_directories(1),
            Err(ProtocolError::Invalid(_))
        ));
        let mut b = MessageBuilder::new();
        b.code_u32(0);
        write_file(&mut b, &file("a"));
        write_file(&mut b, &file("b"));
        let mut r = MessageReader::new(b.build().slice(8..));
        assert!(matches!(
            read_files(&mut r, 2, &mut budget),
            Err(ProtocolError::Invalid(_))
        ));
        assert_eq!(read_files(&mut r, 1, &mut budget).unwrap().len(), 1);
    }

    #[test]
    fn search_response_legacy_without_trailer() {
        let mut b = MessageBuilder::new();
        b.code_u32(9).string("u").u32(1).u32(1);
        write_file(&mut b, &file("a"));
        b.u8(0).u32(5).u32(6);
        let PeerIncoming::SearchResponse(m) = decode(b.compress().build()) else {
            panic!()
        };
        assert_eq!(m.file_count(), 1);
        assert_eq!(m.locked_file_count(), 0);
        assert!(!m.has_free_upload_slot);
        assert_eq!((m.upload_speed, m.queue_length), (5, 6));
    }

    #[test]
    fn user_info_round_trip() {
        let m = UserInfo {
            description: "d".into(),
            picture: Some(vec![1, 2, 3]),
            upload_slots: 2,
            queue_length: 4,
            has_free_upload_slot: true,
            upload_permissions: None,
        };
        assert_eq!(decode(m.to_bytes()), PeerIncoming::InfoResponse(m.clone()));
        let m = UserInfo {
            picture: None,
            upload_permissions: Some(1),
            ..m
        };
        assert_eq!(decode(m.to_bytes()), PeerIncoming::InfoResponse(m));
    }

    #[test]
    fn folder_contents_round_trip() {
        let q = FolderContentsRequest {
            token: 3,
            directory_name: "x".into(),
        };
        assert_eq!(
            &q.to_bytes()[..],
            &[13, 0, 0, 0, 36, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, b'x']
        );
        assert_eq!(decode(q.to_bytes()), PeerIncoming::FolderContentsRequest(q));
        let m = FolderContentsResponse {
            token: 3,
            directory_name: "x".into(),
            directories: vec![dir("x")],
        };
        assert_eq!(
            decode(m.to_bytes()),
            PeerIncoming::FolderContentsResponse(m)
        );
    }

    #[test]
    fn transfer_request() {
        let m = TransferRequest {
            direction: TransferDirection::Upload,
            token: 2,
            filename: "f".into(),
            file_size: 10,
        };
        assert_eq!(
            &m.to_bytes()[..],
            &[
                25, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, b'f', 10, 0, 0, 0, 0,
                0, 0, 0
            ]
        );
        assert_eq!(decode(m.to_bytes()), PeerIncoming::TransferRequest(m));
        // download request without size
        let f = MessageBuilder::new()
            .code_u32(40)
            .u32(0)
            .u32(2)
            .string("f")
            .build();
        let PeerIncoming::TransferRequest(m) = decode(f) else {
            panic!()
        };
        assert_eq!(m.direction, TransferDirection::Download);
        assert_eq!(m.file_size, 0);
        let bad = MessageBuilder::new()
            .code_u32(40)
            .u32(5)
            .u32(2)
            .string("f")
            .build();
        assert!(PeerIncoming::decode(bad.slice(4..)).is_err());
    }

    #[test]
    fn transfer_response() {
        let a = TransferResponse::allowed(1, Some(99));
        assert_eq!(
            &a.to_bytes()[..],
            &[
                17, 0, 0, 0, 41, 0, 0, 0, 1, 0, 0, 0, 1, 99, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        assert_eq!(decode(a.to_bytes()), PeerIncoming::TransferResponse(a));
        let r = TransferResponse::rejected(1, "Queued");
        assert_eq!(decode(r.to_bytes()), PeerIncoming::TransferResponse(r));
        let f = MessageBuilder::new().code_u32(41).u32(1).u8(1).build();
        assert_eq!(
            decode(f),
            PeerIncoming::TransferResponse(TransferResponse::allowed(1, None))
        );
    }

    #[test]
    fn filename_messages() {
        let q = QueueDownloadRequest {
            filename: "f".into(),
        };
        assert_eq!(
            &q.to_bytes()[..],
            &[9, 0, 0, 0, 43, 0, 0, 0, 1, 0, 0, 0, b'f']
        );
        assert_eq!(decode(q.to_bytes()), PeerIncoming::QueueDownload(q));
        let p = PlaceInQueueRequest {
            filename: "f".into(),
        };
        assert_eq!(decode(p.to_bytes()), PeerIncoming::PlaceInQueueRequest(p));
        let u = UploadFailed {
            filename: "f".into(),
        };
        assert_eq!(decode(u.to_bytes()), PeerIncoming::UploadFailed(u));
        let pr = PlaceInQueueResponse {
            filename: "f".into(),
            place_in_queue: 7,
        };
        assert_eq!(
            decode(pr.to_bytes()),
            PeerIncoming::PlaceInQueueResponse(pr)
        );
        let d = UploadDenied {
            filename: "f".into(),
            message: "Banned".into(),
        };
        assert_eq!(decode(d.to_bytes()), PeerIncoming::UploadDenied(d));
    }

    #[test]
    fn unknown_and_truncated() {
        let f = MessageBuilder::new().code_u32(1).u32(5).build();
        assert!(matches!(decode(f), PeerIncoming::Unknown { code: 1, .. }));
        let f = MessageBuilder::new().code_u32(44).string("f").build();
        assert!(PeerIncoming::decode(f.slice(4..)).is_err());
    }
}
