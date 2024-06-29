use futures::StreamExt;
use rand::SeedableRng;
use test_log::test;

use bridge_shared::{
	blockchain_service::AbstractBlockchainService,
	bridge_contracts::{BridgeContractCounterparty, BridgeContractInitiator},
	bridge_monitoring::{BridgeContractCounterpartyEvent, BridgeContractInitiatorEvent},
	bridge_service::BridgeService,
	types::{
		Amount, BridgeTransferDetails, CompletedDetails, Convert, HashLock, HashLockPreImage,
		InitiatorAddress, LockDetails, RecipientAddress, TimeLock,
	},
};

use crate::shared::{
	B1Client, B2Client, BC1Address, BC1Hash, BC2Address, BC2Hash, CounterpartyContractMonitoring,
	InitiatorContractMonitoring,
};

mod shared;

use shared::testing::{
	blockchain::{AbstractBlockchain, AbstractBlockchainClient},
	rng::{RngSeededClone, TestRng},
};

use self::shared::{B1Service, B2Service};

async fn setup_bridge_service() -> (
	BridgeService<B1Service, B2Service>,
	B1Client,
	B2Client,
	AbstractBlockchain<BC1Address, BC1Hash, TestRng>,
	AbstractBlockchain<BC2Address, BC2Hash, TestRng>,
) {
	let mut rng = TestRng::from_seed([0u8; 32]);

	let mut blockchain_1 =
		AbstractBlockchain::<BC1Address, BC1Hash, _>::new(rng.seeded_clone(), "Blockchain1");
	let mut blockchain_2 =
		AbstractBlockchain::<BC2Address, BC2Hash, _>::new(rng.seeded_clone(), "Blockchain2");

	// Contracts and monitors for blockchain 1
	let client_1 =
		AbstractBlockchainClient::new(blockchain_1.connection(), rng.seeded_clone(), 0.0, 0.00);
	let monitor_1_initiator = InitiatorContractMonitoring::build(blockchain_1.add_event_listener());
	let monitor_1_counterparty =
		CounterpartyContractMonitoring::build(blockchain_1.add_event_listener());

	// Contracts and monitors for blockchain 2
	let client_2 =
		AbstractBlockchainClient::new(blockchain_2.connection(), rng.seeded_clone(), 0.0, 0.00);
	let monitor_2_initiator = InitiatorContractMonitoring::build(blockchain_2.add_event_listener());
	let monitor_2_counterparty =
		CounterpartyContractMonitoring::build(blockchain_2.add_event_listener());

	let blockchain_1_client = B1Client::build(client_1.clone());
	let blockchain_1_service = AbstractBlockchainService {
		initiator_contract: blockchain_1_client.clone(),
		initiator_monitoring: monitor_1_initiator,
		counterparty_contract: blockchain_1_client.clone(),
		counterparty_monitoring: monitor_1_counterparty,
		_phantom: Default::default(),
	};

	let blockchain_2_client = B2Client::build(client_2.clone());
	let blockchain_2_service = AbstractBlockchainService {
		initiator_contract: blockchain_2_client.clone(),
		initiator_monitoring: monitor_2_initiator,
		counterparty_contract: blockchain_2_client.clone(),
		counterparty_monitoring: monitor_2_counterparty,
		_phantom: Default::default(),
	};

	let bridge_service = BridgeService::new(blockchain_1_service, blockchain_2_service);

	(bridge_service, blockchain_1_client, blockchain_2_client, blockchain_1, blockchain_2)
}

