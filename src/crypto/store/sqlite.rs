// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::mem;
use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

use async_trait::async_trait;
use olm_rs::PicklingMode;
use serde_json;
use sqlx::{query, query_as, sqlite::SqliteQueryAs, Connect, Executor, SqliteConnection};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use super::{Account, CryptoStore, CryptoStoreError, InboundGroupSession, Result, Session};
use crate::api::r0::keys::KeyAlgorithm;
use crate::crypto::device::{Device, TrustState};
use crate::crypto::memory_stores::{DeviceStore, GroupSessionStore, SessionStore, UserDevices};
use crate::events::Algorithm;
use crate::identifiers::{DeviceId, RoomId, UserId};

pub struct SqliteStore {
    user_id: Arc<String>,
    device_id: Arc<String>,
    account_id: Option<i64>,
    path: PathBuf,

    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    devices: DeviceStore,
    tracked_users: HashSet<UserId>,

    connection: Arc<Mutex<SqliteConnection>>,
    pickle_passphrase: Option<Zeroizing<String>>,
}

static DATABASE_NAME: &str = "matrix-sdk-crypto.db";

impl SqliteStore {
    pub async fn open<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &str,
        path: P,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, None).await
    }

    pub async fn open_with_passphrase<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &str,
        path: P,
        passphrase: String,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, Some(Zeroizing::new(passphrase))).await
    }

    fn path_to_url(path: &Path) -> Result<Url> {
        // TODO this returns an empty error if the path isn't absolute.
        let url = Url::from_directory_path(path).expect("Invalid path");
        Ok(url.join(DATABASE_NAME)?)
    }

    async fn open_helper<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &str,
        path: P,
        passphrase: Option<Zeroizing<String>>,
    ) -> Result<SqliteStore> {
        let url = SqliteStore::path_to_url(path.as_ref())?;

        let connection = SqliteConnection::connect(url.as_ref()).await?;
        let store = SqliteStore {
            user_id: Arc::new(user_id.to_string()),
            device_id: Arc::new(device_id.to_owned()),
            account_id: None,
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            devices: DeviceStore::new(),
            path: path.as_ref().to_owned(),
            connection: Arc::new(Mutex::new(connection)),
            pickle_passphrase: passphrase,
            tracked_users: HashSet::new(),
        };
        store.create_tables().await?;
        Ok(store)
    }

    async fn create_tables(&self) -> Result<()> {
        let mut connection = self.connection.lock().await;
        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS accounts (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                "shared" INTEGER NOT NULL,
                UNIQUE(user_id,device_id)
            );
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS sessions (
                "session_id" TEXT NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "creation_time" TEXT NOT NULL,
                "last_use_time" TEXT NOT NULL,
                "sender_key" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS "olmsessions_account_id" ON "sessions" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS inbound_group_sessions (
                "session_id" TEXT NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "sender_key" TEXT NOT NULL,
                "signing_key" TEXT NOT NULL,
                "room_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS "olm_groups_sessions_account_id" ON "inbound_group_sessions" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS devices (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "display_name" TEXT,
                "trust_state" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id,device_id)
            );

            CREATE INDEX IF NOT EXISTS "devices_account_id" ON "devices" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS algorithms (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "algorithms_device_id" ON "algorithms" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS device_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                "key" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "device_keys_device_id" ON "device_keys" ("device_id");
        "#,
            )
            .await?;

        Ok(())
    }

    async fn lazy_load_sessions(&mut self, sender_key: &str) -> Result<()> {
        let loaded_sessions = self.sessions.get(sender_key).is_some();

        if !loaded_sessions {
            let sessions = self.load_sessions_for(sender_key).await?;

            if !sessions.is_empty() {
                self.sessions.set_for_sender(sender_key, sessions);
            }
        }

        Ok(())
    }

    async fn get_sessions_for(
        &mut self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        self.lazy_load_sessions(sender_key).await?;
        Ok(self.sessions.get(sender_key))
    }

    async fn load_sessions_for(&mut self, sender_key: &str) -> Result<Vec<Session>> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, creation_time, last_use_time
             FROM sessions WHERE account_id = ? and sender_key = ?",
        )
        .bind(account_id)
        .bind(sender_key)
        .fetch_all(&mut *connection)
        .await?;

        let now = Instant::now();

        Ok(rows
            .iter()
            .map(|row| {
                let pickle = &row.0;
                let sender_key = &row.1;
                let creation_time = now
                    .checked_sub(serde_json::from_str::<Duration>(&row.2)?)
                    .ok_or(CryptoStoreError::SessionTimestampError)?;
                let last_use_time = now
                    .checked_sub(serde_json::from_str::<Duration>(&row.3)?)
                    .ok_or(CryptoStoreError::SessionTimestampError)?;

                Ok(Session::from_pickle(
                    pickle.to_string(),
                    self.get_pickle_mode(),
                    sender_key.to_string(),
                    creation_time,
                    last_use_time,
                )?)
            })
            .collect::<Result<Vec<Session>>>()?)
    }

    async fn load_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, signing_key, room_id
             FROM inbound_group_sessions WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                let pickle = &row.0;
                let sender_key = &row.1;
                let signing_key = &row.2;
                let room_id = &row.3;

                Ok(InboundGroupSession::from_pickle(
                    pickle.to_string(),
                    self.get_pickle_mode(),
                    sender_key.to_string(),
                    signing_key.to_owned(),
                    RoomId::try_from(room_id.as_str()).unwrap(),
                )?)
            })
            .collect::<Result<Vec<InboundGroupSession>>>()?)
    }

    async fn load_devices(&self) -> Result<DeviceStore> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(i64, String, String, Option<String>, i64)> = query_as(
            "SELECT id, user_id, device_id, display_name, trust_state
             FROM devices WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        let store = DeviceStore::new();

        for row in rows {
            let device_row_id = row.0;
            let user_id = if let Ok(u) = UserId::try_from(&row.1 as &str) {
                u
            } else {
                continue;
            };

            let device_id = &row.2.to_string();
            let display_name = &row.3;
            let trust_state = TrustState::from(row.4);

            let algorithm_rows: Vec<(String,)> =
                query_as("SELECT algorithm FROM algorithms WHERE device_id = ?")
                    .bind(device_row_id)
                    .fetch_all(&mut *connection)
                    .await?;

            let algorithms = algorithm_rows
                .iter()
                .map(|row| Algorithm::from(&row.0 as &str))
                .collect::<Vec<Algorithm>>();

            let key_rows: Vec<(String, String)> =
                query_as("SELECT algorithm, key FROM device_keys WHERE device_id = ?")
                    .bind(device_row_id)
                    .fetch_all(&mut *connection)
                    .await?;

            let mut keys = HashMap::new();

            for row in key_rows {
                let algorithm = if let Ok(a) = KeyAlgorithm::try_from(&row.0 as &str) {
                    a
                } else {
                    continue;
                };

                let key = &row.1;

                keys.insert(algorithm, key.to_owned());
            }

            let device = Device::new(
                user_id,
                device_id.to_owned(),
                display_name.clone(),
                trust_state,
                algorithms,
                keys,
            );

            store.add(device);
        }

        Ok(store)
    }

    async fn save_device_helper(&self, device: Device) -> Result<()> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;

        let mut connection = self.connection.lock().await;

        query(
            "INSERT INTO devices (
                account_id, user_id, device_id,
                display_name, trust_state
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, user_id, device_id) DO UPDATE SET
                display_name = excluded.display_name,
                trust_state = excluded.trust_state
             ",
        )
        .bind(account_id)
        .bind(&device.user_id().to_string())
        .bind(device.device_id())
        .bind(device.display_name())
        .bind(device.trust_state() as i64)
        .execute(&mut *connection)
        .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM devices
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(&device.user_id().to_string())
        .bind(device.device_id())
        .fetch_one(&mut *connection)
        .await?;

        let device_row_id = row.0;

        for algorithm in device.algorithms() {
            query(
                "INSERT OR IGNORE INTO algorithms (
                    device_id, algorithm
                 ) VALUES (?1, ?2)
                 ",
            )
            .bind(device_row_id)
            .bind(algorithm.to_string())
            .execute(&mut *connection)
            .await?;
        }

        for (key_algorithm, key) in device.keys() {
            query(
                "INSERT OR IGNORE INTO device_keys (
                    device_id, algorithm, key
                 ) VALUES (?1, ?2, ?3)
                 ",
            )
            .bind(device_row_id)
            .bind(key_algorithm.to_string())
            .bind(key)
            .execute(&mut *connection)
            .await?;
        }

        Ok(())
    }

    fn get_pickle_mode(&self) -> PicklingMode {
        match &self.pickle_passphrase {
            Some(p) => PicklingMode::Encrypted {
                key: p.as_bytes().to_vec(),
            },
            None => PicklingMode::Unencrypted,
        }
    }
}

