#![allow(clippy::let_unit_value)]

pub mod debug;
mod interconnect;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::iter::FromIterator;
use std::sync::Arc;

use fedimint_api::core::ModuleInstanceId;
use fedimint_api::db::{Database, DatabaseTransaction};
use fedimint_api::encoding::{Decodable, Encodable};
use fedimint_api::module::audit::Audit;
use fedimint_api::module::registry::{ModuleDecoderRegistry, ModuleRegistry, ServerModuleRegistry};
use fedimint_api::module::{ModuleError, TransactionItemAmount};
use fedimint_api::server::{DynServerModule, DynVerificationCache};
use fedimint_api::task::TaskGroup;
use fedimint_api::{Amount, OutPoint, PeerId, TransactionId};
use fedimint_core::epoch::*;
use fedimint_core::outcome::TransactionStatus;
use futures::future::select_all;
use hbbft::honey_badger::Batch;
use itertools::Itertools;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, error, info_span, instrument, trace, warn, Instrument};

use crate::config::{ModuleGenRegistry, ServerConfig};
use crate::consensus::interconnect::FedimintInterconnect;
use crate::db::{
    AcceptedTransactionKey, DropPeerKey, DropPeerKeyPrefix, EpochHistoryKey, LastEpochKey,
    ProposedTransactionKey, ProposedTransactionKeyPrefix, RejectedTransactionKey,
};
use crate::transaction::{Transaction, TransactionError};

pub type HbbftSerdeConsensusOutcome = hbbft::honey_badger::Batch<Vec<SerdeConsensusItem>, PeerId>;
pub type HbbftConsensusOutcome = hbbft::honey_badger::Batch<Vec<ConsensusItem>, PeerId>;
pub type HbbftMessage = hbbft::honey_badger::Message<PeerId>;

// TODO remove HBBFT `Batch` from `ConsensusOutcome`
#[derive(Debug, Clone)]
pub struct ConsensusOutcomeConversion(pub HbbftConsensusOutcome);

impl PartialEq<Self> for ConsensusOutcomeConversion {
    fn eq(&self, other: &Self) -> bool {
        self.0.epoch.eq(&other.0.epoch) && self.0.contributions.eq(&other.0.contributions)
    }
}

impl From<EpochOutcome> for ConsensusOutcomeConversion {
    fn from(history: EpochOutcome) -> Self {
        ConsensusOutcomeConversion(Batch {
            epoch: history.epoch,
            contributions: BTreeMap::from_iter(history.items.into_iter()),
        })
    }
}

/// Proposed HBBFT consensus changes including removing peers
#[derive(Debug, Clone)]
pub struct ConsensusProposal {
    pub items: Vec<ConsensusItem>,
    pub drop_peers: Vec<PeerId>,
}

// TODO: we should make other fields private and get rid of this
#[non_exhaustive]
pub struct FedimintConsensus {
    /// Configuration describing the federation and containing our secrets
    pub cfg: ServerConfig,

    pub module_inits: ModuleGenRegistry,

    pub modules: ServerModuleRegistry,
    /// KV Database into which all state is persisted to recover from in case of a crash
    pub db: Database,

    /// Notifies tasks when there is a new transaction
    pub transaction_notify: Arc<Notify>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub struct AcceptedTransaction {
    pub epoch: u64,
    pub transaction: Transaction,
}

#[derive(Debug)]
struct VerificationCaches {
    caches: HashMap<ModuleInstanceId, DynVerificationCache>,
}

struct FundingVerifier {
    input_amount: Amount,
    output_amount: Amount,
    fee_amount: Amount,
}

impl FedimintConsensus {
    pub async fn new(
        cfg: ServerConfig,
        db: Database,
        module_inits: ModuleGenRegistry,
        task_group: &mut TaskGroup,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            modules: module_inits.init_all(&cfg, &db, task_group).await?,
            cfg,
            module_inits,
            db,
            transaction_notify: Arc::new(Notify::new()),
        })
    }

    /// Like [`Self::new`], but when you want to initialize modules separately.
    pub fn new_with_modules(
        cfg: ServerConfig,
        db: Database,
        module_inits: ModuleGenRegistry,
        modules: ModuleRegistry<DynServerModule>,
    ) -> Self {
        Self {
            modules,
            cfg,
            module_inits,
            db,
            transaction_notify: Arc::new(Notify::new()),
        }
    }
}

