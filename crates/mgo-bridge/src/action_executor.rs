// Copyright (c) MangoNet Labs Ltd.
// SPDX-License-Identifier: Apache-2.0

//! BridgeActionExecutor receives BridgeActions (from BridgeOrchestrator),
//! collects bridge authority signatures and submit signatures on chain.

use mango_metrics::spawn_logged_monitored_task;
use shared_crypto::intent::{Intent, IntentMessage};
use mgo_json_rpc_types::{
    MgoExecutionStatus, MgoTransactionBlockEffects, MgoTransactionBlockEffectsAPI,
};
use mgo_types::{
    base_types::{ObjectID, ObjectRef, MgoAddress},
    committee::VALIDITY_THRESHOLD,
    crypto::{Signature, MgoKeyPair},
    digests::TransactionDigest,
    gas_coin::GasCoin,
    object::Owner,
    transaction::Transaction,
};

use crate::{
    client::bridge_authority_aggregator::BridgeAuthorityAggregator,
    error::BridgeError,
    storage::BridgeOrchestratorTables,
    mgo_client::{MgoClient, MgoClientInner},
    mgo_transaction_builder::build_transaction,
    types::{BridgeAction, BridgeActionStatus, VerifiedCertifiedBridgeAction},
};
use std::sync::Arc;
use tracing::{error, info, warn};

pub const CHANNEL_SIZE: usize = 1000;

// delay schedule: at most 16 times including the initial attempt
// 0.1s, 0.2s, 0.4s, 0.8s, 1.6s, 3.2s, 6.4s, 12.8s, 25.6s, 51.2s, 102.4s, 204.8s, 409.6s, 819.2s, 1638.4s
pub const MAX_SIGNING_ATTEMPTS: u64 = 16;
pub const MAX_EXECUTION_ATTEMPTS: u64 = 16;

async fn delay(attempt_times: u64) {
    let delay_ms = 100 * (2 ^ attempt_times);
    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
}

#[derive(Debug)]
pub struct BridgeActionExecutionWrapper(pub BridgeAction, pub u64);

#[derive(Debug)]
pub struct CertifiedBridgeActionExecutionWrapper(pub VerifiedCertifiedBridgeAction, pub u64);

pub trait BridgeActionExecutorTrait {
    fn run(
        self,
    ) -> (
        Vec<tokio::task::JoinHandle<()>>,
        mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
    );
}

pub struct BridgeActionExecutor<C> {
    mgo_client: Arc<MgoClient<C>>,
    bridge_auth_agg: Arc<BridgeAuthorityAggregator>,
    key: MgoKeyPair,
    mgo_address: MgoAddress,
    gas_object_id: ObjectID,
    store: Arc<BridgeOrchestratorTables>,
}

impl<C> BridgeActionExecutorTrait for BridgeActionExecutor<C>
where
    C: MgoClientInner + 'static,
{
    fn run(
        self,
    ) -> (
        Vec<tokio::task::JoinHandle<()>>,
        mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
    ) {
        let (tasks, sender, _) = self.run_inner();
        (tasks, sender)
    }
}

