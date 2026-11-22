//! Message codes for every Soulseek message family.

use super::codec::ProtocolError;

macro_rules! codes {
    ($(#[$m:meta])* $name:ident: $repr:ty { $($variant:ident = $value:expr,)* }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr($repr)]
        pub enum $name {
            $($variant = $value,)*
        }

        impl $name {
            /// All defined codes.
            pub const ALL: &'static [$name] = &[$($name::$variant,)*];
        }

        impl TryFrom<$repr> for $name {
            type Error = ProtocolError;

            fn try_from(v: $repr) -> Result<Self, ProtocolError> {
                match v {
                    $($value => Ok($name::$variant),)*
                    other => Err(ProtocolError::UnknownCode { code: other as u32 }),
                }
            }
        }

        impl From<$name> for $repr {
            fn from(c: $name) -> $repr {
                c as $repr
            }
        }
    };
}

codes! {
    /// Distributed network message codes (1 byte).
    DistributedCode: u8 {
        Ping = 0,
        SearchRequest = 3,
        BranchLevel = 4,
        BranchRoot = 5,
        Unknown = 6,
        ChildDepth = 7,
        EmbeddedMessage = 93,
    }
}

codes! {
    /// Peer connection initialization codes (1 byte).
    InitCode: u8 {
        PierceFirewall = 0,
        PeerInit = 1,
    }
}

codes! {
    /// Peer message codes (4 bytes).
    PeerCode: u32 {
        PrivateMessage = 1,
        BrowseRequest = 4,
        BrowseResponse = 5,
        SearchRequest = 8,
        SearchResponse = 9,
        PrivateRoomInvitation = 10,
        CancelledQueuedTransfer = 14,
        InfoRequest = 15,
        InfoResponse = 16,
        SendConnectToken = 33,
        MoveDownloadToTop = 34,
        FolderContentsRequest = 36,
        FolderContentsResponse = 37,
        TransferRequest = 40,
        TransferResponse = 41,
        UploadPlacehold = 42,
        QueueDownload = 43,
        PlaceInQueueResponse = 44,
        UploadFailed = 46,
        ExactFileSearchRequest = 47,
        QueuedDownloads = 48,
        IndirectFileSearchRequest = 49,
        UploadDenied = 50,
        PlaceInQueueRequest = 51,
        UploadQueueNotification = 52,
    }
}

codes! {
    /// Server message codes (4 bytes).
    ServerCode: u32 {
        Unknown = 0,
        Login = 1,
        SetListenPort = 2,
        GetPeerAddress = 3,
        WatchUser = 5,
        UnwatchUser = 6,
        GetStatus = 7,
        SayInChatRoom = 13,
        JoinRoom = 14,
        LeaveRoom = 15,
        UserJoinedRoom = 16,
        UserLeftRoom = 17,
        ConnectToPeer = 18,
        PrivateMessage = 22,
        AcknowledgePrivateMessage = 23,
        FileSearch = 26,
        SetOnlineStatus = 28,
        Ping = 32,
        SendSpeed = 34,
        SharedFoldersAndFiles = 35,
        GetUserStats = 36,
        QueuedDownloads = 40,
        KickedFromServer = 41,
        UserSearch = 42,
        LikedInterestAdd = 51,
        LikedInterestRemove = 52,
        GetPersonalRecommendations = 54,
        GetGlobalRecommendations = 56,
        GetUserInterests = 57,
        RoomList = 64,
        ExactFileSearch = 65,
        GlobalAdminMessage = 66,
        PrivilegedUsers = 69,
        HaveNoParents = 71,
        ParentsIP = 73,
        ParentMinSpeed = 83,
        ParentSpeedRatio = 84,
        ParentInactivityTimeout = 86,
        SearchInactivityTimeout = 87,
        MinimumParentsInCache = 88,
        DistributedAliveInterval = 90,
        AddPrivilegedUser = 91,
        CheckPrivileges = 92,
        EmbeddedMessage = 93,
        AcceptChildren = 100,
        NetInfo = 102,
        WishlistSearch = 103,
        WishlistInterval = 104,
        GetPersonalSimilarUsers = 110,
        GetInterestRecommendations = 111,
        GetInterestSimilarUsers = 112,
        RoomTickers = 113,
        RoomTickerAdd = 114,
        RoomTickerRemove = 115,
        SetRoomTicker = 116,
        HatedInterestAdd = 117,
        HatedInterestRemove = 118,
        RoomSearch = 120,
        SendUploadSpeed = 121,
        UserPrivileges = 122,
        GivePrivileges = 123,
        NotifyPrivileges = 124,
        AcknowledgeNotifyPrivileges = 125,
        BranchLevel = 126,
        BranchRoot = 127,
        ChildDepth = 129,
        DistributedReset = 130,
        PrivateRoomUsers = 133,
        PrivateRoomAddUser = 134,
        PrivateRoomRemoveUser = 135,
        PrivateRoomDropMembership = 136,
        PrivateRoomDropOwnership = 137,
        PrivateRoomUnknown = 138,
        PrivateRoomAdded = 139,
        PrivateRoomRemoved = 140,
        PrivateRoomToggle = 141,
        NewPassword = 142,
        PrivateRoomAddOperator = 143,
        PrivateRoomRemoveOperator = 144,
        PrivateRoomOperatorAdded = 145,
        PrivateRoomOperatorRemoved = 146,
        PrivateRoomOwned = 148,
        MessageUsers = 149,
        AskPublicChat = 150,
        StopPublicChat = 151,
        PublicChat = 152,
        RelatedSearch = 153,
        ExcludedSearchPhrases = 160,
        CannotConnect = 1001,
        CannotJoinRoom = 1003,
    }
}

impl ServerCode {
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

impl PeerCode {
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

impl InitCode {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl DistributedCode {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for c in ServerCode::ALL {
            assert_eq!(ServerCode::try_from(c.as_u32()).unwrap(), *c);
        }
        for c in PeerCode::ALL {
            assert_eq!(PeerCode::try_from(c.as_u32()).unwrap(), *c);
        }
        for c in InitCode::ALL {
            assert_eq!(InitCode::try_from(c.as_u8()).unwrap(), *c);
        }
        for c in DistributedCode::ALL {
            assert_eq!(DistributedCode::try_from(c.as_u8()).unwrap(), *c);
        }
    }

    #[test]
    fn values_and_unknown() {
        assert_eq!(ServerCode::CannotJoinRoom.as_u32(), 1003);
        assert_eq!(PeerCode::UploadQueueNotification.as_u32(), 52);
        assert_eq!(DistributedCode::EmbeddedMessage.as_u8(), 93);
        assert!(matches!(
            PeerCode::try_from(2u32),
            Err(ProtocolError::UnknownCode { code: 2 })
        ));
        assert!(InitCode::try_from(5u8).is_err());
        assert_eq!(ServerCode::ALL.len(), 90);
    }
}