#[async_trait]
impl CryptoStore for SqliteStore {
    async fn load_account(&mut self) -> Result<Option<Account>> {
        let mut connection = self.connection.lock().await;

        let row: Option<(i64, String, bool)> = query_as(
            "SELECT id, pickle, shared FROM accounts
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(&*self.user_id)
        .bind(&*self.device_id)
        .fetch_optional(&mut *connection)
        .await?;

        let result = if let Some((id, pickle, shared)) = row {
            self.account_id = Some(id);
            Some(Account::from_pickle(
                pickle,
                self.get_pickle_mode(),
                shared,
            )?)
        } else {
            return Ok(None);
        };

        drop(connection);

        let mut group_sessions = self.load_inbound_group_sessions().await?;

        let _ = group_sessions
            .drain(..)
            .map(|s| {
                self.inbound_group_sessions.add(s);
            })
            .collect::<()>();

        let devices = self.load_devices().await?;
        mem::replace(&mut self.devices, devices);

        // TODO load the tracked users here as well.

        Ok(result)
    }

    async fn save_account(&mut self, account: Account) -> Result<()> {
        let pickle = account.pickle(self.get_pickle_mode()).await;
        let mut connection = self.connection.lock().await;

        query(
            "INSERT INTO accounts (
                user_id, device_id, pickle, shared
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, device_id) DO UPDATE SET
                pickle = excluded.pickle,
                shared = excluded.shared
             ",
        )
        .bind(&*self.user_id.to_string())
        .bind(&*self.device_id.to_string())
        .bind(&pickle)
        .bind(account.shared())
        .execute(&mut *connection)
        .await?;

        let account_id: (i64,) =
            query_as("SELECT id FROM accounts WHERE user_id = ? and device_id = ?")
                .bind(&*self.user_id.to_string())
                .bind(&*self.device_id.to_string())
                .fetch_one(&mut *connection)
                .await?;

        self.account_id = Some(account_id.0);

        Ok(())
    }