impl<C> BridgeActionExecutor<C>
where
    C: MgoClientInner + 'static,
{
    pub fn new(
        mgo_client: Arc<MgoClient<C>>,
        bridge_auth_agg: Arc<BridgeAuthorityAggregator>,
        store: Arc<BridgeOrchestratorTables>,
        key: MgoKeyPair,
        mgo_address: MgoAddress,
        gas_object_id: ObjectID,
    ) -> Self {
        Self {
            mgo_client,
            bridge_auth_agg,
            store,
            key,
            gas_object_id,
            mgo_address,
        }
    }

    fn run_inner(
        self,
    ) -> (
        Vec<tokio::task::JoinHandle<()>>,
        mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
        mango_metrics::metered_channel::Sender<CertifiedBridgeActionExecutionWrapper>,
    ) {
        let key = self.key;

        let (sender, receiver) = mango_metrics::metered_channel::channel(
            CHANNEL_SIZE,
            &mango_metrics::get_metrics()
                .unwrap()
                .channels
                .with_label_values(&["executor_signing_queue"]),
        );

        let (execution_tx, execution_rx) = mango_metrics::metered_channel::channel(
            CHANNEL_SIZE,
            &mango_metrics::get_metrics()
                .unwrap()
                .channels
                .with_label_values(&["executor_execution_queue"]),
        );
        let execution_tx_clone = execution_tx.clone();
        let sender_clone = sender.clone();
        let store_clone = self.store.clone();
        let client_clone = self.mgo_client.clone();
        let mut tasks = vec![];
        tasks.push(spawn_logged_monitored_task!(
            Self::run_signature_aggregation_loop(
                client_clone,
                self.bridge_auth_agg,
                store_clone,
                sender_clone,
                receiver,
                execution_tx_clone,
            )
        ));

        let execution_tx_clone = execution_tx.clone();
        tasks.push(spawn_logged_monitored_task!(
            Self::run_onchain_execution_loop(
                self.mgo_client.clone(),
                key,
                self.mgo_address,
                self.gas_object_id,
                self.store.clone(),
                execution_tx_clone,
                execution_rx,
            )
        ));
        (tasks, sender, execution_tx)
    }

    async fn run_signature_aggregation_loop(
        mgo_client: Arc<MgoClient<C>>,
        auth_agg: Arc<BridgeAuthorityAggregator>,
        store: Arc<BridgeOrchestratorTables>,
        signing_queue_sender: mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
        mut signing_queue_receiver: mango_metrics::metered_channel::Receiver<
            BridgeActionExecutionWrapper,
        >,
        execution_queue_sender: mango_metrics::metered_channel::Sender<
            CertifiedBridgeActionExecutionWrapper,
        >,
    ) {
        info!("Starting run_signature_aggregation_loop");
        while let Some(action) = signing_queue_receiver.recv().await {
            info!("Received action for signing: {:?}", action);
            let auth_agg_clone = auth_agg.clone();
            let signing_queue_sender_clone = signing_queue_sender.clone();
            let execution_queue_sender_clone = execution_queue_sender.clone();
            let mgo_client_clone = mgo_client.clone();
            let store_clone = store.clone();
            spawn_logged_monitored_task!(Self::request_signature(
                mgo_client_clone,
                auth_agg_clone,
                action,
                store_clone,
                signing_queue_sender_clone,
                execution_queue_sender_clone
            ));
        }
    }

    // Checks if the action is already processed on chain.
    // If yes, skip this action and remove it from the pending log.
    // Returns true if the action is already processed.
    async fn handle_already_processed_action_maybe(
        mgo_client: &Arc<MgoClient<C>>,
        action: &BridgeAction,
        store: &Arc<BridgeOrchestratorTables>,
    ) -> bool {
        let status = mgo_client
            .get_action_onchain_status_until_success(action)
            .await;
        match status {
            BridgeActionStatus::Approved | BridgeActionStatus::Claimed => {
                info!(
                    "Action already approved or claimed, removing action from pending logs: {:?}",
                    action
                );
                store
                    .remove_pending_actions(&[action.digest()])
                    .unwrap_or_else(|e| {
                        panic!("Write to DB should not fail: {:?}", e);
                    });
                true
            }
            // Although theoretically a legit MgoToEthBridgeAction should not have
            // status `RecordNotFound`
            BridgeActionStatus::Pending | BridgeActionStatus::RecordNotFound => false,
        }
    }

    async fn request_signature(
        mgo_client: Arc<MgoClient<C>>,
        auth_agg: Arc<BridgeAuthorityAggregator>,
        action: BridgeActionExecutionWrapper,
        store: Arc<BridgeOrchestratorTables>,
        signing_queue_sender: mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
        execution_queue_sender: mango_metrics::metered_channel::Sender<
            CertifiedBridgeActionExecutionWrapper,
        >,
    ) {
        let BridgeActionExecutionWrapper(action, attempt_times) = action;
        // If the action is already processed, skip it.
        if Self::handle_already_processed_action_maybe(&mgo_client, &action, &store).await {
            return;
        }

        // TODO: use different threshold based on action types.
        match auth_agg
            .request_committee_signatures(action.clone(), VALIDITY_THRESHOLD)
            .await
        {
            Ok(certificate) => {
                execution_queue_sender
                    .send(CertifiedBridgeActionExecutionWrapper(certificate, 0))
                    .await
                    .expect("Sending to execution queue should not fail");
            }
            Err(e) => {
                warn!("Failed to collect sigs for bridge action: {:?}", e);

                if attempt_times >= MAX_SIGNING_ATTEMPTS {
                    error!("Manual intervention is required. Failed to collect sigs for bridge action after {MAX_SIGNING_ATTEMPTS} attempts: {:?}", e);
                    return;
                }
                delay(attempt_times).await;
                signing_queue_sender
                    .send(BridgeActionExecutionWrapper(action, attempt_times + 1))
                    .await
                    .expect("Sending to signing queue should not fail");
            }
        }
    }

    // Before calling this function, `key` and `mgo_address` need to be
    // verified to match.
    async fn run_onchain_execution_loop(
        mgo_client: Arc<MgoClient<C>>,
        mgo_key: MgoKeyPair,
        mgo_address: MgoAddress,
        gas_object_id: ObjectID,
        store: Arc<BridgeOrchestratorTables>,
        execution_queue_sender: mango_metrics::metered_channel::Sender<
            CertifiedBridgeActionExecutionWrapper,
        >,
        mut execution_queue_receiver: mango_metrics::metered_channel::Receiver<
            CertifiedBridgeActionExecutionWrapper,
        >,
    ) {
        info!("Starting run_onchain_execution_loop");
        while let Some(certificate_wrapper) = execution_queue_receiver.recv().await {
            info!(
                "Received certified action for execution: {:?}",
                certificate_wrapper
            );
            let CertifiedBridgeActionExecutionWrapper(certificate, attempt_times) =
                certificate_wrapper;

            let action = certificate.data();
            // If the action is already processed, skip it.
            if Self::handle_already_processed_action_maybe(&mgo_client, action, &store).await {
                return;
            }

            // TODO check gas coin balance here. If gas balance too low, do not proceed.
            let (_gas_coin, gas_object_ref) =
                Self::get_gas_data_assert_ownership(mgo_address, gas_object_id, &mgo_client).await;
            let ceriticate_clone = certificate.clone();
            let tx_data = match build_transaction(mgo_address, &gas_object_ref, ceriticate_clone) {
                Ok(tx_data) => tx_data,
                Err(err) => {
                    // TODO: add mertrics
                    error!(
                        "Failed to build transaction for action {:?}: {:?}",
                        certificate, err
                    );
                    // This should not happen, but in case it does, we do not want to
                    // panic, instead we log here for manual intervention.
                    continue;
                }
            };
            let sig = Signature::new_secure(
                &IntentMessage::new(Intent::mgo_transaction(), &tx_data),
                &mgo_key,
            );
            let signed_tx = Transaction::from_data(tx_data, vec![sig]);
            let tx_digest = *signed_tx.digest();

            info!(?tx_digest, ?gas_object_ref, "Sending transaction to Mgo");
            // TODO: add metrics to detect low balances and so on
            match mgo_client
                .execute_transaction_block_with_effects(signed_tx)
                .await
            {
                Ok(effects) => {
                    let effects = effects.effects.expect("We requested effects but got None.");
                    Self::handle_execution_effects(tx_digest, effects, &store, action).await
                }

                // If the transaction did not go through, retry up to a certain times.
                Err(err) => {
                    error!("Mgo transaction failed at signing: {err:?}");

                    // Do this in a separate task so we won't deadlock here
                    let sender_clone = execution_queue_sender.clone();
                    spawn_logged_monitored_task!(async move {
                        // TODO: metrics + alerts
                        // If it fails for too many times, log and ask for manual intervention.
                        if attempt_times >= MAX_EXECUTION_ATTEMPTS {
                            error!("Manual intervention is required. Failed to collect execute transaction for bridge action after {MAX_EXECUTION_ATTEMPTS} attempts: {:?}", err);
                            return;
                        }
                        delay(attempt_times).await;
                        sender_clone
                            .send(CertifiedBridgeActionExecutionWrapper(
                                certificate,
                                attempt_times + 1,
                            ))
                            .await
                            .expect("Sending to execution queue should not fail");
                        info!("Re-enqueued certificate for execution");
                    });
                }
            }
        }
    }

    // TODO: do we need a mechanism to periodically read pending actions from DB?
    async fn handle_execution_effects(
        tx_digest: TransactionDigest,
        effects: MgoTransactionBlockEffects,
        store: &Arc<BridgeOrchestratorTables>,
        action: &BridgeAction,
    ) {
        let status = effects.status();
        match status {
            MgoExecutionStatus::Success => {
                info!(?tx_digest, "Mgo transaction executed successfully");
                store
                    .remove_pending_actions(&[action.digest()])
                    .unwrap_or_else(|e| {
                        panic!("Write to DB should not fail: {:?}", e);
                    })
            }
            MgoExecutionStatus::Failure { error } => {
                // In practice the transaction could fail because of running out of gas, but really
                // should not be due to other reasons.
                // This means manual intervention is needed. So we do not push them back to
                // the execution queue because retries are mostly likely going to fail anyway.
                // After human examination, the node should be restarted and fetch them from WAL.

                // TODO metrics + alerts
                error!(?tx_digest, "Manual intervention is needed. Mgo transaction executed and failed with error: {error:?}");
            }
        }
    }

    /// Panics if the gas object is not owned by the address.
    async fn get_gas_data_assert_ownership(
        mgo_address: MgoAddress,
        gas_object_id: ObjectID,
        mgo_client: &MgoClient<C>,
    ) -> (GasCoin, ObjectRef) {
        let (gas_coin, gas_obj_ref, owner) = mgo_client
            .get_gas_data_panic_if_not_gas(gas_object_id)
            .await;

        // TODO: when we add multiple gas support in the future we could discard
        // transferred gas object instead.
        assert_eq!(
            owner,
            Owner::AddressOwner(mgo_address),
            "Gas object {:?} is no longer owned by address {}",
            gas_object_id,
            mgo_address
        );
        (gas_coin, gas_obj_ref)
    }
}

