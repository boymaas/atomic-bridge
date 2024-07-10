use std::time::Duration;

use futures::StreamExt;
use test_log::test;

use bridge_shared::{
	bridge_contracts::{
		BridgeContractCounterparty, BridgeContractCounterpartyError, BridgeContractInitiator,
		BridgeContractInitiatorError,
	},
	bridge_monitoring::{BridgeContractCounterpartyEvent, BridgeContractInitiatorEvent},
	bridge_service::{
		active_swap::{ActiveSwapConfig, LockBridgeTransferAssetsError},
		events::{CEvent, CWarn, Event, IEvent, IWarn},
		BridgeServiceConfig,
	},
	types::{
		Amount, BridgeTransferDetails, CompletedDetails, Convert, HashLock, HashLockPreImage,
		InitiatorAddress, RecipientAddress, TimeLock,
	},
};

mod shared;

use crate::shared::{
	setup_bridge_service, testing::blockchain::client::MethodName, B2Client, BC1Address, BC1Hash,
	BC2Address, BC2Hash, SetupBridgeServiceResult,
};

use self::shared::testing::blockchain::client::{CallConfig, ErrorConfig};

#[test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn test_bridge_service_error_handling() {
	let SetupBridgeServiceResult(
		mut bridge_service,
		mut blockchain_1_client,
		mut blockchain_2_client,
		blockchain_1,
		blockchain_2,
	) = setup_bridge_service(BridgeServiceConfig {
		active_swap: ActiveSwapConfig {
			error_attempts: 3,
			error_delay: Duration::from_secs(1),
			contract_call_timeout: Duration::from_secs(5),
		},
	});

	tokio::spawn(blockchain_1);
	tokio::spawn(blockchain_2);

	// Lets make the blockchain_2_client fail on the locking of assets
	blockchain_2_client.set_call_config(
		MethodName::LockBridgeTransferAssets,
		1,
		CallConfig {
			error: ErrorConfig::CounterpartyError(
				BridgeContractCounterpartyError::LockTransferAssetsError,
			),
			delay: None,
		},
	);

	// Step 1: Initiating the swap on Blockchain 1 with an invalid hash lock

	tracing::debug!("Initiating bridge transfer with invalid hash lock");
	blockchain_1_client
		.initiate_bridge_transfer(
			InitiatorAddress(BC1Address("initiator")),
			RecipientAddress::from(BC1Address("recipient")),
			HashLock(BC1Hash::from("hash_lock")),
			TimeLock(100),
			Amount(1000),
		)
		.await
		.expect("initiate_bridge_transfer failed");

	// B1I Initiated
	let transfer_initiated_event = bridge_service.next().await.expect("No event");
	let transfer_initiated_event =
		transfer_initiated_event.B1I_ContractEvent().expect("Not a B1I event");
	tracing::debug!(?transfer_initiated_event);
	assert_eq!(
		transfer_initiated_event,
		&BridgeContractInitiatorEvent::Initiated(BridgeTransferDetails {
			bridge_transfer_id: transfer_initiated_event.bridge_transfer_id().clone(),
			initiator_address: InitiatorAddress(BC1Address("initiator")),
			recipient_address: RecipientAddress::from(BC1Address("recipient")),
			hash_lock: HashLock(BC1Hash::from("hash_lock")),
			time_lock: TimeLock(100),
			amount: Amount(1000)
		})
	);

	// B2C Locking call failed due to mock above
	let event = bridge_service.next().await.expect("No event");
	tracing::debug!(?event);
	assert!(matches!(
		event.B2C().and_then(CEvent::warn).expect("not a b2c warn event"),
		CWarn::BridgeAssetsLockingError(_)
	));

	// dbg!(&bridge_service.active_swaps_b1_to_b2);

	// The Bridge is expected to retry the operation after the configured delay in case of an error.
	let event = bridge_service.next().await.expect("No event");
	tracing::debug!(?event);
	assert!(matches!(event, Event::B2C(CEvent::RetryLockingAssets(_))));

	// Post-retry, the client is expected to successfully invoke the contract and return a Locked
	// event.
	let event = bridge_service.next().await.expect("No event");
	let event = event.B2C_ContractEvent().expect("Not a B2C event");
	tracing::debug!(?event);
	assert!(matches!(event, BridgeContractCounterpartyEvent::Locked(_)));

	// Bridge gracefully recovered from an error

	// Step 2: Attempting to complete the swap on Blockchain 2
	tracing::debug!("Attempting to complete bridge transfer with invalid secret");

	<B2Client as BridgeContractCounterparty>::complete_bridge_transfer(
		&mut blockchain_2_client,
		Convert::convert(transfer_initiated_event.bridge_transfer_id()),
		HashLockPreImage(b"hash_lock".to_vec()),
	)
	.await
	.expect("complete_bridge_transfer failed");

	// Expecting the bridge to detect the initiator's completion of the swap and reveal the secret
	let completed_event_counterparty = bridge_service.next().await.expect("No event");
	let completed_event_counterparty =
		completed_event_counterparty.B2C_ContractEvent().expect("Not a B2C event");
	tracing::debug!(?completed_event_counterparty);
	assert_eq!(
		completed_event_counterparty,
		&BridgeContractCounterpartyEvent::Completed(CompletedDetails {
			bridge_transfer_id: Convert::convert(transfer_initiated_event.bridge_transfer_id()),
			recipient_address: RecipientAddress::from(BC2Address("recipient")),
			hash_lock: HashLock(BC2Hash::from("hash_lock")),
			secret: HashLockPreImage(b"hash_lock".to_vec()),
			amount: Amount(1000),
		})
	);

	// Subsequently, the bridge service has successfully detected the secret and is now attempting to finalize
	// the swap on blockchain 1. It is imperative for this process to succeed; otherwise, funds may be irretrievably
	// lost, necessitating manual intervention to resolve the issue.

	// Intentionally causing the blockchain_1_client to fail during the asset locking process to
	// simulate various failure scenarios.
	blockchain_1_client.set_call_config(
		MethodName::CompleteBridgeTransferInitiator,
		1,
		CallConfig {
			error: ErrorConfig::InitiatorError(BridgeContractInitiatorError::CompleteTransferError),
			delay: None,
		},
	);

	// B1C Locking call failed due to mock above
	let event = bridge_service.next().await.expect("No event");
	tracing::debug!(?event);
	assert!(matches!(
		event.B1I().and_then(IEvent::warn).expect("not a b2c warn event"),
		IWarn::CompleteTransferError(_)
	));

	// The Bridge is expected to retry the operation after the configured delay in case of an error.
	let event = bridge_service.next().await.expect("No event");
	tracing::debug!(?event);
	assert!(matches!(event, Event::B1I(IEvent::RetryCompletingTransfer(_))));

	// Bridge service completes the swap using the secret to claim the funds on Blockchain 1
	// Since the mock passes.

	tracing::debug!("Bridge service completing bridge transfer on Blockchain 1");

	let completed_event_initiator = bridge_service.next().await.expect("No event");
	let completed_event_initiator =
		completed_event_initiator.B1I_ContractEvent().expect("Not a B1I event");
	tracing::debug!(?completed_event_initiator);
	assert_eq!(
		completed_event_initiator,
		&BridgeContractInitiatorEvent::Completed(
			transfer_initiated_event.bridge_transfer_id().clone()
		)
	);
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn test_bridge_service_timeout_error_handling() {
	let SetupBridgeServiceResult(
		mut bridge_service,
		mut blockchain_1_client,
		mut blockchain_2_client,
		blockchain_1,
		blockchain_2,
	) = setup_bridge_service(BridgeServiceConfig {
		active_swap: ActiveSwapConfig {
			error_attempts: 1,
			error_delay: Duration::from_secs(1),
			contract_call_timeout: Duration::from_millis(100), // Set a short timeout for testing
		},
	});

	tokio::spawn(blockchain_1);
	tokio::spawn(blockchain_2);

	// Lets make the blockchain_2_client fail on the locking of assets
	blockchain_2_client.set_call_config(
		MethodName::LockBridgeTransferAssets,
		1,
		// Longer delay than the timeout, to trigger timeout
		CallConfig { error: ErrorConfig::None, delay: Some(Duration::from_secs(1)) },
	);

	// Step 1: Initiating the swap on Blockchain 1
	tracing::debug!("Initiating bridge transfer with short timeout");
	blockchain_1_client
		.initiate_bridge_transfer(
			InitiatorAddress(BC1Address("initiator")),
			RecipientAddress::from(BC1Address("recipient")),
			HashLock(BC1Hash::from("hash_lock")),
			TimeLock(100),
			Amount(1000),
		)
		.await
		.expect("initiate_bridge_transfer failed");

	// B1I Initiated
	let transfer_initiated_event = bridge_service.next().await.expect("No event");
	let transfer_initiated_event =
		transfer_initiated_event.B1I_ContractEvent().expect("Not a B1I event");
	tracing::debug!(?transfer_initiated_event);
	assert_eq!(
		transfer_initiated_event,
		&BridgeContractInitiatorEvent::Initiated(BridgeTransferDetails {
			bridge_transfer_id: transfer_initiated_event.bridge_transfer_id().clone(),
			initiator_address: InitiatorAddress(BC1Address("initiator")),
			recipient_address: RecipientAddress::from(BC1Address("recipient")),
			hash_lock: HashLock(BC1Hash::from("hash_lock")),
			time_lock: TimeLock(100),
			amount: Amount(1000)
		})
	);

	// B2C Locking call should timeout
	let event = bridge_service.next().await.expect("No event");
	tracing::debug!(?event);
	assert!(matches!(
		event.B2C().and_then(CEvent::warn).expect("not a b2c warn event"),
		CWarn::BridgeAssetsLockingError(LockBridgeTransferAssetsError::ContractCallTimeoutError)
	));

	// The Bridge is expected to retry the operation after the configured delay in case of an error.
	let event = bridge_service.next().await.expect("No event");
	tracing::debug!(?event);
	assert!(matches!(event, Event::B2C(CEvent::RetryLockingAssets(_))));

	// Post-retry, the client is expected to successfully invoke the contract and return a Locked
	// event.
	let event = bridge_service.next().await.expect("No event");
	let event = event.B2C_ContractEvent().expect("Not a B2C event");
	tracing::debug!(?event);
	assert!(matches!(event, BridgeContractCounterpartyEvent::Locked(_)));
}
