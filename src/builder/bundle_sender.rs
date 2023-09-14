use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, Context};
use ethers::types::{transaction::eip2718::TypedTransaction, Address, H256, U256};
use tokio::{
    join,
    sync::{broadcast, mpsc, oneshot},
    time,
};
use tonic::transport::Channel;
use tracing::{error, info, trace, warn};

use crate::{
    builder::{
        bundle_proposer::BundleProposer,
        emit::{BuilderEvent, BundleTxDetails},
        transaction_tracker::{SendResult, TrackerUpdate, TransactionTracker},
    },
    common::{
        block_watcher,
        emit::WithEntryPoint,
        gas::GasFees,
        math,
        protos::op_pool::{
            self, op_pool_client::OpPoolClient, RemoveEntitiesRequest, RemoveOpsRequest,
        },
        types::{Entity, EntryPointLike, ExpectedStorage, ProviderLike, UserOperation},
    },
};

// Overhead on gas estimates to account for inaccuracies.
const GAS_ESTIMATE_OVERHEAD_PERCENT: u64 = 10;

#[derive(Debug)]
pub struct Settings {
    pub replacement_fee_percent_increase: u64,
    pub max_fee_increases: u64,
}

#[derive(Debug)]
pub struct BundleSender<P, PL, E, T>
where
    P: BundleProposer,
    PL: ProviderLike,
    E: EntryPointLike,
    T: TransactionTracker,
{
    id: u64,
    manual_bundling_mode: Arc<AtomicBool>,
    send_bundle_receiver: mpsc::Receiver<SendBundleRequest>,
    chain_id: u64,
    beneficiary: Address,
    eth_poll_interval: Duration,
    op_pool: OpPoolClient<Channel>,
    proposer: P,
    entry_point: E,
    transaction_tracker: T,
    // TODO: Figure out what we really want to do for detecting new blocks.
    provider: Arc<PL>,
    settings: Settings,
    event_sender: broadcast::Sender<WithEntryPoint<BuilderEvent>>,
}

#[derive(Debug)]
struct BundleTx {
    tx: TypedTransaction,
    expected_storage: ExpectedStorage,
    op_hashes: Vec<H256>,
}

pub struct SendBundleRequest {
    pub responder: oneshot::Sender<SendBundleResult>,
}

#[derive(Debug)]
pub enum SendBundleResult {
    Success {
        block_number: u64,
        attempt_number: u64,
        tx_hash: H256,
    },
    NoOperationsInitially,
    NoOperationsAfterFeeIncreases {
        initial_op_count: usize,
        attempt_number: u64,
    },
    StalledAtMaxFeeIncreases,
    Error(anyhow::Error),
}