pub async fn submit_to_executor(
    tx: &mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
    action: BridgeAction,
) -> Result<(), BridgeError> {
    tx.send(BridgeActionExecutionWrapper(action, 0))
        .await
        .map_err(|e| BridgeError::Generic(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use fastcrypto::traits::KeyPair;
    use prometheus::Registry;
    use mgo_json_rpc_types::MgoTransactionBlockResponse;
    use mgo_types::crypto::get_key_pair;
    use mgo_types::gas_coin::GasCoin;
    use mgo_types::{base_types::random_object_ref, transaction::TransactionData};

    use crate::{
        crypto::{
            BridgeAuthorityKeyPair, BridgeAuthorityPublicKeyBytes,
            BridgeAuthorityRecoverableSignature,
        },
        server::mock_handler::BridgeRequestMockHandler,
        mgo_mock_client::MgoMockClient,
        test_utils::{
            get_test_authorities_and_run_mock_bridge_server, get_test_mgo_to_eth_bridge_action,
            sign_action_with_key,
        },
        types::{BridgeCommittee, BridgeCommitteeValiditySignInfo, CertifiedBridgeAction},
    };

    use super::*;

    #[tokio::test]
    async fn test_onchain_execution_loop() {
        let (
            signing_tx,
            _execution_tx,
            mgo_client_mock,
            mut tx_subscription,
            store,
            secrets,
            dummy_mgo_key,
            mock0,
            mock1,
            mock2,
            mock3,
            _handles,
            gas_object_ref,
            mgo_address,
        ) = setup();

        // TODO: remove once we don't rely on env var to get object id
        std::env::set_var("ROOT_BRIDGE_OBJECT_ID", "0x09");
        std::env::set_var("ROOT_BRIDGE_OBJECT_INITIAL_SHARED_VERSION", "1");

        let (action_certificate, _, _) = get_bridge_authority_approved_action(
            vec![&mock0, &mock1, &mock2, &mock3],
            vec![&secrets[0], &secrets[1], &secrets[2], &secrets[3]],
        );
        let action = action_certificate.data().clone();

        let tx_data = build_transaction(mgo_address, &gas_object_ref, action_certificate).unwrap();

        let tx_digest = get_tx_digest(tx_data, &dummy_mgo_key);

        let gas_coin = GasCoin::new_for_testing(1_000_000_000_000); // dummy gas coin
        mgo_client_mock.add_gas_object_info(
            gas_coin.clone(),
            gas_object_ref,
            Owner::AddressOwner(mgo_address),
        );

        // Mock the transaction to be successfully executed
        mock_transaction_response(
            &mgo_client_mock,
            tx_digest,
            MgoExecutionStatus::Success,
            true,
        );

        store.insert_pending_actions(&[action.clone()]).unwrap();
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Kick it
        submit_to_executor(&signing_tx, action.clone())
            .await
            .unwrap();

        // Expect to see the transaction to be requested and successfully executed hence removed from WAL
        tx_subscription.recv().await.unwrap();
        assert!(store.get_all_pending_actions().unwrap().is_empty());

        /////////////////////////////////////////////////////////////////////////////////////////////////
        ////////////////////////////////////// Test execution failure ///////////////////////////////////
        /////////////////////////////////////////////////////////////////////////////////////////////////

        let (action_certificate, _, _) = get_bridge_authority_approved_action(
            vec![&mock0, &mock1, &mock2, &mock3],
            vec![&secrets[0], &secrets[1], &secrets[2], &secrets[3]],
        );

        let action = action_certificate.data().clone();

        let tx_data = build_transaction(mgo_address, &gas_object_ref, action_certificate).unwrap();
        let tx_digest = get_tx_digest(tx_data, &dummy_mgo_key);

        // Mock the transaction to fail
        mock_transaction_response(
            &mgo_client_mock,
            tx_digest,
            MgoExecutionStatus::Failure {
                error: "failure is mother of success".to_string(),
            },
            true,
        );

        store.insert_pending_actions(&[action.clone()]).unwrap();
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Kick it
        submit_to_executor(&signing_tx, action.clone())
            .await
            .unwrap();

        // Expect to see the transaction to be requested and but failed
        tx_subscription.recv().await.unwrap();
        // The action is not removed from WAL because the transaction failed
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        /////////////////////////////////////////////////////////////////////////////////////////////////
        //////////////////////////// Test transaction failed at signing stage ///////////////////////////
        /////////////////////////////////////////////////////////////////////////////////////////////////

        let (action_certificate, _, _) = get_bridge_authority_approved_action(
            vec![&mock0, &mock1, &mock2, &mock3],
            vec![&secrets[0], &secrets[1], &secrets[2], &secrets[3]],
        );

        let action = action_certificate.data().clone();

        let tx_data = build_transaction(mgo_address, &gas_object_ref, action_certificate).unwrap();
        let tx_digest = get_tx_digest(tx_data, &dummy_mgo_key);
        mock_transaction_error(
            &mgo_client_mock,
            tx_digest,
            BridgeError::Generic("some random error".to_string()),
            true,
        );

        store.insert_pending_actions(&[action.clone()]).unwrap();
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Kick it
        submit_to_executor(&signing_tx, action.clone())
            .await
            .unwrap();

        // Failure will trigger retry, we wait for 2 requests before checking WAL log
        let tx_digest = tx_subscription.recv().await.unwrap();
        assert_eq!(tx_subscription.recv().await.unwrap(), tx_digest);

        // The retry is still going on, action still in WAL
        assert!(store
            .get_all_pending_actions()
            .unwrap()
            .contains_key(&action.digest()));

        // Now let it succeed
        mock_transaction_response(
            &mgo_client_mock,
            tx_digest,
            MgoExecutionStatus::Success,
            true,
        );

        // Give it 1 second to retry and succeed
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        // The action is successful and should be removed from WAL now
        assert!(!store
            .get_all_pending_actions()
            .unwrap()
            .contains_key(&action.digest()));
    }

    #[tokio::test]
    async fn test_signature_aggregation_loop() {
        let (
            signing_tx,
            _execution_tx,
            mgo_client_mock,
            mut tx_subscription,
            store,
            secrets,
            dummy_mgo_key,
            mock0,
            mock1,
            mock2,
            mock3,
            _handles,
            gas_object_ref,
            mgo_address,
        ) = setup();

        // TODO: remove once we don't rely on env var to get object id
        std::env::set_var("ROOT_BRIDGE_OBJECT_ID", "0x09");
        std::env::set_var("ROOT_BRIDGE_OBJECT_INITIAL_SHARED_VERSION", "1");

        let (action_certificate, mgo_tx_digest, mgo_tx_event_index) =
            get_bridge_authority_approved_action(
                vec![&mock0, &mock1, &mock2, &mock3],
                vec![&secrets[0], &secrets[1], &secrets[2], &secrets[3]],
            );
        let action = action_certificate.data().clone();
        mock_bridge_authority_signing_errors(
            vec![&mock0, &mock1, &mock2],
            mgo_tx_digest,
            mgo_tx_event_index,
        );
        let mut sigs = mock_bridge_authority_sigs(
            vec![&mock3],
            &action,
            vec![&secrets[3]],
            mgo_tx_digest,
            mgo_tx_event_index,
        );

        let gas_coin = GasCoin::new_for_testing(1_000_000_000_000); // dummy gas coin
        mgo_client_mock.add_gas_object_info(
            gas_coin,
            gas_object_ref,
            Owner::AddressOwner(mgo_address),
        );

        store.insert_pending_actions(&[action.clone()]).unwrap();
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Kick it
        submit_to_executor(&signing_tx, action.clone())
            .await
            .unwrap();

        // Wait until the transaction is retried at least once (instead of deing dropped)
        loop {
            let requested_times =
                mock0.get_mgo_token_events_requested(mgo_tx_digest, mgo_tx_event_index);
            if requested_times >= 2 {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        // Nothing is sent to execute yet
        assert_eq!(
            tx_subscription.try_recv().unwrap_err(),
            tokio::sync::broadcast::error::TryRecvError::Empty
        );
        // Still in WAL
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Let authorities to sign the action too. Now we are above the threshold
        let sig_from_2 = mock_bridge_authority_sigs(
            vec![&mock2],
            &action,
            vec![&secrets[2]],
            mgo_tx_digest,
            mgo_tx_event_index,
        );
        sigs.extend(sig_from_2);
        let certified_action = CertifiedBridgeAction::new_from_data_and_sig(
            action.clone(),
            BridgeCommitteeValiditySignInfo { signatures: sigs },
        );
        let action_certificate = VerifiedCertifiedBridgeAction::new_from_verified(certified_action);

        let tx_data = build_transaction(mgo_address, &gas_object_ref, action_certificate).unwrap();
        let tx_digest = get_tx_digest(tx_data, &dummy_mgo_key);

        mock_transaction_response(
            &mgo_client_mock,
            tx_digest,
            MgoExecutionStatus::Success,
            true,
        );

        // Expect to see the transaction to be requested and succeed
        assert_eq!(tx_subscription.recv().await.unwrap(), tx_digest);
        // The action is removed from WAL
        assert!(!store
            .get_all_pending_actions()
            .unwrap()
            .contains_key(&action.digest()));
    }

    #[tokio::test]
    async fn test_skip_request_signature_if_already_processed_on_chain() {
        let (
            signing_tx,
            _execution_tx,
            mgo_client_mock,
            mut tx_subscription,
            store,
            _secrets,
            _dummy_mgo_key,
            mock0,
            mock1,
            mock2,
            mock3,
            _handles,
            _gas_object_ref,
            _mgo_address,
        ) = setup();

        // TODO: remove once we don't rely on env var to get object id
        std::env::set_var("ROOT_BRIDGE_OBJECT_ID", "0x09");
        std::env::set_var("ROOT_BRIDGE_OBJECT_INITIAL_SHARED_VERSION", "1");

        let mgo_tx_digest = TransactionDigest::random();
        let mgo_tx_event_index = 0;
        let action = get_test_mgo_to_eth_bridge_action(
            Some(mgo_tx_digest),
            Some(mgo_tx_event_index),
            None,
            None,
        );
        mock_bridge_authority_signing_errors(
            vec![&mock0, &mock1, &mock2, &mock3],
            mgo_tx_digest,
            mgo_tx_event_index,
        );
        store.insert_pending_actions(&[action.clone()]).unwrap();
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Kick it
        submit_to_executor(&signing_tx, action.clone())
            .await
            .unwrap();
        let action_digest = action.digest();

        // Wait for 1 second. It should still in the process of retrying requesting sigs becaues we mock errors above.
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        tx_subscription.try_recv().unwrap_err();
        // And the action is still in WAL
        assert!(store
            .get_all_pending_actions()
            .unwrap()
            .contains_key(&action_digest));

        mgo_client_mock.set_action_onchain_status(&action, BridgeActionStatus::Approved);

        // The next retry will see the action is already processed on chain and remove it from WAL
        let now = std::time::Instant::now();
        while store
            .get_all_pending_actions()
            .unwrap()
            .contains_key(&action_digest)
        {
            if now.elapsed().as_secs() > 10 {
                panic!("Timeout waiting for action to be removed from WAL");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        tx_subscription.try_recv().unwrap_err();
    }

    #[tokio::test]
    async fn test_skip_tx_submission_if_already_processed_on_chain() {
        let (
            _signing_tx,
            execution_tx,
            mgo_client_mock,
            mut tx_subscription,
            store,
            secrets,
            dummy_mgo_key,
            mock0,
            mock1,
            mock2,
            mock3,
            _handles,
            gas_object_ref,
            mgo_address,
        ) = setup();

        // TODO: remove once we don't rely on env var to get object id
        std::env::set_var("ROOT_BRIDGE_OBJECT_ID", "0x09");
        std::env::set_var("ROOT_BRIDGE_OBJECT_INITIAL_SHARED_VERSION", "1");

        let (action_certificate, _, _) = get_bridge_authority_approved_action(
            vec![&mock0, &mock1, &mock2, &mock3],
            vec![&secrets[0], &secrets[1], &secrets[2], &secrets[3]],
        );

        let action = action_certificate.data().clone();

        let tx_data =
            build_transaction(mgo_address, &gas_object_ref, action_certificate.clone()).unwrap();
        let tx_digest = get_tx_digest(tx_data, &dummy_mgo_key);
        mock_transaction_error(
            &mgo_client_mock,
            tx_digest,
            BridgeError::Generic("some random error".to_string()),
            true,
        );

        let gas_coin = GasCoin::new_for_testing(1_000_000_000_000); // dummy gas coin
        mgo_client_mock.add_gas_object_info(
            gas_coin.clone(),
            gas_object_ref,
            Owner::AddressOwner(mgo_address),
        );

        mgo_client_mock.set_action_onchain_status(&action, BridgeActionStatus::Pending);

        store.insert_pending_actions(&[action.clone()]).unwrap();
        assert_eq!(
            store.get_all_pending_actions().unwrap()[&action.digest()],
            action.clone()
        );

        // Kick it (send to the execution queue, skipping the signing queue)
        execution_tx
            .send(CertifiedBridgeActionExecutionWrapper(action_certificate, 0))
            .await
            .unwrap();

        // Some requests come in and will fail.
        tx_subscription.recv().await.unwrap();

        // Set the action to be already approved on chain
        mgo_client_mock.set_action_onchain_status(&action, BridgeActionStatus::Approved);

        // The next retry will see the action is already processed on chain and remove it from WAL
        let now = std::time::Instant::now();
        let action_digest = action.digest();
        while store
            .get_all_pending_actions()
            .unwrap()
            .contains_key(&action_digest)
        {
            if now.elapsed().as_secs() > 10 {
                panic!("Timeout waiting for action to be removed from WAL");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    fn mock_bridge_authority_sigs(
        mocks: Vec<&BridgeRequestMockHandler>,
        action: &BridgeAction,
        secrets: Vec<&BridgeAuthorityKeyPair>,
        mgo_tx_digest: TransactionDigest,
        mgo_tx_event_index: u16,
    ) -> BTreeMap<BridgeAuthorityPublicKeyBytes, BridgeAuthorityRecoverableSignature> {
        assert_eq!(mocks.len(), secrets.len());
        let mut signed_actions = BTreeMap::new();
        for (mock, secret) in mocks.iter().zip(secrets.iter()) {
            let signed_action = sign_action_with_key(action, secret);
            mock.add_mgo_event_response(
                mgo_tx_digest,
                mgo_tx_event_index,
                Ok(signed_action.clone()),
            );
            signed_actions.insert(secret.public().into(), signed_action.into_sig().signature);
        }
        signed_actions
    }

    fn mock_bridge_authority_signing_errors(
        mocks: Vec<&BridgeRequestMockHandler>,
        mgo_tx_digest: TransactionDigest,
        mgo_tx_event_index: u16,
    ) {
        for mock in mocks {
            mock.add_mgo_event_response(
                mgo_tx_digest,
                mgo_tx_event_index,
                Err(BridgeError::RestAPIError("small issue".into())),
            );
        }
    }

    /// Create a BridgeAction and mock authorities to return signatures
    fn get_bridge_authority_approved_action(
        mocks: Vec<&BridgeRequestMockHandler>,
        secrets: Vec<&BridgeAuthorityKeyPair>,
    ) -> (VerifiedCertifiedBridgeAction, TransactionDigest, u16) {
        let mgo_tx_digest = TransactionDigest::random();
        let mgo_tx_event_index = 1;
        let action = get_test_mgo_to_eth_bridge_action(
            Some(mgo_tx_digest),
            Some(mgo_tx_event_index),
            None,
            None,
        );

        let sigs =
            mock_bridge_authority_sigs(mocks, &action, secrets, mgo_tx_digest, mgo_tx_event_index);
        let certified_action = CertifiedBridgeAction::new_from_data_and_sig(
            action,
            BridgeCommitteeValiditySignInfo { signatures: sigs },
        );
        (
            VerifiedCertifiedBridgeAction::new_from_verified(certified_action),
            mgo_tx_digest,
            mgo_tx_event_index,
        )
    }

    fn get_tx_digest(tx_data: TransactionData, dummy_mgo_key: &MgoKeyPair) -> TransactionDigest {
        let sig = Signature::new_secure(
            &IntentMessage::new(Intent::mgo_transaction(), &tx_data),
            dummy_mgo_key,
        );
        let signed_tx = Transaction::from_data(tx_data, vec![sig]);
        *signed_tx.digest()
    }

    /// Why is `wildcard` needed? This is because authority signatures
    /// are part of transaction data. Depending on whose signatures
    /// are included in what order, this may change the tx digest.
    fn mock_transaction_response(
        mgo_client_mock: &MgoMockClient,
        tx_digest: TransactionDigest,
        status: MgoExecutionStatus,
        wildcard: bool,
    ) {
        let mut response = MgoTransactionBlockResponse::new(tx_digest);
        let effects = MgoTransactionBlockEffects::new_for_testing(tx_digest, status);
        response.effects = Some(effects);
        if wildcard {
            mgo_client_mock.set_wildcard_transaction_response(Ok(response));
        } else {
            mgo_client_mock.add_transaction_response(tx_digest, Ok(response));
        }
    }

    fn mock_transaction_error(
        mgo_client_mock: &MgoMockClient,
        tx_digest: TransactionDigest,
        error: BridgeError,
        wildcard: bool,
    ) {
        if wildcard {
            mgo_client_mock.set_wildcard_transaction_response(Err(error));
        } else {
            mgo_client_mock.add_transaction_response(tx_digest, Err(error));
        }
    }

    #[allow(clippy::type_complexity)]
    fn setup() -> (
        mango_metrics::metered_channel::Sender<BridgeActionExecutionWrapper>,
        mango_metrics::metered_channel::Sender<CertifiedBridgeActionExecutionWrapper>,
        MgoMockClient,
        tokio::sync::broadcast::Receiver<TransactionDigest>,
        Arc<BridgeOrchestratorTables>,
        Vec<BridgeAuthorityKeyPair>,
        MgoKeyPair,
        BridgeRequestMockHandler,
        BridgeRequestMockHandler,
        BridgeRequestMockHandler,
        BridgeRequestMockHandler,
        Vec<tokio::task::JoinHandle<()>>,
        ObjectRef,
        MgoAddress,
    ) {
        telemetry_subscribers::init_for_testing();
        let registry = Registry::new();
        mango_metrics::init_metrics(&registry);

        let (mgo_address, kp): (_, fastcrypto::secp256k1::Secp256k1KeyPair) = get_key_pair();
        let mgo_key = MgoKeyPair::from(kp);
        let gas_object_ref = random_object_ref();
        let temp_dir = tempfile::tempdir().unwrap();
        let store = BridgeOrchestratorTables::new(temp_dir.path());
        let mgo_client_mock = MgoMockClient::default();
        let tx_subscription = mgo_client_mock.subscribe_to_requested_transactions();
        let mgo_client = Arc::new(MgoClient::new_for_testing(mgo_client_mock.clone()));

        // The dummy key is used to sign transaction so we can get TransactionDigest easily.
        // User signature is not part of the transaction so it does not matter which key it is.
        let (_, dummy_kp): (_, fastcrypto::secp256k1::Secp256k1KeyPair) = get_key_pair();
        let dummy_mgo_key = MgoKeyPair::from(dummy_kp);

        let mock0 = BridgeRequestMockHandler::new();
        let mock1 = BridgeRequestMockHandler::new();
        let mock2 = BridgeRequestMockHandler::new();
        let mock3 = BridgeRequestMockHandler::new();

        let (mut handles, authorities, secrets) = get_test_authorities_and_run_mock_bridge_server(
            vec![2500, 2500, 2500, 2500],
            vec![mock0.clone(), mock1.clone(), mock2.clone(), mock3.clone()],
        );

        let committee = BridgeCommittee::new(authorities).unwrap();

        let agg = Arc::new(BridgeAuthorityAggregator::new(Arc::new(committee)));

        let executor = BridgeActionExecutor::new(
            mgo_client.clone(),
            agg.clone(),
            store.clone(),
            mgo_key,
            mgo_address,
            gas_object_ref.0,
        );

        let (executor_handle, signing_tx, execution_tx) = executor.run_inner();
        handles.extend(executor_handle);
        (
            signing_tx,
            execution_tx,
            mgo_client_mock,
            tx_subscription,
            store,
            secrets,
            dummy_mgo_key,
            mock0,
            mock1,
            mock2,
            mock3,
            handles,
            gas_object_ref,
            mgo_address,
        )
    }
}