    async fn save_session(&mut self, session: Session) -> Result<()> {
        self.lazy_load_sessions(&session.sender_key).await?;
        self.sessions.add(session.clone()).await;

        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;

        let session_id = session.session_id();
        let creation_time = serde_json::to_string(&session.creation_time.elapsed())?;
        let last_use_time = serde_json::to_string(&session.last_use_time.elapsed())?;
        let pickle = session.pickle(self.get_pickle_mode()).await;

        let mut connection = self.connection.lock().await;

        query(
            "REPLACE INTO sessions (
                session_id, account_id, creation_time, last_use_time, sender_key, pickle
             ) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&session_id)
        .bind(&account_id)
        .bind(&*creation_time)
        .bind(&*last_use_time)
        .bind(&*session.sender_key)
        .bind(&pickle)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn get_sessions(&mut self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        Ok(self.get_sessions_for(sender_key).await?)
    }

    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<bool> {
        let account_id = self.account_id.ok_or(CryptoStoreError::AccountUnset)?;
        let pickle = session.pickle(self.get_pickle_mode()).await;
        let mut connection = self.connection.lock().await;
        let session_id = session.session_id();

        query(
            "INSERT INTO inbound_group_sessions (
                session_id, account_id, sender_key, signing_key,
                room_id, pickle
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
                pickle = excluded.pickle
             ",
        )
        .bind(session_id)
        .bind(account_id)
        .bind(&*session.sender_key)
        .bind(&*session.signing_key)
        .bind(&*session.room_id.to_string())
        .bind(&pickle)
        .execute(&mut *connection)
        .await?;

        Ok(self.inbound_group_sessions.add(session))
    }

    async fn get_inbound_group_session(
        &mut self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self
            .inbound_group_sessions
            .get(room_id, sender_key, session_id))
    }

    fn tracked_users(&self) -> &HashSet<UserId> {
        &self.tracked_users
    }

    async fn add_user_for_tracking(&mut self, user: &UserId) -> Result<bool> {
        // TODO save the tracked user to the database.
        Ok(self.tracked_users.insert(user.clone()))
    }

    async fn save_device(&self, device: Device) -> Result<()> {
        self.devices.add(device.clone());
        self.save_device_helper(device).await
    }

    async fn delete_device(&self, device: Device) -> Result<()> {
        todo!()
    }

    async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<Option<Device>> {
        Ok(self.devices.get(user_id, device_id))
    }

    async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        Ok(self.devices.user_devices(user_id))
    }
}

