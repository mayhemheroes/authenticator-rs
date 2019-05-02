/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::io::{self, Write};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::ctap::{Challenge, CollectedClientData, Origin, WebauthnType};
use crate::ctap2::attestation::{AttestationObject, AttestationStatement};
use crate::ctap2::commands::{
    AssertionObject, GetAssertion, MakeCredentials, MakeCredentialsOptions, Pin,
};
use crate::ctap2::server::{PublicKeyCredentialParameters, RelyingParty, RelyingPartyData, User};
#[cfg(test)]
use crate::transport::platform::TestCase;
use crate::{RegisterFlags, SignFlags};
use consts::PARAMETER_SIZE;
use runloop::RunLoop;
use statemachine::StateMachine;
use util::{OnceCallback, OnceCallbackMap};

enum QueueAction {
    Register {
        timeout: u64,
        params: MakeCredentials,
        callback: OnceCallbackMap<(AttestationObject, CollectedClientData), ::RegisterResult>,
    },
    Sign {
        timeout: u64,
        command: GetAssertion,
        callback: OnceCallbackMap<AssertionObject, ::SignResult>,
    },
    Cancel,
}

pub(crate) enum Capability {
    Fido2 = 2,
}

#[deprecated(note = "U2FManager has been deprecated in favor of Manager")]
pub struct U2FManager {
    queue: RunLoop,
    tx: Sender<QueueAction>,
    filter: Option<Capability>,
}

impl U2FManager {
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = channel();

        // Tests case injection works with thread local storage values,
        // this looks up the value, and reinject it inside the new thread.
        // This is only enabled for tests
        #[cfg(test)]
        let value = TestCase::active();