#[test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
async fn test_bridge_service_integration_a_to_b() {
	let (
		mut bridge_service,
		mut blockchain_1_client,
		mut blockchain_2_client,
		blockchain_1,
		blockchain_2,
	) = setup_bridge_service().await;

	tokio::spawn(blockchain_1);
	tokio::spawn(blockchain_2);

	// Step 1: Initiating the swap on Blockchain 1

	// The initiator of the swap triggers a bridge transfer, simultaneously time-locking the assets
	// in the smart contract.
	blockchain_1_client
		.initiate_bridge_transfer(
			InitiatorAddress(BC1Address("initiator")),
			RecipientAddress(BC1Address("recipient")),
			HashLock(BC1Hash::from("hash_lock")),
			TimeLock(100),
			Amount(1000),
		)
		.await
		.expect("initiate_bridge_transfer failed");

	// We expect the bridge to recognize the contract event and emit the appropriate message
	let transfer_initiated_event = bridge_service.next().await.expect("No event");
	let transfer_initiated_event =
		transfer_initiated_event.B1I_ContractEvent().expect("Not a B1I event");
	tracing::debug!(?transfer_initiated_event);
	assert_eq!(
		transfer_initiated_event,
		&BridgeContractInitiatorEvent::Initiated(BridgeTransferDetails {
			bridge_transfer_id: transfer_initiated_event.bridge_transfer_id().clone(),
			initiator_address: InitiatorAddress(BC1Address("initiator")),
			recipient_address: RecipientAddress(BC1Address("recipient")),
			hash_lock: HashLock(BC1Hash::from("hash_lock")),
			time_lock: TimeLock(100),
			amount: Amount(1000)
		})
	);

	// Step 2: Locking the assets on the Blockchain 2

	// Upon recognizing the event, our bridge server has invoked the counterparty
	// contract on blockchain 2 to initiate asset locking within the smart contract.
	let counterparty_locked_event = bridge_service.next().await.expect("No event");
	let counterparty_locked_event =
		counterparty_locked_event.B2C_ContractEvent().expect("Not a B2C event");
	tracing::debug!(?counterparty_locked_event);
	assert_eq!(
		counterparty_locked_event,
		&BridgeContractCounterpartyEvent::Locked(LockDetails {
			bridge_transfer_id: Convert::convert(transfer_initiated_event.bridge_transfer_id()),
			hash_lock: HashLock(BC2Hash::from("hash_lock")),
			time_lock: TimeLock(100),
			recipient_address: RecipientAddress(BC2Address("recipient")),
			amount: Amount(1000)
		})
	);

	// Step 3: Client completes the swap on Blockchain 2, revealing the pre_image of the hash lock

	// Once the assets are secured within the counterparty smart contract, the initiator is able
	// to execute the complete bridge transfer by disclosing the secret key required to unlock the assets.
	<B2Client as BridgeContractCounterparty>::complete_bridge_transfer(
		&mut blockchain_2_client,
		Convert::convert(transfer_initiated_event.bridge_transfer_id()),
		HashLockPreImage(b"hash_lock".to_vec()),
	)
	.await
	.expect("complete_bridge_transfer failed");

	// As the claim was made by the counterparty, we anticipate the bridge to generate a bridge
	// contract counterpart event.
	let completed_event_counterparty = bridge_service.next().await.expect("No event");
	let completed_event_counterparty =
		completed_event_counterparty.B2C_ContractEvent().expect("Not a B2C event");
	tracing::debug!(?completed_event_counterparty);
	assert_eq!(
		completed_event_counterparty,
		&BridgeContractCounterpartyEvent::Completed(CompletedDetails {
			bridge_transfer_id: Convert::convert(transfer_initiated_event.bridge_transfer_id()),
			recipient_address: RecipientAddress(BC2Address("recipient")),
			hash_lock: HashLock(BC2Hash::from("hash_lock")),
			secret: HashLockPreImage(b"hash_lock".to_vec()),
			amount: Amount(1000)
		})
	);

	// Step 4: Bridge service completes the swap, using the secret to claim the funds on Blockchain 1

	// As the initiator has successfully claimed the funds on the Counterparty blockchain, the bridge
	// is now expected to finalize the swap by completing the necessary tasks on the initiator
	// blockchain.
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