impl VerificationCaches {
    fn get_cache(&self, module_key: ModuleInstanceId) -> &DynVerificationCache {
        self.caches
            .get(&module_key)
            .expect("Verification caches were built for all modules")
    }
}

impl FedimintConsensus {
    pub fn decoders(&self) -> ModuleDecoderRegistry {
        self.modules.decoder_registry()
    }

    pub async fn database_transaction(&self) -> DatabaseTransaction<'_> {
        self.db.begin_transaction().await
    }

    pub async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<(), TransactionSubmissionError> {
        // we already processed the transaction before the request was received
        if self
            .transaction_status(transaction.tx_hash())
            .await
            .is_some()
        {
            return Ok(());
        }

        let tx_hash = transaction.tx_hash();
        debug!(%tx_hash, "Received mint transaction");

        let mut funding_verifier = FundingVerifier::default();

        let mut pub_keys = Vec::new();

        // Create read-only DB tx so that the read state is consistent
        let mut dbtx = self.db.begin_transaction().await;

        for input in &transaction.inputs {
            let module = self.modules.get_expect(input.module_instance_id());

            let cache = module.build_verification_cache(&[input.clone()]);
            let interconnect = self.build_interconnect();
            let meta = module
                .validate_input(
                    &interconnect,
                    &mut dbtx.with_module_prefix(input.module_instance_id()),
                    &cache,
                    input,
                )
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;

            pub_keys.push(meta.puk_keys);
            funding_verifier.add_input(meta.amount);
        }
        transaction.validate_signature(pub_keys.into_iter().flatten())?;

        for output in &transaction.outputs {
            let amount = self
                .modules
                .get_expect(output.module_instance_id())
                .validate_output(
                    &mut dbtx.with_module_prefix(output.module_instance_id()),
                    output,
                )
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;
            funding_verifier.add_output(amount);
        }

        funding_verifier.verify_funding()?;

        let new = dbtx
            .insert_entry(&ProposedTransactionKey(tx_hash), &transaction)
            .await
            .expect("DB error");
        dbtx.commit_tx().await.expect("DB Error");

        if new.is_some() {
            warn!("Added consensus item was already in consensus queue");
        }

        self.transaction_notify.notify_one();
        Ok(())
    }

    /// Calculate the result of the `consensus_outcome` and save it/them.
    ///
    /// `reference_rejected_txs` should be `Some` if the `consensus_outcome` is coming from a
    /// a reference (already signed) `OutcomeHistory`, that contains `rejected_txs`,
    /// so we can check it against our own `rejected_txs` we calculate in this function.
    ///
    /// **Note**: `reference_rejected_txs` **must** come from a validated/trustworthy
    /// source and be correct, or it can cause a panic.
    #[instrument(skip_all, fields(epoch = consensus_outcome.epoch))]
    pub async fn process_consensus_outcome(
        &self,
        consensus_outcome: HbbftConsensusOutcome,
        reference_rejected_txs: Option<BTreeSet<TransactionId>>,
    ) -> SignedEpochOutcome {
        let epoch_history = self
            .db
            .autocommit(
                |dbtx| {
                    let consensus_outcome = consensus_outcome.clone();
                    let reference_rejected_txs = reference_rejected_txs.clone();

                    Box::pin(async move {
                        let epoch = consensus_outcome.epoch;
                        let outcome = consensus_outcome.clone();

                        let UnzipConsensusItem {
                            epoch_outcome_signature_share: _epoch_outcome_signature_share_cis,
                            transaction: transaction_cis,
                            module: module_cis,
                        } = consensus_outcome
                            .contributions
                            .into_iter()
                            .flat_map(|(peer, cis)| cis.into_iter().map(move |ci| (peer, ci)))
                            .unzip_consensus_item();

                        self.process_module_consensus_items(dbtx, &module_cis).await;

                        let rejected_txs = self
                            .process_transactions(dbtx, epoch, &transaction_cis)
                            .await;

                        if let Some(reference_rejected_txs) = reference_rejected_txs.as_ref() {
                            // Result of the consensus are supposed to be deterministic.
                            // If our result is not the same as what the (honest) majority of the federation
                            // signed over, it's a catastrophical bug/mismatch of Federation's fedimintd
                            // implementations.
                            assert_eq!(
                                reference_rejected_txs, &rejected_txs,
                                "rejected_txs mismatch: reference = {:?} != {:?}",
                                reference_rejected_txs, rejected_txs
                            );
                        }

                        let epoch_history = self
                            .finalize_process_epoch(dbtx, outcome.clone(), rejected_txs)
                            .await;
                        Result::<_, ()>::Ok(epoch_history)
                    })
                },
                Some(100),
            )
            .await
            .expect("Committing consensus epoch failed");

        let audit = self.audit().await;
        if audit.sum().milli_sat < 0 {
            panic!(
                "Balance sheet of the fed has gone negative, this should never happen! {}",
                audit
            )
        }

        epoch_history
    }

    /// Calls `begin_consensus_epoch` on all modules, dispatching their consensus items
    async fn process_module_consensus_items(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        module_cis: &[(PeerId, fedimint_api::core::DynModuleConsensusItem)],
    ) {
        let per_module_cis: HashMap<
            ModuleInstanceId,
            Vec<(PeerId, fedimint_api::core::DynModuleConsensusItem)>,
        > = module_cis
            .iter()
            .cloned()
            .into_group_map_by(|(_peer, mci)| mci.module_instance_id());

        for (module_key, module_cis) in per_module_cis {
            self.modules
                .get_expect(module_key)
                .begin_consensus_epoch(&mut dbtx.with_module_prefix(module_key), module_cis)
                .await;
        }
    }

    /// Applies all valid fedimint transactions to the database transaction `dbtx` and returns a set
    /// of invalid transactions that were filtered out
    async fn process_transactions(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        epoch: u64,
        transactions: &[(PeerId, Transaction)],
    ) -> BTreeSet<TransactionId> {
        // Process transactions
        let mut rejected_txs: BTreeSet<TransactionId> = BTreeSet::new();

        let caches = self.build_verification_caches(transactions.iter().map(|(_, tx)| tx));
        let mut processed_txs: HashSet<TransactionId> = HashSet::new();

        for (_, transaction) in transactions.iter().cloned() {
            let txid: TransactionId = transaction.tx_hash();
            if !processed_txs.insert(txid) {
                // Avoid processing duplicate tx from different peers
                continue;
            }

            let span = info_span!("Processing transaction");
            async {
                trace!(?transaction);
                dbtx.remove_entry(&ProposedTransactionKey(txid))
                    .await
                    .expect("DB Error");

                dbtx.set_tx_savepoint().await;
                // TODO: use borrowed transaction
                match self
                    .process_transaction(dbtx, transaction.clone(), &caches)
                    .await
                {
                    Ok(()) => {
                        dbtx.insert_entry(
                            &AcceptedTransactionKey(txid),
                            &AcceptedTransaction { epoch, transaction },
                        )
                        .await
                        .expect("DB Error");
                    }
                    Err(error) => {
                        rejected_txs.insert(txid);
                        dbtx.rollback_tx_to_savepoint().await;
                        warn!(%error, "Transaction failed");
                        dbtx.insert_entry(&RejectedTransactionKey(txid), &format!("{:?}", error))
                            .await
                            .expect("DB Error");
                    }
                }
            }
            .instrument(span)
            .await;
        }

        rejected_txs
    }

    /// Saves the epoch history, calls `end_consensus_epoch` on all modules and bans misbehaving
    /// peers
    async fn finalize_process_epoch(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        outcome: HbbftConsensusOutcome,
        rejected_txs: BTreeSet<TransactionId>,
    ) -> SignedEpochOutcome {
        let epoch_peers: HashSet<PeerId> = outcome.contributions.keys().copied().collect();

        let mut drop_peers = Vec::<PeerId>::new();

        let epoch_history = self
            .save_epoch_history(outcome.clone(), dbtx, &mut drop_peers, rejected_txs)
            .await;

        for (module_key, module) in self.modules.iter_modules() {
            let module_drop_peers = module
                .end_consensus_epoch(&epoch_peers, &mut dbtx.with_module_prefix(module_key))
                .await;
            drop_peers.extend(module_drop_peers);
        }

        for peer in drop_peers {
            dbtx.insert_entry(&DropPeerKey(peer), &())
                .await
                .expect("DB Error");
        }

        epoch_history
    }

    pub async fn get_last_epoch(&self) -> Option<u64> {
        self.db
            .begin_transaction()
            .await
            .get_value(&LastEpochKey)
            .await
            .expect("db query must not fail")
            .map(|e| e.0)
    }

    pub async fn epoch_history(&self, epoch: u64) -> Option<SignedEpochOutcome> {
        self.db
            .begin_transaction()
            .await
            .get_value(&EpochHistoryKey(epoch))
            .await
            .unwrap()
    }

    async fn save_epoch_history<'a>(
        &self,
        outcome: HbbftConsensusOutcome,
        dbtx: &mut DatabaseTransaction<'a>,
        drop_peers: &mut Vec<PeerId>,
        rejected_txs: BTreeSet<TransactionId>,
    ) -> SignedEpochOutcome {
        let prev_epoch_key = EpochHistoryKey(outcome.epoch.saturating_sub(1));
        let peers: Vec<PeerId> = outcome.contributions.keys().cloned().collect();
        let maybe_prev_epoch = dbtx.get_value(&prev_epoch_key).await.expect("DB error");

        let current = SignedEpochOutcome::new(
            outcome.epoch,
            outcome.contributions,
            rejected_txs,
            maybe_prev_epoch.as_ref(),
        );

        // validate and update sigs on prev epoch
        if let Some(prev_epoch) = maybe_prev_epoch {
            let pks = &self.cfg.consensus.epoch_pk_set;

            match current.add_sig_to_prev(pks, prev_epoch) {
                Ok(prev_epoch) => {
                    dbtx.insert_entry(&prev_epoch_key, &prev_epoch)
                        .await
                        .expect("DB Error");
                }
                Err(EpochVerifyError::NotEnoughValidSigShares(contributing_peers)) => {
                    warn!("Unable to sign epoch {}", prev_epoch_key.0);
                    for peer in peers {
                        if !contributing_peers.contains(&peer) {
                            warn!("Dropping {} for not contributing valid epoch sigs.", peer);
                            drop_peers.push(peer);
                        }
                    }
                }
                Err(_) => panic!("Not possible"),
            }
        }

        dbtx.insert_entry(&LastEpochKey, &EpochHistoryKey(current.outcome.epoch))
            .await
            .expect("DB Error");
        dbtx.insert_entry(&EpochHistoryKey(current.outcome.epoch), &current)
            .await
            .expect("DB Error");

        current
    }

    pub async fn await_consensus_proposal(&self) {
        let proposal_futures = self
            .modules
            .iter_modules()
            .map(|(module_instance_id, module)| {
                Box::pin(async move {
                    let mut dbtx = self.database_transaction().await;
                    let mut module_dbtx = dbtx.with_module_prefix(module_instance_id);
                    module.await_consensus_proposal(&mut module_dbtx).await
                })
            })
            .collect::<Vec<_>>();

        select_all(proposal_futures).await;
    }

    pub async fn get_consensus_proposal(&self) -> ConsensusProposal {
        let mut dbtx = self.database_transaction().await;

        let drop_peers = dbtx
            .find_by_prefix(&DropPeerKeyPrefix)
            .await
            .map(|res| {
                let key = res.expect("DB error").0;
                key.0
            })
            .collect();

        let mut items: Vec<ConsensusItem> = dbtx
            .find_by_prefix(&ProposedTransactionKeyPrefix)
            .await
            .map(|res| {
                let (_key, value) = res.expect("DB error");
                ConsensusItem::Transaction(value)
            })
            .collect();

        for (instance_id, module) in self.modules.iter_modules() {
            items.extend(
                module
                    .consensus_proposal(&mut dbtx.with_module_prefix(instance_id), instance_id)
                    .await
                    .into_iter()
                    .map(ConsensusItem::Module),
            );
        }

        if let Some(epoch) = dbtx.get_value(&LastEpochKey).await.unwrap() {
            let last_epoch = dbtx.get_value(&epoch).await.unwrap().unwrap();
            let sig = self.cfg.private.epoch_sks.0.sign(last_epoch.hash);
            let item = ConsensusItem::EpochOutcomeSignatureShare(EpochOutcomeSignatureShare(sig));
            items.push(item);
        };

        ConsensusProposal { items, drop_peers }
    }

    async fn process_transaction<'a>(
        &self,
        dbtx: &mut DatabaseTransaction<'a>,
        transaction: Transaction,
        caches: &VerificationCaches,
    ) -> Result<(), TransactionSubmissionError> {
        let mut funding_verifier = FundingVerifier::default();

        let tx_hash = transaction.tx_hash();

        let mut pub_keys = Vec::new();
        for input in transaction.inputs.iter() {
            let meta = self
                .modules
                .get_expect(input.module_instance_id())
                .apply_input(
                    &self.build_interconnect(),
                    &mut dbtx.with_module_prefix(input.module_instance_id()),
                    input,
                    caches.get_cache(input.module_instance_id()),
                )
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;
            pub_keys.push(meta.puk_keys);
            funding_verifier.add_input(meta.amount);
        }
        transaction.validate_signature(pub_keys.into_iter().flatten())?;

        for (idx, output) in transaction.outputs.into_iter().enumerate() {
            let out_point = OutPoint {
                txid: tx_hash,
                out_idx: idx as u64,
            };
            let amount = self
                .modules
                .get_expect(output.module_instance_id())
                .apply_output(
                    &mut dbtx.with_module_prefix(output.module_instance_id()),
                    &output,
                    out_point,
                )
                .await
                .map_err(|e| TransactionSubmissionError::ModuleError(tx_hash, e))?;
            funding_verifier.add_output(amount);
        }

        funding_verifier.verify_funding()?;

        Ok(())
    }

    pub async fn transaction_status(
        &self,
        txid: TransactionId,
    ) -> Option<crate::outcome::TransactionStatus> {
        let mut dbtx = self.database_transaction().await;

        let accepted: Option<AcceptedTransaction> = dbtx
            .get_value(&AcceptedTransactionKey(txid))
            .await
            .expect("DB error");

        if let Some(accepted_tx) = accepted {
            let mut outputs = Vec::new();
            for (out_idx, output) in accepted_tx.transaction.outputs.iter().enumerate() {
                let outpoint = OutPoint {
                    txid,
                    out_idx: out_idx as u64,
                };
                let outcome = self
                    .modules
                    .get_expect(output.module_instance_id())
                    .output_status(
                        &mut dbtx.with_module_prefix(output.module_instance_id()),
                        outpoint,
                        output.module_instance_id(),
                    )
                    .await
                    .expect("the transaction was processed, so should be known");
                outputs.push((&outcome).into())
            }

            return Some(crate::outcome::TransactionStatus::Accepted {
                epoch: accepted_tx.epoch,
                outputs,
            });
        }

        let rejected: Option<String> = self
            .db
            .begin_transaction()
            .await
            .get_value(&RejectedTransactionKey(txid))
            .await
            .expect("DB error");

        if let Some(message) = rejected {
            return Some(TransactionStatus::Rejected(message));
        }

        None
    }

    fn build_verification_caches<'a>(
        &self,
        transactions: impl Iterator<Item = &'a Transaction> + Send,
    ) -> VerificationCaches {
        let module_inputs = transactions
            .flat_map(|tx| tx.inputs.iter())
            .cloned()
            .into_group_map_by(|input| input.module_instance_id());

        // TODO: should probably run in parallel, but currently only the mint does anything at all
        let caches = module_inputs
            .into_iter()
            .map(|(module_key, inputs)| {
                let module = self.modules.get_expect(module_key);
                (module_key, module.build_verification_cache(&inputs))
            })
            .collect();

        VerificationCaches { caches }
    }

    pub async fn audit(&self) -> Audit {
        let mut dbtx = self.database_transaction().await;
        let mut audit = Audit::default();
        for (module_instance_id, module) in self.modules.iter_modules() {
            module
                .audit(&mut dbtx.with_module_prefix(module_instance_id), &mut audit)
                .await
        }
        audit
    }

    fn build_interconnect(&self) -> FedimintInterconnect {
        FedimintInterconnect { fedimint: self }
    }
}

