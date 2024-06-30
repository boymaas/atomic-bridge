use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{trace, warn};

use crate::{
	blockchain_service::{BlockchainService, ContractEvent},
	bridge_contracts::{BridgeContractCounterparty, BridgeContractInitiator},
	bridge_monitoring::{BridgeContractCounterpartyEvent, BridgeContractInitiatorEvent},
	bridge_service::{
		active_swap::ActiveSwapEvent,
		events::{CEvent, CWarn, IEvent, IWarn},
	},
	types::Convert,
};

pub mod active_swap;
pub mod events;

use self::{active_swap::ActiveSwapMap, events::Event};

pub struct BridgeService<B1, B2>
where
	B1: BlockchainService,
	B2: BlockchainService,
{
	pub blockchain_1: B1,
	pub blockchain_2: B2,

	pub active_swaps_b1_to_b2: ActiveSwapMap<B1, B2>,
	pub active_swaps_b2_to_b1: ActiveSwapMap<B2, B1>,
}

impl<B1, B2> BridgeService<B1, B2>
where
	B1: BlockchainService + 'static,
	B2: BlockchainService + 'static,
{
	pub fn new(blockchain_1: B1, blockchain_2: B2) -> Self {
		Self {
			active_swaps_b1_to_b2: ActiveSwapMap::build(
				blockchain_1.initiator_contract().clone(),
				blockchain_2.counterparty_contract().clone(),
			),
			active_swaps_b2_to_b1: ActiveSwapMap::build(
				blockchain_2.initiator_contract().clone(),
				blockchain_1.counterparty_contract().clone(),
			),
			blockchain_1,
			blockchain_2,
		}
	}
}

fn handle_initiator_event<BFrom, BTo>(
	initiator_event: BridgeContractInitiatorEvent<BFrom::Address, BFrom::Hash>,
	active_swaps: &mut ActiveSwapMap<BFrom, BTo>,
) -> Option<Event<BFrom, BTo>>
where
	BFrom: BlockchainService + 'static,
	BTo: BlockchainService + 'static,
	<<BTo as BlockchainService>::CounterpartyContract as BridgeContractCounterparty>::Address:
		From<<BFrom as BlockchainService>::Address>,
	<<BTo as BlockchainService>::CounterpartyContract as BridgeContractCounterparty>::Hash:
		From<<BFrom as BlockchainService>::Hash>,
{
	match initiator_event {
		BridgeContractInitiatorEvent::Initiated(ref details) => {
			if active_swaps.already_executing(&details.bridge_transfer_id) {
				warn!("BridgeService: Bridge transfer {:?} already present, monitoring should only return event once", details.bridge_transfer_id);
				return Some(Event::B1I(IEvent::Warn(IWarn::AlreadyPresent(details.clone()))));
			}
			active_swaps.start_bridge_transfer(details.clone());
			Some(Event::B1I(IEvent::ContractEvent(initiator_event)))
		}
		BridgeContractInitiatorEvent::Completed(_) => {
			Some(Event::B1I(IEvent::ContractEvent(initiator_event)))
		}
		BridgeContractInitiatorEvent::Refunded(_) => todo!(),
	}
}

fn handle_counterparty_event<BFrom, BTo>(
	event: BridgeContractCounterpartyEvent<BTo::Address, BTo::Hash>,
	active_swaps: &mut ActiveSwapMap<BFrom, BTo>,
) -> Option<Event<BFrom, BTo>>
where
	BFrom: BlockchainService + 'static,
	BTo: BlockchainService + 'static,
	<BFrom as BlockchainService>::Hash: std::convert::From<<BTo as BlockchainService>::Hash>,
	<<BFrom as BlockchainService>::InitiatorContract as BridgeContractInitiator>::Hash:
		std::convert::From<<BTo as BlockchainService>::Hash>,
{
	use BridgeContractCounterpartyEvent::*;
	match event {
		Locked(ref _details) => Some(Event::B2C(CEvent::ContractEvent(event))),
		Completed(ref details) => match active_swaps.complete_bridge_transfer(details.clone()) {
			Ok(_) => {
				trace!("BridgeService: Bridge transfer completed successfully");
				Some(Event::B2C(CEvent::ContractEvent(event)))
			}
			Err(error) => {
				warn!("BridgeService: Error completing bridge transfer: {:?}", error);
				match error {
					active_swap::ActiveSwapMapError::NonExistingSwap => Some(Event::B2C(
						CEvent::Warn(CWarn::CannotCompleteUnexistingSwap(details.clone())),
					)),
				}
			}
		},
	}
}