#[cfg_attr(tarpaulin, skip)]
impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("SqliteStore")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use crate::api::r0::keys::SignedKey;
    use crate::crypto::device::test::get_device;
    use crate::crypto::olm::GroupSessionKey;
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use std::collections::HashMap;
    use tempfile::tempdir;

    use super::{
        Account, CryptoStore, InboundGroupSession, RoomId, Session, SqliteStore, TryFrom, UserId,
    };

    static USER_ID: &str = "@example:localhost";
    static DEVICE_ID: &str = "DEVICEID";

    async fn get_store(passphrase: Option<&str>) -> (SqliteStore, tempfile::TempDir) {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();

        let user_id = &UserId::try_from(USER_ID).unwrap();

        let store = if let Some(passphrase) = passphrase {
            SqliteStore::open_with_passphrase(
                &user_id,
                DEVICE_ID,
                tmpdir_path,
                passphrase.to_owned(),
            )
            .await
            .expect("Can't create a passphrase protected store")
        } else {
            SqliteStore::open(&user_id, DEVICE_ID, tmpdir_path)
                .await
                .expect("Can't create store")
        };

        (store, tmpdir)
    }

    async fn get_loaded_store() -> (Account, SqliteStore, tempfile::TempDir) {
        let (mut store, dir) = get_store(None).await;
        let account = get_account();
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        (account, store, dir)
    }

    fn get_account() -> Account {
        Account::new()
    }

    async fn get_account_and_session() -> (Account, Session) {
        let alice = Account::new();

        let bob = Account::new();

        bob.generate_one_time_keys(1).await;
        let one_time_key = bob
            .one_time_keys()
            .await
            .curve25519()
            .iter()
            .nth(0)
            .unwrap()
            .1
            .to_owned();
        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: HashMap::new(),
        };
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session = alice
            .create_outbound_session(&sender_key, &one_time_key)
            .await
            .unwrap();

        (alice, session)
    }

    #[tokio::test]
    async fn create_store() {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        let _ = SqliteStore::open(&UserId::try_from(USER_ID).unwrap(), "DEVICEID", tmpdir_path)
            .await
            .expect("Can't create store");
    }

    #[tokio::test]
    async fn save_account() {
        let (mut store, _dir) = get_store(None).await;
        assert!(store.load_account().await.unwrap().is_none());
        let account = get_account();

        store
            .save_account(account)
            .await
            .expect("Can't save account");
    }

    #[tokio::test]
    async fn load_account() {
        let (mut store, _dir) = get_store(None).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test]
    async fn load_account_with_passphrase() {
        let (mut store, _dir) = get_store(Some("secret_passphrase")).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test]
    async fn save_and_share_account() {
        let (mut store, _dir) = get_store(None).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        account.mark_as_shared();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test]
    async fn save_session() {
        let (mut store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;

        assert!(store.save_session(session.clone()).await.is_err());

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        store.save_session(session).await.unwrap();
    }

    #[tokio::test]
    async fn load_sessions() {
        let (mut store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_session(session.clone()).await.unwrap();

        let sessions = store
            .load_sessions_for(&session.sender_key)
            .await
            .expect("Can't load sessions");
        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[tokio::test]
    async fn add_and_save_session() {
        let (mut store, dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        let sender_key = session.sender_key.to_owned();
        let session_id = session.session_id().to_owned();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_session(session).await.unwrap();

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());

        drop(store);

        let mut store =
            SqliteStore::open(&UserId::try_from(USER_ID).unwrap(), DEVICE_ID, dir.path())
                .await
                .expect("Can't create store");

        let loaded_account = store.load_account().await.unwrap().unwrap();
        assert_eq!(account, loaded_account);

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());
    }

    #[tokio::test]
    async fn save_inbound_group_session() {
        let (account, mut store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &RoomId::try_from("!test:localhost").unwrap(),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        store
            .save_inbound_group_session(session)
            .await
            .expect("Can't save group session");
    }

    #[tokio::test]
    async fn load_inbound_group_session() {
        let (account, mut store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &RoomId::try_from("!test:localhost").unwrap(),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        let session_id = session.session_id().to_owned();

        store
            .save_inbound_group_session(session.clone())
            .await
            .expect("Can't save group session");

        let sessions = store.load_inbound_group_sessions().await.unwrap();

        assert_eq!(session_id, sessions[0].session_id());

        let loaded_session = store
            .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session, loaded_session);
    }

    #[tokio::test]
    async fn test_tracked_users() {
        let (_account, mut store, _dir) = get_loaded_store().await;
        let device = get_device();

        assert!(store.add_user_for_tracking(device.user_id()).await.unwrap());
        assert!(!store.add_user_for_tracking(device.user_id()).await.unwrap());

        let tracked_users = store.tracked_users();

        tracked_users.contains(device.user_id());
    }

    #[tokio::test]
    async fn device_saving() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        store.save_device(device.clone()).await.unwrap();

        drop(store);

        let mut store =
            SqliteStore::open(&UserId::try_from(USER_ID).unwrap(), DEVICE_ID, dir.path())
                .await
                .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store
            .get_device(device.user_id(), device.device_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(device, loaded_device);

        for algorithm in loaded_device.algorithms() {
            assert!(device.algorithms().contains(algorithm));
        }
        assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
        assert_eq!(device.keys(), loaded_device.keys());

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
        assert_eq!(user_devices.keys().nth(0).unwrap(), device.device_id());
        assert_eq!(user_devices.devices().nth(0).unwrap(), &device);
    }
}