impl FundingVerifier {
    fn add_input(&mut self, input_amount: TransactionItemAmount) {
        self.input_amount += input_amount.amount;
        self.fee_amount += input_amount.fee;
    }

    fn add_output(&mut self, output_amount: TransactionItemAmount) {
        self.output_amount += output_amount.amount;
        self.fee_amount += output_amount.fee;
    }

    fn verify_funding(self) -> Result<(), TransactionError> {
        if self.input_amount == (self.output_amount + self.fee_amount) {
            Ok(())
        } else {
            Err(TransactionError::UnbalancedTransaction {
                inputs: self.input_amount,
                outputs: self.output_amount,
                fee: self.fee_amount,
            })
        }
    }
}

impl Default for FundingVerifier {
    fn default() -> Self {
        FundingVerifier {
            input_amount: Amount::ZERO,
            output_amount: Amount::ZERO,
            fee_amount: Amount::ZERO,
        }
    }
}

#[derive(Debug, Error)]
pub enum TransactionSubmissionError {
    #[error("High level transaction error: {0}")]
    TransactionError(#[from] TransactionError),
    #[error("Module input or output error in tx {0}: {1}")]
    ModuleError(TransactionId, ModuleError),
    #[error("Transaction conflict error")]
    TransactionConflictError,
}