impl<B1, B2> Stream for BridgeService<B1, B2>
where
	B1: BlockchainService + 'static,
	B2: BlockchainService + 'static,

	<B1::InitiatorContract as BridgeContractInitiator>::Hash: From<B2::Hash>,
	<B1::InitiatorContract as BridgeContractInitiator>::Address: From<B2::Address>,

	<B1::CounterpartyContract as BridgeContractCounterparty>::Hash: From<B2::Hash>,
	<B1::CounterpartyContract as BridgeContractCounterparty>::Address: From<B2::Address>,

	<B2::InitiatorContract as BridgeContractInitiator>::Hash: From<B1::Hash>,
	<B2::InitiatorContract as BridgeContractInitiator>::Address: From<B1::Address>,

	<B2::CounterpartyContract as BridgeContractCounterparty>::Hash: From<B1::Hash>,
	<B2::CounterpartyContract as BridgeContractCounterparty>::Address: From<B1::Address>,

	<B1 as BlockchainService>::Hash: Convert<B2::Hash>,
	<B2 as BlockchainService>::Hash: Convert<B1::Hash>,

	<B1 as BlockchainService>::Hash: From<<B2 as BlockchainService>::Hash>,
	<<B1 as BlockchainService>::InitiatorContract as BridgeContractInitiator>::Hash:
		From<<B2 as BlockchainService>::Hash>,
{
	type Item = Event<B1, B2>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();

		use ActiveSwapEvent::*;

		// Handle active swaps initiated from blockchain 1
		match this.active_swaps_b1_to_b2.poll_next_unpin(cx) {
			Poll::Ready(Some(event)) => {
				trace!("BridgeService: Received event from active swaps B1 -> B2: {:?}", event);
				match event {
					BridgeAssetsLocked(bridge_transfer_id) => {
						trace!(
							"BridgeService: Bridge assets locked for transfer {:?}",
							bridge_transfer_id
						);
					}
					BridgeAssetsLockingError(error) => {
						warn!("BridgeService: Error locking bridge assets: {:?}", error);
					}
					BridgeAssetsCompleted(bridge_transfer_id) => {
						trace!(
							"BridgeService: Bridge assets completed for transfer {:?}",
							bridge_transfer_id
						);
					}
					BridgeAssetsCompletingError(error) => {
						warn!("BridgeService: Error completing bridge assets: {:?}", error);
					}
				}
			}
			Poll::Ready(None) => {
				trace!("BridgeService: Active swaps B1 -> B2 has no more events");
			}
			Poll::Pending => {
				trace!("BridgeService: Active swaps B1 -> B2 has no events at this time");
			}
		}

		// Handle active swaps initiated from blockchain 2
		match this.active_swaps_b2_to_b1.poll_next_unpin(cx) {
			Poll::Ready(Some(event)) => {
				trace!("BridgeService: Received event from active swaps B2 -> B1: {:?}", event);
				match event {
					BridgeAssetsLocked(bridge_transfer_id) => {
						trace!(
							"BridgeService: Bridge assets locked for transfer {:?}",
							bridge_transfer_id
						);
					}
					BridgeAssetsLockingError(error) => {
						warn!("BridgeService: Error locking bridge assets: {:?}", error);
					}
					BridgeAssetsCompleted(_) => todo!(),
					BridgeAssetsCompletingError(_) => todo!(),
				}
			}
			Poll::Ready(None) => {
				trace!("BridgeService: Active swaps B2 -> B1 has no more events");
			}
			Poll::Pending => {
				trace!("BridgeService: Active swaps B2 -> B1 has no events at this time");
			}
		}

		match this.blockchain_1.poll_next_unpin(cx) {
			Poll::Ready(Some(event)) => {
				trace!("BridgeService: Received event from blockchain service 1: {:?}", event);
				match event {
					ContractEvent::InitiatorEvent(initiator_event) => {
						trace!("BridgeService: Initiator event from blockchain service 1");
						if let Some(event) = handle_initiator_event::<B1, B2>(
							initiator_event,
							&mut this.active_swaps_b1_to_b2,
						) {
							return Poll::Ready(Some(event));
						}
					}
					ContractEvent::CounterpartyEvent(_) => {
						trace!("BridgeService: Counterparty event from blockchain service 1");
					}
				}
			}
			Poll::Ready(None) => {
				trace!("BridgeService: Blockchain service 1 has no more events");
			}
			Poll::Pending => {
				trace!("BridgeService: Blockchain service 1 has no events at this time");
			}
		}

		match this.blockchain_2.poll_next_unpin(cx) {
			Poll::Ready(Some(event)) => {
				trace!("BridgeService: Received event from blockchain service 2: {:?}", event);
				match event {
					ContractEvent::InitiatorEvent(_) => {
						trace!("BridgeService: Initiator event from blockchain service 2");
					}
					ContractEvent::CounterpartyEvent(event) => {
						trace!("BridgeService: Counterparty event from blockchain service 2");
						if let Some(event) = handle_counterparty_event::<B1, B2>(
							event,
							&mut this.active_swaps_b1_to_b2,
						) {
							return Poll::Ready(Some(event));
						}
					}
				}
			}
			Poll::Ready(None) => {
				trace!("BridgeService: Blockchain service 2 has no more events");
			}
			Poll::Pending => {
				trace!("BridgeService: Blockchain service 2 has no events at this time");
			}
		}

		Poll::Pending
	}
}
