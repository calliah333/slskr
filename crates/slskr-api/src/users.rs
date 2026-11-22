//! Users API models (`/api/v0/users/{username}/...`), including the
//! Soulseek.NET `File`/`Directory`/`BrowseResponse` shapes slskd returns as-is.

use serde::{Deserialize, Serialize};

/// Soulseek.NET `UserPresence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum UserPresence {
    #[default]
    Offline,
    Away,
    Online,
}

/// Soulseek.NET `FileAttributeType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileAttributeType {
    BitRate,
    Length,
    VariableBitRate,
    SampleRate,
    BitDepth,
}

/// Soulseek.NET `FileAttribute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAttribute {
    #[serde(rename = "type")]
    pub kind: FileAttributeType,
    pub value: i32,
}

/// Soulseek.NET `File`, as serialized in browse/directory/share responses and
/// search hub `RESPONSE` events.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    #[serde(default)]
    pub attribute_count: u32,
    #[serde(default)]
    pub attributes: Vec<FileAttribute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u32>,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub extension: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_variable_bit_rate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub size: u64,
}

impl File {
    /// Builds a file, deriving `attributeCount` and the typed attribute
    /// accessors from `attributes` as Soulseek.NET does.
    pub fn new(
        code: i32,
        filename: String,
        size: u64,
        extension: String,
        attributes: Vec<FileAttribute>,
    ) -> Self {
        let find =
            |kind: FileAttributeType| attributes.iter().find(|a| a.kind == kind).map(|a| a.value);
        let as_u32 = |value: Option<i32>| value.and_then(|v| u32::try_from(v).ok());

        Self {
            attribute_count: attributes.len() as u32,
            bit_depth: as_u32(find(FileAttributeType::BitDepth)),
            bit_rate: as_u32(find(FileAttributeType::BitRate)),
            is_variable_bit_rate: find(FileAttributeType::VariableBitRate).map(|v| v == 1),
            length: as_u32(find(FileAttributeType::Length)),
            sample_rate: as_u32(find(FileAttributeType::SampleRate)),
            attributes,
            code,
            extension,
            filename,
            size,
        }
    }
}

/// Soulseek.NET `Directory`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Directory {
    pub name: String,
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub files: Vec<File>,
}

impl Directory {
    /// Builds a directory with `fileCount` derived from `files`.
    pub fn new(name: String, files: Vec<File>) -> Self {
        Self {
            file_count: files.len() as u32,
            name,
            files,
        }
    }
}

/// Soulseek.NET `BrowseResponse`, returned by `GET /users/{username}/browse`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseResponse {
    #[serde(default)]
    pub directories: Vec<Directory>,
    #[serde(default)]
    pub directory_count: u32,
    #[serde(default)]
    pub locked_directories: Vec<Directory>,
    #[serde(default)]
    pub locked_directory_count: u32,
}

impl BrowseResponse {
    /// Builds a response with counts derived from the lists.
    pub fn new(directories: Vec<Directory>, locked_directories: Vec<Directory>) -> Self {
        Self {
            directory_count: directories.len() as u32,
            locked_directory_count: locked_directories.len() as u32,
            directories,
            locked_directories,
        }
    }
}

/// Browse progress, returned by `GET /users/{username}/browse/status`
/// (Soulseek.NET `BrowseProgressUpdatedEventArgs`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseStatus {
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub bytes_remaining: u64,
    #[serde(default)]
    pub percent_complete: f64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub username: String,
}

/// Body of `POST /users/{username}/directory`; the response is `Vec<Directory>`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryContentsRequest {
    pub directory: String,
}

/// .NET `IPEndPoint`, returned by `GET /users/{username}/endpoint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// .NET `AddressFamily` name: `InterNetwork` or `InterNetworkV6`.
    #[serde(default = "default_address_family")]
    pub address_family: String,
    pub address: String,
    pub port: u16,
}

fn default_address_family() -> String {
    "InterNetwork".to_owned()
}

impl Endpoint {
    /// Builds an endpoint from a socket address.
    pub fn from_socket_addr(addr: std::net::SocketAddr) -> Self {
        Self {
            address_family: if addr.is_ipv4() {
                "InterNetwork"
            } else {
                "InterNetworkV6"
            }
            .to_owned(),
            address: addr.ip().to_string(),
            port: addr.port(),
        }
    }
}

/// slskd `Users.Info`, returned by `GET /users/{username}/info`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub has_picture: bool,
    /// Base64-encoded picture bytes (.NET `byte[]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(default)]
    pub queue_length: u32,
    #[serde(default)]
    pub upload_slots: u32,
}

/// slskd `Users.Status`, returned by `GET /users/{username}/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(default)]
    pub is_privileged: bool,
    #[serde(default)]
    pub presence: UserPresence,
}

/// slskd `Users.Statistics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Statistics {
    #[serde(default)]
    pub average_speed: u32,
    #[serde(default)]
    pub directory_count: u32,
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub upload_count: u64,
}

/// slskd `Users.User`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statistics: Option<Statistics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_json_shape() {
        let file = File::new(
            1,
            "@@music\\a.mp3".into(),
            1234,
            "mp3".into(),
            vec![
                FileAttribute {
                    kind: FileAttributeType::BitRate,
                    value: 320,
                },
                FileAttribute {
                    kind: FileAttributeType::VariableBitRate,
                    value: 0,
                },
            ],
        );
        let value = serde_json::to_value(&file).unwrap();
        assert_eq!(value["attributeCount"], 2);
        assert_eq!(value["attributes"][0]["type"], "BitRate");
        assert_eq!(value["attributes"][0]["value"], 320);
        assert_eq!(value["bitRate"], 320);
        assert_eq!(value["isVariableBitRate"], false);
        assert!(value.get("sampleRate").is_none());
        assert_eq!(serde_json::from_value::<File>(value).unwrap(), file);
    }

    #[test]
    fn browse_and_user_shapes() {
        let browse = BrowseResponse::new(vec![Directory::new("a".into(), vec![])], vec![]);
        let value = serde_json::to_value(&browse).unwrap();
        assert_eq!(value["directoryCount"], 1);
        assert_eq!(value["lockedDirectoryCount"], 0);
        assert_eq!(value["directories"][0]["fileCount"], 0);

        let status: Status =
            serde_json::from_str(r#"{"isPrivileged":true,"presence":"Away"}"#).unwrap();
        assert_eq!(status.presence, UserPresence::Away);

        let endpoint = Endpoint::from_socket_addr("1.2.3.4:2234".parse().unwrap());
        assert_eq!(
            serde_json::to_string(&endpoint).unwrap(),
            r#"{"addressFamily":"InterNetwork","address":"1.2.3.4","port":2234}"#
        );

        let info: Info = serde_json::from_str(
            r#"{"description":"hi","hasFreeUploadSlot":true,"hasPicture":false,"queueLength":3,"uploadSlots":2}"#,
        )
        .unwrap();
        assert_eq!(info.queue_length, 3);
        assert!(info.picture.is_none());
    }
}