        // Start a new work queue thread.
        let queue = RunLoop::new(move |alive| {
            #[cfg(test)]
            TestCase::activate(value);

            let mut sm = StateMachine::new();

            while alive() {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(QueueAction::Register {
                        timeout,
                        params,
                        callback,
                    }) => {
                        // This must not block, otherwise we can't cancel.
                        sm.register(timeout, params, callback);
                    }
                    Ok(QueueAction::Sign {
                        timeout,
                        command,
                        callback,
                    }) => {
                        // This must not block, otherwise we can't cancel.
                        sm.sign(timeout, command, callback);
                    }
                    Ok(QueueAction::Cancel) => {
                        // Cancelling must block so that we don't start a new
                        // polling thread before the old one has shut down.
                        sm.cancel();
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        break;
                    }
                    _ => { /* continue */ }
                }
            }

            // Cancel any ongoing activity.
            sm.cancel();
        })?;

        Ok(Self {
            queue,
            tx,
            filter: None,
        })
    }

    pub fn fido2_capable(&mut self) {
        self.filter = Some(Capability::Fido2);
    }

    pub fn register<F>(
        &self,
        flags: ::RegisterFlags,
        timeout: u64,
        challenge: Vec<u8>,
        application: ::AppId,
        key_handles: Vec<::KeyHandle>,
        callback: F,
    ) -> Result<(), ::Error>
    where
        F: FnOnce(Result<::RegisterResult, ::Error>),
        F: Send + 'static,
    {
        if challenge.len() != PARAMETER_SIZE || application.len() != PARAMETER_SIZE {
            return Err(::Error::Unknown);
        }
        let challenge = Challenge::from(challenge);

        let client_data = CollectedClientData {
            type_: WebauthnType::Create,
            challenge,
            origin: Origin::None,
            token_binding: None,
        };

        let mut excluded_handles = Vec::with_capacity(key_handles.len());
        for key_handle in &key_handles {
            if key_handle.credential.len() > 256 {
                return Err(::Error::Unknown);
            }

            excluded_handles.push(key_handle.into());
        }

        let options = MakeCredentialsOptions {
            user_validation: flags.contains(RegisterFlags::REQUIRE_USER_VERIFICATION),
            ..MakeCredentialsOptions::default()
        };

        let callback = OnceCallback::new(callback);
        let callback = callback.map(
            |(attestation_object, collected_client_data): (
                AttestationObject,
                CollectedClientData,
            )| {
                let mut cursor = io::Cursor::new(Vec::new());
                // 1 byte:   register response = 0x05
                cursor
                    .write_all(&[0x05])
                    .expect("unable to write reserved byte");

                let credential_data = attestation_object.auth_data.credential_data.unwrap();
                // 64 bytes: public_key
                cursor
                    .write_all(&credential_data.credential_public_key.bytes[..])
                    .expect("unable to write public_key");

                // 1 byte: key_handle_len
                cursor
                    .write_all(&[credential_data.credential_id.len() as u8])
                    .expect("unable to write key_handle_len");

                // N bytes: Key_handle
                cursor
                    .write_all(&credential_data.credential_id[..])
                    .expect("unable to write key_handle");

                // N bytes: attestation
                let u2f = match attestation_object.att_statement {
                    AttestationStatement::FidoU2F(u2f) => u2f,
                    _ => panic!("u2f statement format expected"),
                };
                cursor
                    .write_all(u2f.attestation_cert[0].as_ref())
                    .expect("unable to write attestation");
                // N bytes: signature
                cursor
                    .write_all(u2f.sig.as_ref())
                    .expect("unable to write signature");

                cursor.into_inner()
            },
        );

        let rp = RelyingParty::new_hash(&application).map_err(|_| ::Error::Unknown)?;

        let register = MakeCredentials::new(
            client_data,
            rp,
            None,
            Vec::new(),
            excluded_handles,
            None,
            None,
        );

        let action = QueueAction::Register {
            timeout,
            params: register,
            callback,
        };
        self.tx.send(action).map_err(|_| ::Error::Unknown)
    }

    pub fn sign<F>(
        &self,
        flags: SignFlags,
        timeout: u64,
        challenge: Vec<u8>,
        app_ids: Vec<::AppId>,
        key_handles: Vec<::KeyHandle>,
        callback: F,
    ) -> Result<(), ::Error>
    where
        F: FnOnce(Result<::SignResult, ::Error>),
        F: Send + 'static,
    {
        if challenge.len() != PARAMETER_SIZE {
            return Err(::Error::Unknown);
        }

        let challenge = Challenge::from(challenge);
        let callback = OnceCallback::new(callback);

        if app_ids.is_empty() {
            return Err(::Error::Unknown);
        }

        let client_data = CollectedClientData {
            type_: WebauthnType::Get,
            challenge,
            origin: Origin::None,
            token_binding: None,
        };

        // TODO(baloo): This block of code and commend was previously in src/statemanchine.rs
        //              I moved this logic here, and I'm not quite sure about what we
        //              should do, have to ask jcj
        //
        // We currently support none of the authenticator selection
        // criteria because we can't ask tokens whether they do support
        // those features. If flags are set, ignore all tokens for now.
        //
        // Technically, this is a ConstraintError because we shouldn't talk
        // to this authenticator in the first place. But the result is the
        // same anyway.
        //if !flags.is_empty() {
        //    return;
        //}
        let options = MakeCredentialsOptions {
            // TODO(baloo): user_validation is required for yubikeys, not sure why
            //user_validation: flags.contains(SignFlags::REQUIRE_USER_VERIFICATION),
            user_validation: true,
            ..MakeCredentialsOptions::default()
        };

        for app_id in &app_ids {
            for key_handle in &key_handles {
                if key_handle.credential.len() > 256 {
                    return Err(::Error::Unknown);
                }
                let rp = RelyingParty::new_hash(app_id).map_err(|_| ::Error::Unknown)?;

                let allow_list = vec![key_handle.into()];

                let command =
                    GetAssertion::new(client_data.clone(), rp, allow_list, Some(options), None);

                let app_id = app_id.clone();
                let key_handle = key_handle.credential.clone();
                let callback = callback.clone();

                let callback = callback.map(move |assertion_object: AssertionObject| {
                    (app_id, key_handle, assertion_object.u2f_sign_data())
                });

                let action = QueueAction::Sign {
                    command,
                    timeout,
                    callback,
                };
                self.tx.send(action).map_err(|_| ::Error::Unknown)?;
            }
        }
        Ok(())
    }

    pub fn cancel(&self) -> Result<(), ::Error> {
        self.tx
            .send(QueueAction::Cancel)
            .map_err(|_| ::Error::Unknown)
    }
}

impl Drop for U2FManager {
    fn drop(&mut self) {
        self.queue.cancel();
    }
}
