// Copyright 2021 Damir Jelić
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

mod double_ratchet;

use x25519_dalek::PublicKey as Curve25591PublicKey;

use double_ratchet::{LocalDoubleRatchet, RemoteDoubleRatchet, RemoteRatchetKey};
pub use double_ratchet::{OlmMessage, PrekeyMessage, RemoteShared3DHSecret, Shared3DHSecret};

pub(super) struct SessionKeys {
    identity_key: Curve25591PublicKey,
    ephemeral_key: Curve25591PublicKey,
    one_time_key: Curve25591PublicKey,
}

impl SessionKeys {
    pub(super) fn new(
        identity_key: Curve25591PublicKey,
        ephemeral_key: Curve25591PublicKey,
        one_time_key: Curve25591PublicKey,
    ) -> Self {
        Self {
            identity_key,
            ephemeral_key,
            one_time_key,
        }
    }
}

pub struct Session {
    session_keys: Option<SessionKeys>,
    sending_ratchet: LocalDoubleRatchet,
    receiving_ratchet: Option<RemoteDoubleRatchet>,
}

impl Session {
    pub(super) fn new(shared_secret: Shared3DHSecret, session_keys: SessionKeys) -> Self {
        let local_ratchet = LocalDoubleRatchet::active(shared_secret);

        Self {
            session_keys: Some(session_keys),
            sending_ratchet: local_ratchet,
            receiving_ratchet: None,
        }
    }

    pub(super) fn new_remote(
        shared_secret: RemoteShared3DHSecret,
        remote_ratchet_key: RemoteRatchetKey,
    ) -> Self {
        let (root_key, remote_chain_key) = shared_secret.expand();

        let local_ratchet = LocalDoubleRatchet::inactive(root_key, remote_ratchet_key.clone());
        let remote_ratchet = RemoteDoubleRatchet::new(remote_ratchet_key, remote_chain_key);

        Self {
            session_keys: None,
            sending_ratchet: local_ratchet,
            receiving_ratchet: Some(remote_ratchet),
        }
    }

    pub fn encrypt(&mut self, plaintext: &str) -> Vec<u8> {
        let message = match &mut self.sending_ratchet {
            LocalDoubleRatchet::Inactive(ratchet) => {
                let mut ratchet = ratchet.activate();
                let message = ratchet.encrypt(plaintext.as_bytes());
                self.sending_ratchet = LocalDoubleRatchet::Active(ratchet);
                message
            }
            LocalDoubleRatchet::Active(ratchet) => ratchet.encrypt(plaintext.as_bytes()),
        };

        if let Some(session_keys) = &self.session_keys {
            PrekeyMessage::from_parts(
                &session_keys.one_time_key,
                &session_keys.ephemeral_key,
                &session_keys.identity_key,
                message.into_vec(),
            )
            .into_vec()
        } else {
            message.into_vec()
        }
    }

    pub fn decrypt_prekey(&mut self, message: Vec<u8>) -> Vec<u8> {
        let message = PrekeyMessage::from(message);
        let (_, _, _, message) = message.decode().unwrap();

        self.decrypt(message)
    }

    pub fn decrypt(&mut self, message: Vec<u8>) -> Vec<u8> {
        let message = OlmMessage::from(message);
        let decoded = message.decode().unwrap();

        // TODO try to use existing message keys.

        if !self
            .receiving_ratchet
            .as_ref()
            .map_or(false, |r| r.belongs_to(&decoded.ratchet_key))
        {
            let (sending_ratchet, mut remote_ratchet) =
                self.sending_ratchet.advance(decoded.ratchet_key);

            // TODO don't update the state if the message doesn't decrypt
            let plaintext = remote_ratchet.decrypt(&message, &decoded.ciphertext, decoded.mac);

            self.sending_ratchet = LocalDoubleRatchet::Inactive(sending_ratchet);
            self.receiving_ratchet = Some(remote_ratchet);
            self.session_keys = None;

            plaintext
        } else if let Some(ref mut remote_ratchet) = self.receiving_ratchet {
            remote_ratchet.decrypt(&message, &decoded.ciphertext, decoded.mac)
        } else {
            todo!()
        }
    }
}