impl<P, PL, E, T> BundleSender<P, PL, E, T>
where
    P: BundleProposer,
    PL: ProviderLike,
    E: EntryPointLike,
    T: TransactionTracker,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        manual_bundling_mode: Arc<AtomicBool>,
        send_bundle_receiver: mpsc::Receiver<SendBundleRequest>,
        chain_id: u64,
        beneficiary: Address,
        eth_poll_interval: Duration,
        op_pool: OpPoolClient<Channel>,
        proposer: P,
        entry_point: E,
        transaction_tracker: T,
        provider: Arc<PL>,
        settings: Settings,
        event_sender: broadcast::Sender<WithEntryPoint<BuilderEvent>>,
    ) -> Self {
        Self {
            id,
            manual_bundling_mode,
            send_bundle_receiver,
            chain_id,
            beneficiary,
            eth_poll_interval,
            op_pool,
            proposer,
            entry_point,
            transaction_tracker,
            provider,
            settings,
            event_sender,
        }
    }

    /// Loops forever, attempting to form and send a bundle on each new block,
    /// then waiting for one bundle to be mined or dropped before forming the
    /// next one.
    pub async fn send_bundles_in_loop(mut self) -> ! {
        let mut last_block_number = 0;
        loop {
            let mut send_bundle_response: Option<oneshot::Sender<SendBundleResult>> = None;

            if self.manual_bundling_mode.load(Ordering::Relaxed) {
                tokio::select! {
                    Some(r) = self.send_bundle_receiver.recv() => {
                        send_bundle_response = Some(r.responder);
                    }
                    _ = time::sleep(self.eth_poll_interval) => {
                        continue;
                    }
                }
            }

            last_block_number = block_watcher::wait_for_new_block_number(
                &*self.provider,
                last_block_number,
                self.eth_poll_interval,
            )
            .await;
            self.check_for_and_log_transaction_update().await;
            let result = self.send_bundle_with_increasing_gas_fees().await;
            match &result {
                SendBundleResult::Success {
                    block_number,
                    attempt_number,
                    tx_hash,
                } =>
                    if *attempt_number == 0 {
                        info!("Bundle with hash {tx_hash:?} landed in block {block_number}");
                    } else {
                        info!("Bundle with hash {tx_hash:?} landed in block {block_number} after increasing gas fees {attempt_number} time(s)");
                    }
                SendBundleResult::NoOperationsInitially => trace!("No ops to send at block {last_block_number}"),
                SendBundleResult::NoOperationsAfterFeeIncreases {
                    initial_op_count,
                    attempt_number,
                } => info!("Bundle initially had {initial_op_count} operations, but after increasing gas fees {attempt_number} time(s) it was empty"),
                SendBundleResult::StalledAtMaxFeeIncreases => warn!("Bundle failed to mine after {} fee increases", self.settings.max_fee_increases),
                SendBundleResult::Error(error) => {
                    BuilderMetrics::increment_bundle_txns_failed(self.id);
                    error!("Failed to send bundle. Will retry next block: {error:#?}");
                }
            }

            if let Some(t) = send_bundle_response.take() {
                if t.send(result).is_err() {
                    error!("Failed to send bundle result to manual caller");
                }
            }
        }
    }

    async fn check_for_and_log_transaction_update(&self) {
        let update = self.transaction_tracker.check_for_update_now().await;
        let update = match update {
            Ok(update) => update,
            Err(error) => {
                error!("Failed to check for transaction updates: {error:#?}");
                return;
            }
        };
        let Some(update) = update else {
            return;
        };
        match update {
            TrackerUpdate::Mined {
                tx_hash,
                block_number,
                attempt_number,
                ..
            } => {
                BuilderMetrics::increment_bundle_txns_success(self.id);
                if attempt_number == 0 {
                    info!("Bundle with hash {tx_hash:?} landed in block {block_number}");
                } else {
                    info!("Bundle with hash {tx_hash:?} landed in block {block_number} after increasing gas fees {attempt_number} time(s)");
                }
            }
            TrackerUpdate::StillPendingAfterWait => (),
            TrackerUpdate::LatestTxDropped { nonce } => {
                self.emit(BuilderEvent::latest_transaction_dropped(
                    self.id,
                    nonce.low_u64(),
                ));
                BuilderMetrics::increment_bundle_txns_dropped(self.id);
                info!("Previous transaction dropped by sender");
            }
            TrackerUpdate::NonceUsedForOtherTx { nonce } => {
                self.emit(BuilderEvent::nonce_used_for_other_transaction(
                    self.id,
                    nonce.low_u64(),
                ));
                BuilderMetrics::increment_bundle_txns_nonce_used(self.id);
                info!("Nonce used by external transaction")
            }
        };
    }

    /// Constructs a bundle and sends it to the entry point as a transaction. If
    /// the bundle fails to be mined after
    /// `settings.max_blocks_to_wait_for_mine` blocks, increases the gas fees by
    /// enough to send a replacement transaction, then constructs a new bundle
    /// using the new, higher gas requirements. Continues to retry with higher
    /// gas costs until one of the following happens:
    ///
    /// 1. A transaction succeeds (not necessarily the most recent one)
    /// 2. The gas fees are high enough that the bundle is empty because there
    ///    are no ops that meet the fee requirements.
    /// 3. The transaction has not succeeded after `settings.max_fee_increases`
    ///    replacements.
    async fn send_bundle_with_increasing_gas_fees(&self) -> SendBundleResult {
        let result = self.send_bundle_with_increasing_gas_fees_inner().await;
        match result {
            Ok(result) => result,
            Err(error) => SendBundleResult::Error(error),
        }
    }

    /// Helper function returning `Result` to be able to use `?`.
    async fn send_bundle_with_increasing_gas_fees_inner(&self) -> anyhow::Result<SendBundleResult> {
        let (nonce, mut required_fees) = self.transaction_tracker.get_nonce_and_required_fees()?;
        let mut initial_op_count: Option<usize> = None;
        for fee_increase_count in 0..=self.settings.max_fee_increases {
            let Some(bundle_tx) = self.get_bundle_tx(nonce, required_fees).await? else {
                self.emit(BuilderEvent::formed_bundle(
                    self.id,
                    None,
                    nonce.low_u64(),
                    fee_increase_count,
                    required_fees,
                ));
                return Ok(match initial_op_count {
                    Some(initial_op_count) => {
                        BuilderMetrics::increment_bundle_txns_abandoned(self.id);
                        SendBundleResult::NoOperationsAfterFeeIncreases {
                            initial_op_count,
                            attempt_number: fee_increase_count,
                        }
                    }
                    None => SendBundleResult::NoOperationsInitially,
                });
            };
            let BundleTx {
                tx,
                expected_storage,
                op_hashes,
            } = bundle_tx;
            if initial_op_count.is_none() {
                initial_op_count = Some(op_hashes.len());
            }
            let current_fees = GasFees::from(&tx);

            BuilderMetrics::increment_bundle_txns_sent(self.id);
            BuilderMetrics::set_current_fees(&current_fees);

            let send_result = self
                .transaction_tracker
                .send_transaction(tx.clone(), &expected_storage)
                .await?;
            let update = match send_result {
                SendResult::TrackerUpdate(update) => update,
                SendResult::TxHash(tx_hash) => {
                    self.emit(BuilderEvent::formed_bundle(
                        self.id,
                        Some(BundleTxDetails {
                            tx_hash,
                            tx,
                            op_hashes: Arc::new(op_hashes),
                        }),
                        nonce.low_u64(),
                        fee_increase_count,
                        required_fees,
                    ));
                    self.transaction_tracker.wait_for_update().await?
                }
            };
            match update {
                TrackerUpdate::Mined {
                    tx_hash,
                    nonce,
                    gas_fees: _,
                    block_number,
                    attempt_number,
                } => {
                    self.emit(BuilderEvent::transaction_mined(
                        self.id,
                        tx_hash,
                        nonce.low_u64(),
                        block_number,
                    ));
                    BuilderMetrics::increment_bundle_txns_success(self.id);
                    return Ok(SendBundleResult::Success {
                        block_number,
                        attempt_number,
                        tx_hash,
                    });
                }
                TrackerUpdate::StillPendingAfterWait => {
                    info!("Transaction not mined for several blocks")
                }
                TrackerUpdate::LatestTxDropped { nonce } => {
                    self.emit(BuilderEvent::latest_transaction_dropped(
                        self.id,
                        nonce.low_u64(),
                    ));
                    BuilderMetrics::increment_bundle_txns_dropped(self.id);
                    info!("Previous transaction dropped by sender");
                }
                TrackerUpdate::NonceUsedForOtherTx { nonce } => {
                    self.emit(BuilderEvent::nonce_used_for_other_transaction(
                        self.id,
                        nonce.low_u64(),
                    ));
                    BuilderMetrics::increment_bundle_txns_nonce_used(self.id);
                    bail!("nonce used by external transaction")
                }
            };
            info!(
                "Bundle transaction failed to mine after {fee_increase_count} fee increases (maxFeePerGas: {}, maxPriorityFeePerGas: {}).",
                current_fees.max_fee_per_gas,
                current_fees.max_priority_fee_per_gas,
            );
            BuilderMetrics::increment_bundle_txn_fee_increases(self.id);
            required_fees = Some(
                current_fees.increase_by_percent(self.settings.replacement_fee_percent_increase),
            );
        }
        BuilderMetrics::increment_bundle_txns_abandoned(self.id);
        Ok(SendBundleResult::StalledAtMaxFeeIncreases)
    }

    /// Builds a bundle and returns some metadata and the transaction to send
    /// it, or `None` if there are no valid operations available.
    async fn get_bundle_tx(
        &self,
        nonce: U256,
        required_fees: Option<GasFees>,
    ) -> anyhow::Result<Option<BundleTx>> {
        let bundle = self
            .proposer
            .make_bundle(required_fees)
            .await
            .context("proposer should create bundle for builder")?;
        let remove_ops_future = async {
            let result = self.remove_ops_from_pool(&bundle.rejected_ops).await;
            if let Err(error) = result {
                error!("Failed to remove rejected ops from pool: {error}");
            }
        };
        let remove_entities_future = async {
            let result = self
                .remove_entities_from_pool(&bundle.rejected_entities)
                .await;
            if let Err(error) = result {
                error!("Failed to remove rejected entities from pool: {error}");
            }
        };
        join!(remove_ops_future, remove_entities_future);
        if bundle.is_empty() {
            if !bundle.rejected_ops.is_empty() || !bundle.rejected_entities.is_empty() {
                info!(
                "Empty bundle with {} rejected ops and {} rejected entities. Removing them from pool.",
                bundle.rejected_ops.len(),
                bundle.rejected_entities.len()
            );
            }
            return Ok(None);
        }
        info!(
            "Selected bundle with {} op(s), with {} rejected op(s) and {} rejected entities",
            bundle.len(),
            bundle.rejected_ops.len(),
            bundle.rejected_entities.len()
        );
        let gas = math::increase_by_percent(bundle.gas_estimate, GAS_ESTIMATE_OVERHEAD_PERCENT);
        let op_hashes: Vec<_> = bundle.iter_ops().map(|op| self.op_hash(op)).collect();
        let mut tx = self.entry_point.get_send_bundle_transaction(
            bundle.ops_per_aggregator,
            self.beneficiary,
            gas,
            bundle.gas_fees,
        );
        tx.set_nonce(nonce);
        Ok(Some(BundleTx {
            tx,
            expected_storage: bundle.expected_storage,
            op_hashes,
        }))
    }

    async fn remove_ops_from_pool(&self, ops: &[UserOperation]) -> anyhow::Result<()> {
        self.op_pool
            .clone()
            .remove_ops(RemoveOpsRequest {
                entry_point: self.entry_point.address().as_bytes().to_vec(),
                hashes: ops
                    .iter()
                    .map(|op| self.op_hash(op).as_bytes().to_vec())
                    .collect(),
            })
            .await
            .context("builder should remove rejected ops from pool")?;
        Ok(())
    }

    async fn remove_entities_from_pool(&self, entities: &[Entity]) -> anyhow::Result<()> {
        self.op_pool
            .clone()
            .remove_entities(RemoveEntitiesRequest {
                entry_point: self.entry_point.address().as_bytes().to_vec(),
                entities: entities.iter().map(op_pool::Entity::from).collect(),
            })
            .await
            .context("builder should remove rejected entities from pool")?;
        Ok(())
    }

    fn op_hash(&self, op: &UserOperation) -> H256 {
        op.op_hash(self.entry_point.address(), self.chain_id)
    }

    fn emit(&self, event: BuilderEvent) {
        let _ = self.event_sender.send(WithEntryPoint {
            entry_point: self.entry_point.address(),
            event,
        });
    }
}

