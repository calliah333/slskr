use std::net::SocketAddr;
use std::time::Duration;

use crate::client::{
    InterestRecommendations, InterestSimilarUsers, Recommendations, SimilarUser, SoulseekClient,
    require_non_blank,
};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::model::{
    RoomData, RoomList, UserData, UserInterests, UserPresence, UserStatistics, UserStatus,
};
use crate::net::message_connection::MessageConnection;
use crate::net::waiter::WaitKey;
use crate::protocol::server::*;

fn key(kind: &'static str, discriminator: &str) -> WaitKey {
    WaitKey(kind, discriminator.to_owned())
}

fn timeout(client: &SoulseekClient) -> Duration {
    client
        .inner()
        .options
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .message_timeout
}

async fn request<T: Send + 'static>(
    client: &SoulseekClient,
    frame: bytes::Bytes,
    wait_key: WaitKey,
) -> Result<T> {
    let inner = client.inner();
    let (transport, waiting) = inner.server.with_logged_in_transport(inner, |transport| {
        (
            transport,
            inner.waiter.wait(wait_key, Some(timeout(client))),
        )
    })?;
    transport.send(frame).await?;
    waiting.await
}

async fn send(client: &SoulseekClient, frame: bytes::Bytes) -> Result<()> {
    client.inner().send_server(frame).await
}

fn username_key(kind: &'static str, username: &str) -> WaitKey {
    key(kind, username)
}

impl SoulseekClient {
    /// Pings the server and returns the round-trip time.
    pub async fn ping_server(&self) -> Result<Duration> {
        self.inner.ensure_logged_in("ping the server")?;
        let started = std::time::Instant::now();
        request::<()>(self, ServerPing {}.to_bytes(), key("ping", "")).await?;
        Ok(started.elapsed())
    }

    /// Changes the logged-in user's password and waits for server confirmation.
    pub async fn change_password(&self, password: &str) -> Result<()> {
        require_non_blank(password, "password")?;
        self.inner.ensure_logged_in("change the password")?;
        request(
            self,
            NewPassword {
                password: password.into(),
            }
            .to_bytes(),
            key("new_password", ""),
        )
        .await
    }

    /// Sets our online presence.
    pub async fn set_status(&self, status: UserPresence) -> Result<()> {
        self.inner.ensure_logged_in("set status")?;
        send(self, SetOnlineStatusCommand { status }.to_bytes()).await
    }
    /// Reports the number of shared directories and files.
    pub async fn set_shared_counts(&self, directories: u32, files: u32) -> Result<()> {
        self.inner.ensure_logged_in("set shared counts")?;
        send(
            self,
            SetSharedCountsCommand {
                directory_count: directories,
                file_count: files,
            }
            .to_bytes(),
        )
        .await
    }
    /// Reports the bytes per second of the last completed upload.
    pub async fn send_upload_speed(&self, speed: u32) -> Result<()> {
        if speed == 0 {
            return Err(Error::Argument(
                "The speed must be greater than zero".into(),
            ));
        }
        self.inner.ensure_logged_in("send upload speed")?;
        send(self, SendUploadSpeedCommand { speed }.to_bytes()).await
    }
    /// Starts receiving public chat messages.
    pub async fn start_public_chat(&self) -> Result<()> {
        self.inner.ensure_logged_in("start public chat")?;
        send(self, StartPublicChatCommand {}.to_bytes()).await
    }
    /// Stops receiving public chat messages.
    pub async fn stop_public_chat(&self) -> Result<()> {
        self.inner.ensure_logged_in("stop public chat")?;
        send(self, StopPublicChatCommand {}.to_bytes()).await
    }