struct BuilderMetrics {}

impl BuilderMetrics {
    fn increment_bundle_txns_sent(id: u64) {
        metrics::increment_counter!("builder_bundle_txns_sent", "builder_id" => id.to_string());
    }

    fn increment_bundle_txns_success(id: u64) {
        metrics::increment_counter!("builder_bundle_txns_success", "builder_id" => id.to_string());
    }

    fn increment_bundle_txns_dropped(id: u64) {
        metrics::increment_counter!("builder_bundle_txns_dropped", "builder_id" => id.to_string());
    }

    // used when we decide to stop trying a transaction
    fn increment_bundle_txns_abandoned(id: u64) {
        metrics::increment_counter!("builder_bundle_txns_abandoned", "builder_id" => id.to_string());
    }

    // used when sending a transaction fails
    fn increment_bundle_txns_failed(id: u64) {
        metrics::increment_counter!("builder_bundle_txns_failed", "builder_id" => id.to_string());
    }

    fn increment_bundle_txns_nonce_used(id: u64) {
        metrics::increment_counter!("builder_bundle_txns_nonce_used", "builder_id" => id.to_string());
    }

    fn increment_bundle_txn_fee_increases(id: u64) {
        metrics::increment_counter!("builder_bundle_fee_increases", "builder_id" => id.to_string());
    }

    fn set_current_fees(fees: &GasFees) {
        metrics::gauge!(
            "builder_current_max_fee",
            fees.max_fee_per_gas.as_u128() as f64
        );
        metrics::gauge!(
            "builder_current_max_priority_fee",
            fees.max_priority_fee_per_gas.as_u128() as f64
        );
    }
}