    /// Resolves a user's listening endpoint; an unspecified address at port zero means offline.
    /// Successful results are cached briefly for the current server session.
    pub async fn get_user_endpoint(&self, username: &str) -> Result<SocketAddr> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("get user endpoint")?;
        if let Some(endpoint) = self.inner.server.cached_endpoint(username) {
            return Ok(endpoint);
        }
        let endpoint: SocketAddr = request(
            self,
            UserAddressRequest {
                username: username.into(),
            }
            .to_bytes(),
            username_key("peer_address", username),
        )
        .await
        .map_err(|error| match error {
            Error::Timeout(_) | Error::Cancelled => error,
            other => Error::UserEndPoint(other.to_string()),
        })?;
        if endpoint.ip().is_unspecified() && endpoint.port() == 0 {
            return Err(Error::UserOffline(format!(
                "User {username} appears to be offline"
            )));
        }
        Ok(endpoint)
    }

    /// Fetches a user's status.
    pub async fn get_user_status(&self, username: &str) -> Result<UserStatus> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("get user status")?;
        request(
            self,
            UserStatusRequest {
                username: username.into(),
            }
            .to_bytes(),
            username_key("user_status", username),
        )
        .await
    }
    /// Fetches a user's statistics; unknown users yield zeros.
    pub async fn get_user_statistics(&self, username: &str) -> Result<UserStatistics> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("get user statistics")?;
        request(
            self,
            UserStatisticsRequest {
                username: username.into(),
            }
            .to_bytes(),
            username_key("user_statistics", username),
        )
        .await
    }
    /// Checks whether a user is privileged.
    pub async fn get_user_privileged(&self, username: &str) -> Result<bool> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("get user privileges")?;
        request(
            self,
            UserPrivilegesRequest {
                username: username.into(),
            }
            .to_bytes(),
            username_key("user_privileges", username),
        )
        .await
    }
    /// Watches a user for status changes and returns their current data.
    pub async fn watch_user(&self, username: &str) -> Result<UserData> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("watch a user")?;
        let r: WatchUserResponse = request(
            self,
            WatchUserRequest {
                username: username.into(),
            }
            .to_bytes(),
            username_key("watch_user", username),
        )
        .await?;
        r.user_data
            .ok_or_else(|| Error::UserNotFound(format!("User {username} does not exist")))
    }
    /// Removes a user from the watch list.
    pub async fn unwatch_user(&self, username: &str) -> Result<()> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("unwatch a user")?;
        send(
            self,
            UnwatchUserCommand {
                username: username.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Returns remaining privilege time in seconds.
    pub async fn get_privileges(&self) -> Result<u32> {
        self.inner.ensure_logged_in("get privileges")?;
        request(
            self,
            CheckPrivilegesRequest {}.to_bytes(),
            key("privileges", ""),
        )
        .await
    }
    /// Gives a user `days` of privileges. Failures arrive as admin messages.
    pub async fn grant_user_privileges(&self, username: &str, days: u32) -> Result<()> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("grant privileges")?;
        send(
            self,
            GivePrivilegesCommand {
                username: username.into(),
                days,
            }
            .to_bytes(),
        )
        .await
    }
    /// Acknowledges a privilege notification.
    pub async fn acknowledge_privilege_notification(&self, id: u32) -> Result<()> {
        self.inner
            .ensure_logged_in("acknowledge privilege notifications")?;
        send(
            self,
            AcknowledgePrivilegeNotificationCommand { id }.to_bytes(),
        )
        .await
    }
    /// Sends a private message without waiting for a response.
    pub async fn send_private_message(&self, username: &str, message: &str) -> Result<()> {
        require_non_blank(username, "username")?;
        if message.is_empty() {
            return Err(Error::Argument(
                "The message must not be a null or empty string".into(),
            ));
        }
        self.inner.ensure_logged_in("send a private message")?;
        send(
            self,
            PrivateMessageCommand {
                username: username.into(),
                message: message.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Acknowledges an incoming private message.
    pub async fn acknowledge_private_message(&self, id: u32) -> Result<()> {
        self.inner
            .ensure_logged_in("acknowledge private messages")?;
        send(self, AcknowledgePrivateMessageCommand { id }.to_bytes()).await
    }

    /// Joins a room and returns its data. A forbidden join returns [`Error::RoomJoinForbidden`];
    /// no reply returns [`Error::NoResponse`] (the client may already be in the room).
    pub async fn join_room(&self, room: &str, is_private: bool) -> Result<RoomData> {
        require_non_blank(room, "room name")?;
        self.inner.ensure_logged_in("join a chat room")?;
        request(
            self,
            JoinRoomRequest {
                room_name: room.into(),
                is_private,
            }
            .to_bytes(),
            key("join_room", room),
        )
        .await
        .map_err(room_timeout)
    }

    /// Leaves a room. No reply returns [`Error::NoResponse`] (the client may not be in the room).
    pub async fn leave_room(&self, room: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        self.inner.ensure_logged_in("leave a chat room")?;
        request(
            self,
            LeaveRoomRequest {
                room_name: room.into(),
            }
            .to_bytes(),
            key("leave_room", room),
        )
        .await
        .map_err(room_timeout)
    }

    /// Fetches the room list, also raised as [`Event::RoomListReceived`].
    pub async fn get_room_list(&self) -> Result<RoomList> {
        self.inner.ensure_logged_in("get the room list")?;
        request(self, RoomListRequest {}.to_bytes(), key("room_list", "")).await
    }
    /// Sends a chat message to a room.
    pub async fn send_room_message(&self, room: &str, message: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        if message.is_empty() {
            return Err(Error::Argument(
                "The message must not be a null or empty string".into(),
            ));
        }
        self.inner.ensure_logged_in("send a chat room message")?;
        send(
            self,
            RoomMessageCommand {
                room_name: room.into(),
                message: message.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Sets our ticker in a room.
    pub async fn set_room_ticker(&self, room: &str, message: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        if message.is_empty() {
            return Err(Error::Argument(
                "The message must not be a null or empty string".into(),
            ));
        }
        self.inner.ensure_logged_in("set a chat room ticker")?;
        send(
            self,
            SetRoomTickerCommand {
                room_name: room.into(),
                message: message.into(),
            }
            .to_bytes(),
        )
        .await
    }

    /// Adds a member to a private room and waits for the server to echo the addition.
    pub async fn add_private_room_member(&self, room: &str, username: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("add a private room member")?;
        request(
            self,
            PrivateRoomAddUser {
                room_name: room.into(),
                username: username.into(),
            }
            .to_bytes(),
            private_room_member_key(room, username),
        )
        .await
    }

    /// Removes a private room member.
    pub async fn remove_private_room_member(&self, room: &str, username: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        require_non_blank(username, "username")?;
        self.inner
            .ensure_logged_in("remove a private room member")?;
        send(
            self,
            PrivateRoomRemoveUser {
                room_name: room.into(),
                username: username.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Adds a private room moderator.
    pub async fn add_private_room_moderator(&self, room: &str, username: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        require_non_blank(username, "username")?;
        self.inner
            .ensure_logged_in("add a private room moderator")?;
        send(
            self,
            PrivateRoomAddOperator {
                room_name: room.into(),
                username: username.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Removes a private room moderator.
    pub async fn remove_private_room_moderator(&self, room: &str, username: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        require_non_blank(username, "username")?;
        self.inner
            .ensure_logged_in("remove a private room moderator")?;
        send(
            self,
            PrivateRoomRemoveOperator {
                room_name: room.into(),
                username: username.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Gives up private room membership.
    pub async fn drop_private_room_membership(&self, room: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        self.inner
            .ensure_logged_in("drop private room membership")?;
        send(
            self,
            PrivateRoomDropMembershipCommand {
                room_name: room.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Gives up private room ownership.
    pub async fn drop_private_room_ownership(&self, room: &str) -> Result<()> {
        require_non_blank(room, "room name")?;
        self.inner.ensure_logged_in("drop private room ownership")?;
        send(
            self,
            PrivateRoomDropOwnershipCommand {
                room_name: room.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Adds a liked interest.
    pub async fn add_liked_interest(&self, interest: &str) -> Result<()> {
        require_non_blank(interest, "interest")?;
        self.inner.ensure_logged_in("add a liked interest")?;
        send(
            self,
            LikedInterestAddCommand {
                interest: interest.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Adds a hated interest.
    pub async fn add_hated_interest(&self, interest: &str) -> Result<()> {
        require_non_blank(interest, "interest")?;
        self.inner.ensure_logged_in("add a hated interest")?;
        send(
            self,
            HatedInterestAddCommand {
                interest: interest.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Removes a liked interest.
    pub async fn remove_liked_interest(&self, interest: &str) -> Result<()> {
        require_non_blank(interest, "interest")?;
        self.inner.ensure_logged_in("remove a liked interest")?;
        send(
            self,
            LikedInterestRemoveCommand {
                interest: interest.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Removes a hated interest.
    pub async fn remove_hated_interest(&self, interest: &str) -> Result<()> {
        require_non_blank(interest, "interest")?;
        self.inner.ensure_logged_in("remove a hated interest")?;
        send(
            self,
            HatedInterestRemoveCommand {
                interest: interest.into(),
            }
            .to_bytes(),
        )
        .await
    }
    /// Fetches the logged-in user's interests.
    pub async fn get_interests(&self) -> Result<UserInterests> {
        self.inner.ensure_logged_in("get interests")?;
        let u = self
            .username()
            .ok_or_else(|| Error::InvalidState("The username is not known".into()))?;
        self.get_user_interests(&u).await
    }
    /// Fetches another user's interests.
    pub async fn get_user_interests(&self, username: &str) -> Result<UserInterests> {
        require_non_blank(username, "username")?;
        self.inner.ensure_logged_in("get user interests")?;
        request(
            self,
            UserInterestsRequest {
                username: username.into(),
            }
            .to_bytes(),
            username_key("user_interests", username),
        )
        .await
    }
    /// Fetches server-wide recommendations.
    pub async fn get_global_recommendations(&self) -> Result<Recommendations> {
        self.inner.ensure_logged_in("get global recommendations")?;
        let r: RecommendationsResponse = request(
            self,
            GlobalRecommendationsRequest {}.to_bytes(),
            key("global_recommendations", ""),
        )
        .await?;
        Ok(Recommendations {
            recommended: r.recommendations,
            not_recommended: r.unrecommendations,
        })
    }
    /// Fetches recommendations based on our interests.
    pub async fn get_personal_recommendations(&self) -> Result<Recommendations> {
        self.inner.ensure_logged_in("get recommendations")?;
        let r: RecommendationsResponse = request(
            self,
            PersonalRecommendationsRequest {}.to_bytes(),
            key("personal_recommendations", ""),
        )
        .await?;
        Ok(Recommendations {
            recommended: r.recommendations,
            not_recommended: r.unrecommendations,
        })
    }
    /// Fetches recommendations related to an interest.
    pub async fn get_interest_recommendations(
        &self,
        interest: &str,
    ) -> Result<InterestRecommendations> {
        require_non_blank(interest, "interest")?;
        self.inner
            .ensure_logged_in("get interest recommendations")?;
        request(
            self,
            InterestRecommendationsRequest {
                interest: interest.into(),
            }
            .to_bytes(),
            key("interest_recommendations", interest),
        )
        .await
    }
    /// Fetches users who like an interest.
    pub async fn get_interest_similar_users(&self, interest: &str) -> Result<InterestSimilarUsers> {
        require_non_blank(interest, "interest")?;
        self.inner.ensure_logged_in("get similar users")?;
        request(
            self,
            InterestSimilarUsersRequest {
                interest: interest.into(),
            }
            .to_bytes(),
            key("interest_similar_users", interest),
        )
        .await
    }
    /// Fetches users with interests similar to ours.
    pub async fn get_similar_users(&self) -> Result<Vec<SimilarUser>> {
        self.inner.ensure_logged_in("get similar users")?;
        request(
            self,
            PersonalSimilarUsersRequest {}.to_bytes(),
            key("similar_users", ""),
        )
        .await
    }
}

fn room_timeout(error: Error) -> Error {
    match error {
        Error::Timeout(message) => Error::NoResponse(message),
        other => other,
    }
}

fn private_room_member_key(room: &str, username: &str) -> WaitKey {
    WaitKey(
        "private_room_add_user",
        format!("{}:{room}{username}", room.len()),
    )
}

fn acknowledge(transport: &MessageConnection, frame: bytes::Bytes) {
    let transport = transport.clone();
    tokio::spawn(async move {
        let _ = transport.send(frame).await;
    });
}

/// Correlates server replies and emits ordinary events. Lifecycle and peer/distributed
/// messages are intentionally left to their owning dispatchers.
pub(super) fn dispatch(
    client: &SoulseekClient,
    transport: &MessageConnection,
    msg: ServerIncoming,
) {
    let w = &client.inner().waiter;
    match msg {
        ServerIncoming::WatchUser(r) => {
            let k = username_key("watch_user", &r.username);
            if !w.complete(&k, r.clone()) {
                if let Some(d) = r.user_data {
                    client.inner().emit(Event::UserStatusChanged(UserStatus {
                        username: d.username,
                        presence: d.status,
                        is_privileged: false,
                    }));
                }
            }
        }
        ServerIncoming::UserStatus(r) => {
            let k = username_key("user_status", &r.username);
            if !w.complete(&k, r.clone()) {
                client.inner().emit(Event::UserStatusChanged(r));
            }
        }
        ServerIncoming::UserStatistics(r) => {
            let k = username_key("user_statistics", &r.username);
            if !w.complete(&k, r.clone()) {
                client.inner().emit(Event::UserStatisticsChanged(r));
            }
        }
        ServerIncoming::UserPrivileges(r) => {
            let k = username_key("user_privileges", &r.username);
            let _ = w.complete(&k, r.is_privileged);
        }
        ServerIncoming::Ping => {
            let _ = w.complete(&key("ping", ""), ());
        }
        ServerIncoming::NewPassword(_) => {
            let _ = w.complete(&key("new_password", ""), ());
        }
        ServerIncoming::CheckPrivileges(v) => {
            let _ = w.complete(&key("privileges", ""), v);
        }
        ServerIncoming::JoinRoom(room) => {
            if let Some(username) = client.username() {
                let user_data = room
                    .users
                    .iter()
                    .find(|user| user.username == username)
                    .cloned();
                client.inner().emit(Event::RoomJoined {
                    room: room.name.clone(),
                    username,
                    user_data,
                });
            }
            w.complete(&key("join_room", &room.name), room);
        }
        ServerIncoming::LeaveRoom(r) => {
            let _ = w.complete(&key("leave_room", &r.room_name), ());
            client.inner().emit(Event::RoomLeft {
                room: r.room_name,
                username: client.username().unwrap_or_default(),
            });
        }
        ServerIncoming::RoomList(r) => {
            let _ = w.complete(&key("room_list", ""), r.clone());
            client.inner().emit(Event::RoomListReceived(r));
        }
        ServerIncoming::CannotJoinRoom(r) => {
            let _ = w.throw(
                &key("join_room", &r.room_name),
                Error::RoomJoinForbidden(format!("Unable to join room {}", r.room_name)),
            );
        }
        ServerIncoming::PrivateRoomAddUser(r) => {
            w.complete(&private_room_member_key(&r.room_name, &r.username), ());
        }
        ServerIncoming::PersonalRecommendations(r) => {
            let _ = w.complete(&key("personal_recommendations", ""), r);
        }
        ServerIncoming::GlobalRecommendations(r) => {
            let _ = w.complete(&key("global_recommendations", ""), r);
        }
        ServerIncoming::UserInterests(r) => {
            let k = username_key("user_interests", &r.username);
            let _ = w.complete(&k, r);
        }
        ServerIncoming::PersonalSimilarUsers(r) => {
            let _ = w.complete(
                &key("similar_users", ""),
                r.users
                    .into_iter()
                    .map(|u| SimilarUser {
                        username: u.username,
                        rating: u.rating,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        ServerIncoming::InterestRecommendations(r) => {
            let k = key("interest_recommendations", &r.interest);
            let value = InterestRecommendations {
                interest: r.interest,
                recommended: r.recommendations,
                not_recommended: Vec::new(),
            };
            let _ = w.complete(&k, value);
        }
        ServerIncoming::InterestSimilarUsers(r) => {
            let k = key("interest_similar_users", &r.interest);
            let value = InterestSimilarUsers {
                interest: r.interest,
                usernames: r.usernames,
            };
            let _ = w.complete(&k, value);
        }
        ServerIncoming::PrivateMessage(r) => {
            let id = r.id;
            client.inner().emit(Event::PrivateMessageReceived(r));
            if client
                .inner()
                .options
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .auto_acknowledge_private_messages
            {
                acknowledge(
                    transport,
                    AcknowledgePrivateMessageCommand { id }.to_bytes(),
                );
            }
        }
        ServerIncoming::NotifyPrivileges(r) => {
            let id = r.id;
            client.inner().emit(Event::PrivilegeNotificationReceived(r));
            if let Some(id) = id
                && client
                    .inner()
                    .options
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .auto_acknowledge_privilege_notifications
            {
                acknowledge(
                    transport,
                    AcknowledgePrivilegeNotificationCommand { id }.to_bytes(),
                );
            }
        }
        ServerIncoming::RoomMessage(r) => client.inner().emit(Event::RoomMessageReceived(r)),
        ServerIncoming::PublicChat(r) => client.inner().emit(Event::PublicChatMessageReceived {
            room: r.room,
            username: r.username,
            message: r.message,
        }),
        ServerIncoming::UserJoinedRoom(r) => client.inner().emit(Event::RoomJoined {
            room: r.room_name,
            username: r.user_data.username.clone(),
            user_data: Some(r.user_data),
        }),
        ServerIncoming::UserLeftRoom(r) => client.inner().emit(Event::RoomLeft {
            room: r.room_name,
            username: r.username,
        }),
        ServerIncoming::RoomTickers(r) => client.inner().emit(Event::RoomTickerListReceived {
            room: r.room_name,
            tickers: r.tickers,
        }),
        ServerIncoming::RoomTickerAdd(r) => client.inner().emit(Event::RoomTickerAdded {
            room: r.room_name,
            ticker: r.ticker,
        }),
        ServerIncoming::RoomTickerRemove(r) => client.inner().emit(Event::RoomTickerRemoved {
            room: r.room_name,
            username: r.username,
        }),
        ServerIncoming::PrivateRoomUsers(r) => {
            client.inner().emit(Event::PrivateRoomUserListReceived(r))
        }
        ServerIncoming::PrivateRoomOwned(r) => client
            .inner()
            .emit(Event::PrivateRoomModeratedUserListReceived(r)),
        ServerIncoming::PrivateRoomAdded(r) => {
            client.inner().emit(Event::PrivateRoomMembershipAdded(r))
        }
        ServerIncoming::PrivateRoomRemoved(r) => {
            client.inner().emit(Event::PrivateRoomMembershipRemoved(r))
        }
        ServerIncoming::PrivateRoomOperatorAdded(r) => {
            client.inner().emit(Event::PrivateRoomModerationAdded(r))
        }
        ServerIncoming::PrivateRoomOperatorRemoved(r) => {
            client.inner().emit(Event::PrivateRoomModerationRemoved(r))
        }
        ServerIncoming::PrivilegedUsers(r) => {
            client.inner().emit(Event::PrivilegedUserListReceived(r))
        }
        ServerIncoming::GlobalAdminMessage(r) => {
            client.inner().emit(Event::GlobalMessageReceived(r))
        }
        ServerIncoming::ExcludedSearchPhrases(r) => {
            client.inner().emit(Event::ExcludedSearchPhrasesReceived(r))
        }
        _ => {}
    }
}
