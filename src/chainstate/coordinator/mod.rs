// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::cmp;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::burnchains::{
    affirmation::{AffirmationMap, AffirmationMapEntry},
    bitcoin::indexer::BitcoinIndexer,
    db::{
        BlockCommitMetadata, BurnchainBlockData, BurnchainDB, BurnchainDBTransaction,
        BurnchainHeaderReader,
    },
    Address, Burnchain, BurnchainBlockHeader, Error as BurnchainError, PoxConstants, Txid,
};
use crate::chainstate::burn::{
    db::sortdb::SortitionDB,
    operations::leader_block_commit::{RewardSetInfo, BURN_BLOCK_MINED_AT_MODULUS},
    operations::BlockstackOperationType,
    operations::LeaderBlockCommitOp,
    BlockSnapshot, ConsensusHash,
};
use crate::chainstate::coordinator::comm::{
    ArcCounterCoordinatorNotices, CoordinatorEvents, CoordinatorNotices, CoordinatorReceivers,
};
use crate::chainstate::stacks::address::PoxAddress;
use crate::chainstate::stacks::index::MarfTrieId;
use crate::chainstate::stacks::{
    db::{
        accounts::MinerReward, ChainStateBootData, ClarityTx, MinerRewardInfo, StacksChainState,
        StacksEpochReceipt, StacksHeaderInfo,
    },
    events::{StacksTransactionEvent, StacksTransactionReceipt, TransactionOrigin},
    miner::{signal_mining_blocked, signal_mining_ready, MinerStatus},
    Error as ChainstateError, StacksBlock, StacksBlockHeader, TransactionPayload,
};
use crate::core::StacksEpoch;
use crate::monitoring::{
    increment_contract_calls_processed, increment_stx_blocks_processed_counter,
};
use crate::net::atlas::{AtlasConfig, AttachmentInstance};
use crate::util_lib::db::DBConn;
use crate::util_lib::db::DBTx;
use crate::util_lib::db::Error as DBError;
use clarity::vm::{
    costs::ExecutionCost,
    types::{PrincipalData, QualifiedContractIdentifier},
    Value,
};

use crate::core::StacksEpochId;
use crate::cost_estimates::{CostEstimator, FeeEstimator, PessimisticEstimator};
use crate::types::chainstate::{
    BlockHeaderHash, BurnchainHeaderHash, PoxId, SortitionId, StacksBlockId,
};
use clarity::vm::database::BurnStateDB;

use crate::chainstate::stacks::index::marf::MARFOpenOpts;

pub use self::comm::CoordinatorCommunication;

use super::stacks::boot::RewardSet;
use stacks_common::util::get_epoch_time_secs;

pub mod comm;
#[cfg(test)]
pub mod tests;

/// The 3 different states for the current
///  reward cycle's relationship to its PoX anchor
#[derive(Debug, Clone, PartialEq)]
pub enum PoxAnchorBlockStatus {
    SelectedAndKnown(BlockHeaderHash, RewardSet),
    SelectedAndUnknown(BlockHeaderHash),
    NotSelected,
}

#[derive(Debug, PartialEq)]
pub struct RewardCycleInfo {
    pub anchor_status: PoxAnchorBlockStatus,
}

impl RewardCycleInfo {
    pub fn selected_anchor_block(&self) -> Option<&BlockHeaderHash> {
        use self::PoxAnchorBlockStatus::*;
        match self.anchor_status {
            SelectedAndUnknown(ref block) | SelectedAndKnown(ref block, _) => Some(block),
            NotSelected => None,
        }
    }
    pub fn is_reward_info_known(&self) -> bool {
        use self::PoxAnchorBlockStatus::*;
        match self.anchor_status {
            SelectedAndUnknown(_) => false,
            SelectedAndKnown(_, _) | NotSelected => true,
        }
    }
    pub fn known_selected_anchor_block(&self) -> Option<&RewardSet> {
        use self::PoxAnchorBlockStatus::*;
        match self.anchor_status {
            SelectedAndUnknown(_) => None,
            SelectedAndKnown(_, ref reward_set) => Some(reward_set),
            NotSelected => None,
        }
    }
    pub fn known_selected_anchor_block_owned(self) -> Option<RewardSet> {
        use self::PoxAnchorBlockStatus::*;
        match self.anchor_status {
            SelectedAndUnknown(_) => None,
            SelectedAndKnown(_, reward_set) => Some(reward_set),
            NotSelected => None,
        }
    }
}

pub trait BlockEventDispatcher {
    fn announce_block(
        &self,
        block: &StacksBlock,
        metadata: &StacksHeaderInfo,
        receipts: &Vec<StacksTransactionReceipt>,
        parent: &StacksBlockId,
        winner_txid: Txid,
        matured_rewards: &Vec<MinerReward>,
        matured_rewards_info: Option<&MinerRewardInfo>,
        parent_burn_block_hash: BurnchainHeaderHash,
        parent_burn_block_height: u32,
        parent_burn_block_timestamp: u64,
        anchored_consumed: &ExecutionCost,
        mblock_confirmed_consumed: &ExecutionCost,
        pox_constants: &PoxConstants,
    );

    /// called whenever a burn block is about to be
    ///  processed for sortition. note, in the event
    ///  of PoX forks, this will be called _multiple_
    ///  times for the same burnchain header hash.
    fn announce_burn_block(
        &self,
        burn_block: &BurnchainHeaderHash,
        burn_block_height: u64,
        rewards: Vec<(PoxAddress, u64)>,
        burns: u64,
        reward_recipients: Vec<PoxAddress>,
    );

    fn dispatch_boot_receipts(&mut self, receipts: Vec<StacksTransactionReceipt>);
}

pub struct ChainsCoordinatorConfig {
    /// true: use affirmation maps before 2.1
    /// false: only use affirmation maps in 2.1 or later
    pub always_use_affirmation_maps: bool,
}

impl ChainsCoordinatorConfig {
    pub fn new() -> ChainsCoordinatorConfig {
        ChainsCoordinatorConfig {
            always_use_affirmation_maps: false,
        }
    }
}

pub struct ChainsCoordinator<
    'a,
    T: BlockEventDispatcher,
    N: CoordinatorNotices,
    R: RewardSetProvider,
    CE: CostEstimator + ?Sized,
    FE: FeeEstimator + ?Sized,
> {
    canonical_sortition_tip: Option<SortitionId>,
    canonical_chain_tip: Option<StacksBlockId>,
    canonical_pox_id: Option<PoxId>,
    heaviest_anchor_block_affirmation_map: Option<AffirmationMap>,
    burnchain_blocks_db: BurnchainDB,
    chain_state_db: StacksChainState,
    sortition_db: SortitionDB,
    burnchain: Burnchain,
    attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
    dispatcher: Option<&'a T>,
    cost_estimator: Option<&'a mut CE>,
    fee_estimator: Option<&'a mut FE>,
    reward_set_provider: R,
    notifier: N,
    atlas_config: AtlasConfig,
    config: ChainsCoordinatorConfig,
}

#[derive(Debug)]
pub enum Error {
    BurnchainBlockAlreadyProcessed,
    BurnchainError(BurnchainError),
    ChainstateError(ChainstateError),
    NonContiguousBurnchainBlock(BurnchainError),
    NoSortitions,
    FailedToProcessSortition(BurnchainError),
    DBError(DBError),
    NotPrepareEndBlock,
    NotPoXAnchorBlock,
}

impl From<BurnchainError> for Error {
    fn from(o: BurnchainError) -> Error {
        Error::BurnchainError(o)
    }
}

impl From<ChainstateError> for Error {
    fn from(o: ChainstateError) -> Error {
        Error::ChainstateError(o)
    }
}

impl From<DBError> for Error {
    fn from(o: DBError) -> Error {
        Error::DBError(o)
    }
}

pub trait RewardSetProvider {
    fn get_reward_set(
        &self,
        current_burn_height: u64,
        chainstate: &mut StacksChainState,
        burnchain: &Burnchain,
        sortdb: &SortitionDB,
        block_id: &StacksBlockId,
    ) -> Result<RewardSet, Error>;
}

pub struct OnChainRewardSetProvider();

impl RewardSetProvider for OnChainRewardSetProvider {
    fn get_reward_set(
        &self,
        current_burn_height: u64,
        chainstate: &mut StacksChainState,
        burnchain: &Burnchain,
        sortdb: &SortitionDB,
        block_id: &StacksBlockId,
    ) -> Result<RewardSet, Error> {
        let registered_addrs =
            chainstate.get_reward_addresses(burnchain, sortdb, current_burn_height, block_id)?;

        let liquid_ustx = chainstate.get_liquid_ustx(block_id);

        let (threshold, participation) = StacksChainState::get_reward_threshold_and_participation(
            &burnchain.pox_constants,
            &registered_addrs[..],
            liquid_ustx,
        );

        if !burnchain
            .pox_constants
            .enough_participation(participation, liquid_ustx)
        {
            info!("PoX reward cycle did not have enough participation. Defaulting to burn";
                  "burn_height" => current_burn_height,
                  "participation" => participation,
                  "liquid_ustx" => liquid_ustx,
                  "registered_addrs" => registered_addrs.len());
            return Ok(RewardSet::empty());
        } else {
            info!("PoX reward cycle threshold computed";
                  "burn_height" => current_burn_height,
                  "threshold" => threshold,
                  "participation" => participation,
                  "liquid_ustx" => liquid_ustx,
                  "registered_addrs" => registered_addrs.len());
        }

        let cur_epoch = SortitionDB::get_stacks_epoch(sortdb.conn(), current_burn_height)?.expect(
            &format!("FATAL: no epoch for burn height {}", current_burn_height),
        );

        Ok(StacksChainState::make_reward_set(
            threshold,
            registered_addrs,
            cur_epoch.epoch_id,
        ))
    }
}

impl<'a, T: BlockEventDispatcher, CE: CostEstimator + ?Sized, FE: FeeEstimator + ?Sized>
    ChainsCoordinator<'a, T, ArcCounterCoordinatorNotices, OnChainRewardSetProvider, CE, FE>
{
    pub fn run(
        config: ChainsCoordinatorConfig,
        chain_state_db: StacksChainState,
        burnchain: Burnchain,
        attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
        dispatcher: &'a mut T,
        comms: CoordinatorReceivers,
        atlas_config: AtlasConfig,
        cost_estimator: Option<&mut CE>,
        fee_estimator: Option<&mut FE>,
        miner_status: Arc<Mutex<MinerStatus>>,
    ) where
        T: BlockEventDispatcher,
    {
        let stacks_blocks_processed = comms.stacks_blocks_processed.clone();
        let sortitions_processed = comms.sortitions_processed.clone();

        let sortition_db = SortitionDB::open(
            &burnchain.get_db_path(),
            true,
            burnchain.pox_constants.clone(),
        )
        .unwrap();
        let burnchain_blocks_db =
            BurnchainDB::open(&burnchain.get_burnchaindb_path(), false).unwrap();

        let canonical_sortition_tip =
            SortitionDB::get_canonical_sortition_tip(sortition_db.conn()).unwrap();

        let heaviest_am = BurnchainDB::get_heaviest_anchor_block_affirmation_map(
            burnchain_blocks_db.conn(),
            &burnchain,
        )
        .unwrap();

        let arc_notices = ArcCounterCoordinatorNotices {
            stacks_blocks_processed,
            sortitions_processed,
        };

        let mut inst = ChainsCoordinator {
            canonical_chain_tip: None,
            canonical_sortition_tip: Some(canonical_sortition_tip),
            canonical_pox_id: None,
            heaviest_anchor_block_affirmation_map: Some(heaviest_am),
            burnchain_blocks_db,
            chain_state_db,
            sortition_db,
            burnchain,
            attachments_tx,
            dispatcher: Some(dispatcher),
            notifier: arc_notices,
            reward_set_provider: OnChainRewardSetProvider(),
            cost_estimator,
            fee_estimator,
            atlas_config,
            config,
        };

        let mut missing_affirmed_anchor_block = None;
        let mut last_retry_ts = 0;
        let retry_interval = 5;
        loop {
            // timeout so that we handle Ctrl-C a little gracefully
            match comms.wait_on() {
                CoordinatorEvents::NEW_STACKS_BLOCK => {
                    signal_mining_blocked(miner_status.clone());
                    debug!("Received new stacks block notice");
                    match inst.handle_new_stacks_block() {
                        Ok(missing_block_opt) => {
                            if missing_block_opt.is_some() {
                                missing_affirmed_anchor_block = missing_block_opt;
                            }
                        }
                        Err(e) => {
                            warn!("Error processing new stacks block: {:?}", e);
                        }
                    }
                    signal_mining_ready(miner_status.clone());
                }
                CoordinatorEvents::NEW_BURN_BLOCK => {
                    signal_mining_blocked(miner_status.clone());
                    debug!("Received new burn block notice");
                    match inst.handle_new_burnchain_block() {
                        Ok(missing_block_opt) => {
                            if missing_block_opt.is_some() {
                                missing_affirmed_anchor_block = missing_block_opt;
                            }
                        }
                        Err(e) => {
                            warn!("Error processing new burn block: {:?}", e);
                        }
                    }
                    signal_mining_ready(miner_status.clone());
                }
                CoordinatorEvents::STOP => {
                    signal_mining_blocked(miner_status.clone());
                    debug!("Received stop notice");
                    return;
                }
                CoordinatorEvents::TIMEOUT => {}
            }

            if let Some(missing_block) = missing_affirmed_anchor_block {
                if last_retry_ts + retry_interval < get_epoch_time_secs() {
                    // periodically retry processing sortitions in the event that a missing anchor
                    // block arrives
                    match inst.retry_burnchain_block() {
                        Ok(missing_block_opt) => {
                            if missing_block_opt.is_none() {
                                debug!(
                                    "Successfully processed missing affirmed anchor block {}",
                                    &missing_block
                                );
                            }

                            missing_affirmed_anchor_block = missing_block_opt;
                        }
                        Err(e) => {
                            warn!("Error retrying sortition-processing with missing affirmed anchor block: {:?}", e);
                        }
                    }
                    last_retry_ts = get_epoch_time_secs();
                }
            }
        }
    }
}

impl<'a, T: BlockEventDispatcher, U: RewardSetProvider> ChainsCoordinator<'a, T, (), U, (), ()> {
    #[cfg(test)]
    pub fn test_new(
        burnchain: &Burnchain,
        chain_id: u32,
        path: &str,
        reward_set_provider: U,
        attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
    ) -> ChainsCoordinator<'a, T, (), U, (), ()> {
        ChainsCoordinator::test_new_with_observer(
            burnchain,
            chain_id,
            path,
            reward_set_provider,
            attachments_tx,
            None,
        )
    }

    #[cfg(test)]
    pub fn test_new_with_observer(
        burnchain: &Burnchain,
        chain_id: u32,
        path: &str,
        reward_set_provider: U,
        attachments_tx: SyncSender<HashSet<AttachmentInstance>>,
        dispatcher: Option<&'a T>,
    ) -> ChainsCoordinator<'a, T, (), U, (), ()> {
        let burnchain = burnchain.clone();

        let mut boot_data = ChainStateBootData::new(&burnchain, vec![], None);

        let sortition_db = SortitionDB::open(
            &burnchain.get_db_path(),
            true,
            burnchain.pox_constants.clone(),
        )
        .unwrap();
        let burnchain_blocks_db =
            BurnchainDB::open(&burnchain.get_burnchaindb_path(), false).unwrap();
        let (chain_state_db, _) = StacksChainState::open_and_exec(
            false,
            chain_id,
            &format!("{}/chainstate/", path),
            Some(&mut boot_data),
            None,
        )
        .unwrap();
        let canonical_sortition_tip =
            SortitionDB::get_canonical_sortition_tip(sortition_db.conn()).unwrap();

        let heaviest_am = BurnchainDB::get_heaviest_anchor_block_affirmation_map(
            burnchain_blocks_db.conn(),
            &burnchain,
        )
        .unwrap();

        ChainsCoordinator {
            canonical_chain_tip: None,
            canonical_sortition_tip: Some(canonical_sortition_tip),
            canonical_pox_id: None,
            heaviest_anchor_block_affirmation_map: Some(heaviest_am),
            burnchain_blocks_db,
            chain_state_db,
            sortition_db,
            burnchain,
            dispatcher,
            cost_estimator: None,
            fee_estimator: None,
            reward_set_provider,
            notifier: (),
            attachments_tx,
            atlas_config: AtlasConfig::default(false),
            config: ChainsCoordinatorConfig::new(),
        }
    }
}

pub fn get_next_recipients<U: RewardSetProvider>(
    sortition_tip: &BlockSnapshot,
    chain_state: &mut StacksChainState,
    sort_db: &mut SortitionDB,
    burnchain: &Burnchain,
    provider: &U,
) -> Result<Option<RewardSetInfo>, Error> {
    let reward_cycle_info = get_reward_cycle_info(
        sortition_tip.block_height + 1,
        &sortition_tip.burn_header_hash,
        &sortition_tip.sortition_id,
        burnchain,
        chain_state,
        sort_db,
        provider,
    )?;
    sort_db
        .get_next_block_recipients(burnchain, sortition_tip, reward_cycle_info.as_ref())
        .map_err(|e| Error::from(e))
}

/// returns None if this burnchain block is _not_ the start of a reward cycle
///         otherwise, returns the required reward cycle info for this burnchain block
///                     in our current sortition view:
///           * PoX anchor block
///           * Was PoX anchor block known?
pub fn get_reward_cycle_info<U: RewardSetProvider>(
    burn_height: u64,
    parent_bhh: &BurnchainHeaderHash,
    sortition_tip: &SortitionId,
    burnchain: &Burnchain,
    chain_state: &mut StacksChainState,
    sort_db: &SortitionDB,
    provider: &U,
) -> Result<Option<RewardCycleInfo>, Error> {
    let epoch_at_height = SortitionDB::get_stacks_epoch(sort_db.conn(), burn_height)?.expect(
        &format!("FATAL: no epoch defined for burn height {}", burn_height),
    );

    if burnchain.is_reward_cycle_start(burn_height) {
        if burnchain
            .pox_constants
            .is_after_pox_sunset_end(burn_height, epoch_at_height.epoch_id)
        {
            return Ok(Some(RewardCycleInfo {
                anchor_status: PoxAnchorBlockStatus::NotSelected,
            }));
        }

        debug!("Beginning reward cycle";
              "burn_height" => burn_height,
              "reward_cycle_length" => burnchain.pox_constants.reward_cycle_length,
              "prepare_phase_length" => burnchain.pox_constants.prepare_length);

        let reward_cycle_info = {
            let ic = sort_db.index_handle(sortition_tip);
            ic.get_chosen_pox_anchor(&parent_bhh, &burnchain.pox_constants)
        }?;
        if let Some((consensus_hash, stacks_block_hash)) = reward_cycle_info {
            // it may have been elected, but we only process it if it's affirmed by the network!
            info!("Anchor block selected: {}", stacks_block_hash);
            let anchor_block_known = StacksChainState::is_stacks_block_processed(
                &chain_state.db(),
                &consensus_hash,
                &stacks_block_hash,
            )?;
            let anchor_status = if anchor_block_known {
                let block_id = StacksBlockId::new(&consensus_hash, &stacks_block_hash);
                let reward_set = provider.get_reward_set(
                    burn_height,
                    chain_state,
                    burnchain,
                    sort_db,
                    &block_id,
                )?;
                test_debug!(
                    "Stacks anchor block {}/{} is processed",
                    &consensus_hash,
                    &stacks_block_hash
                );
                PoxAnchorBlockStatus::SelectedAndKnown(stacks_block_hash, reward_set)
            } else {
                test_debug!(
                    "Stacks anchor block {}/{} is NOT processed",
                    &consensus_hash,
                    &stacks_block_hash
                );
                PoxAnchorBlockStatus::SelectedAndUnknown(stacks_block_hash)
            };
            Ok(Some(RewardCycleInfo { anchor_status }))
        } else {
            Ok(Some(RewardCycleInfo {
                anchor_status: PoxAnchorBlockStatus::NotSelected,
            }))
        }
    } else {
        Ok(None)
    }
}

struct PaidRewards {
    pox: Vec<(PoxAddress, u64)>,
    burns: u64,
}

fn calculate_paid_rewards(ops: &[BlockstackOperationType]) -> PaidRewards {
    let mut reward_recipients: HashMap<_, u64> = HashMap::new();
    let mut burn_amt = 0;
    for op in ops.iter() {
        if let BlockstackOperationType::LeaderBlockCommit(commit) = op {
            if commit.commit_outs.len() == 0 {
                continue;
            }
            let amt_per_address = commit.burn_fee / (commit.commit_outs.len() as u64);
            for addr in commit.commit_outs.iter() {
                if addr.is_burn() {
                    burn_amt += amt_per_address;
                } else {
                    if let Some(prior_amt) = reward_recipients.get_mut(addr) {
                        *prior_amt += amt_per_address;
                    } else {
                        reward_recipients.insert(addr.clone(), amt_per_address);
                    }
                }
            }
        }
    }
    PaidRewards {
        pox: reward_recipients.into_iter().collect(),
        burns: burn_amt,
    }
}

fn dispatcher_announce_burn_ops<T: BlockEventDispatcher>(
    dispatcher: &T,
    burn_header: &BurnchainBlockHeader,
    paid_rewards: PaidRewards,
    reward_recipient_info: Option<RewardSetInfo>,
) {
    let recipients = if let Some(recip_info) = reward_recipient_info {
        recip_info
            .recipients
            .into_iter()
            .map(|(addr, ..)| addr)
            .collect()
    } else {
        vec![]
    };

    dispatcher.announce_burn_block(
        &burn_header.block_hash,
        burn_header.block_height,
        paid_rewards.pox,
        paid_rewards.burns,
        recipients,
    );
}

fn forget_orphan_stacks_blocks(
    sort_conn: &DBConn,
    chainstate_db_tx: &mut DBTx,
    burn_header: &BurnchainHeaderHash,
    invalidation_height: u64,
) {
    if let Ok(sns) = SortitionDB::get_all_snapshots_for_burn_block(&sort_conn, &burn_header) {
        for sn in sns.into_iter() {
            // only retry blocks that are truly in descendant
            // sortitions.
            if sn.sortition && sn.block_height > invalidation_height {
                if let Err(e) = StacksChainState::forget_orphaned_epoch_data(
                    chainstate_db_tx,
                    &sn.consensus_hash,
                    &sn.winning_stacks_block_hash,
                ) {
                    warn!(
                        "Failed to forget that {}/{} is orphaned: {:?}",
                        &sn.consensus_hash, &sn.winning_stacks_block_hash, &e
                    );
                }
            }
        }
    }
}

impl<
        'a,
        T: BlockEventDispatcher,
        N: CoordinatorNotices,
        U: RewardSetProvider,
        CE: CostEstimator + ?Sized,
        FE: FeeEstimator + ?Sized,
    > ChainsCoordinator<'a, T, N, U, CE, FE>
{
    pub fn handle_new_stacks_block(&mut self) -> Result<Option<BlockHeaderHash>, Error> {
        if let Some(pox_anchor) = self.process_ready_blocks()? {
            self.process_new_pox_anchor(pox_anchor)
        } else {
            Ok(None)
        }
    }

    /// Get all block snapshots and their PoX IDs at a given burnchain block height.
    fn get_snapshots_and_pox_ids_at_height(
        &mut self,
        height: u64,
    ) -> Result<Vec<(BlockSnapshot, PoxId)>, Error> {
        let sort_ids = SortitionDB::get_sortition_ids_at_height(self.sortition_db.conn(), height)?;
        let ic = self.sortition_db.index_conn();

        let mut ret = Vec::with_capacity(sort_ids.len());

        for sort_id in sort_ids.iter() {
            let handle = ic.as_handle(sort_id);

            let sn = SortitionDB::get_block_snapshot(&self.sortition_db.conn(), sort_id)?
                .expect("BUG: have sortition ID without snapshot");

            let pox_id = handle.get_pox_id()?;
            ret.push((sn, pox_id));
        }

        Ok(ret)
    }

    /// Compare the coordinator's heaviest affirmation map to the heaviest affirmation map in the
    /// burnchain DB.  If they are different, then invalidate all sortitions not represented on
    /// the coordinator's heaviest affirmation map that are now represented by the burnchain DB's
    /// heaviest affirmation map.
    ///
    /// Care must be taken to ensure that a sortition that was already created, but invalidated, is
    /// not re-created.  This can happen if the affirmation map flaps, causing a sortition that was
    /// created and invalidated to become valid again.  The code here addresses this by considering
    /// three ranges of sortitions (grouped by reward cycle) when processing a new heaviest
    /// affirmation map:
    ///
    /// * The range of sortitions that are valid in both affirmation maps. These sortitions
    /// correspond to the affirmation maps' common prefix.
    /// * The range of sortitions that exists and are invalid on the coordinator's current
    /// affirmation map, but are valid on the new heaviest affirmation map.  These sortitions
    /// come strictly after the common prefix, and are identified by the variables
    /// `first_invalid_start_block` and `last_invalid_start_block` (which identifies their lowest
    /// and highest block heights).
    /// * The range of sortitions that are currently valid, and need to be invalidated.  This range
    /// comes strictly after the aforementioned previously-invalid-but-now-valid sortition range.
    ///
    /// The code does not modify any sortition state for the common prefix of sortitions.
    ///
    /// The code identifies the second range of previously-invalid-but-now-valid sortitions and marks them
    /// as valid once again.  In addition, it updates the Stacks chainstate DB such that any Stacks
    /// blocks that were orphaned and never processed can be retried with the now-revalidated
    /// sortition.
    ///
    /// The code identifies the third range of now-invalid sortitions and marks them as invalid in
    /// the sortition DB.
    ///
    /// Note that regardless of the affirmation map status, a Stacks block will remain processed
    /// once it gets accepted.  Its underlying sortition may become invalidated, in which case, the
    /// Stacks block would no longer be considered as part of the canonical Stacks fork (since the
    /// canonical Stacks chain tip must reside on a valid sortition).  However, a Stacks block that
    /// should be processed at the end of the day may temporarily be considered orphaned if there
    /// is a "deep" affirmation map reorg that causes at least one reward cycle's sortitions to
    /// be treated as invalid.  This is what necessitates retrying Stacks blocks that have been
    /// downloaded and considered orphaned because they were never processed -- they may in fact be
    /// valid and processable once the node has identified the canonical sortition history!
    ///
    /// The only kinds of errors returned here are database query errors.
    fn handle_affirmation_reorg(&mut self) -> Result<(), Error> {
        let canonical_burnchain_tip = self.burnchain_blocks_db.get_canonical_chain_tip()?;
        let heaviest_am = BurnchainDB::get_heaviest_anchor_block_affirmation_map(
            self.burnchain_blocks_db.conn(),
            &self.burnchain,
        )?;
        debug!(
            "Heaviest anchor block affirmation map is `{}` at height {}, current is {:?}",
            &heaviest_am,
            canonical_burnchain_tip.block_height,
            &self.heaviest_anchor_block_affirmation_map
        );

        // did the canonical affirmation map change?
        if let Some(heaviest_am_before) = self.heaviest_anchor_block_affirmation_map.take() {
            if let Some(changed_reward_cycle) = heaviest_am.find_divergence(&heaviest_am_before) {
                let current_reward_cycle = self
                    .burnchain
                    .block_height_to_reward_cycle(canonical_burnchain_tip.block_height)
                    .unwrap_or(0);
                if changed_reward_cycle < current_reward_cycle {
                    info!("Heaviest anchor block affirmation map changed from {} to {} in reward cycle {}", &heaviest_am_before, &heaviest_am, current_reward_cycle);

                    let affirmation_pox_id = heaviest_am.as_pox_id();
                    test_debug!(
                        "PoxId of new affirmation map {:?} is {}",
                        &heaviest_am,
                        &affirmation_pox_id
                    );

                    // find the lowest reward cycle we have to reprocess (which starts at burn
                    // block rc_start_block).

                    // burn chain height at which we'll invalidate *all* sortitions
                    let mut last_invalidate_start_block = 0;

                    // burn chain height at which we'll re-try orphaned Stacks blocks, and
                    // revalidate the sortitions that were previously invalid but have now been
                    // made valid.
                    let mut first_invalidate_start_block = 0;

                    // set of sortition IDs that are currently invalid, but will need to be reset
                    // as valid
                    let mut valid_sortition_ids = vec![];

                    let mut diverged = false;
                    for rc in changed_reward_cycle..current_reward_cycle {
                        last_invalidate_start_block =
                            self.burnchain.reward_cycle_to_block_height(rc);
                        first_invalidate_start_block = last_invalidate_start_block;

                        // + 1 because the first sortition of a reward cycle is congruent to 1 mod
                        // reward_cycle_length.
                        let sort_ids = SortitionDB::get_sortition_ids_at_height(
                            self.sortition_db.conn(),
                            last_invalidate_start_block + 1,
                        )?;

                        // find the sortition ID with the shortest PoX bitvector that is NOT a prefix
                        // of the canonical affirmation map's PoX bitvector.
                        let mut found_diverged = false;
                        for sort_id in sort_ids.iter() {
                            let ic = self.sortition_db.index_conn();
                            let handle = ic.as_handle(sort_id);

                            let pox_id = handle.get_pox_id()?;
                            test_debug!(
                                "Compare {} as prefix of {}?",
                                &pox_id,
                                &affirmation_pox_id
                            );
                            if affirmation_pox_id.has_prefix(&pox_id) {
                                continue;
                            }

                            // pox_id is NOT a prefix of affirmation_pox_id, but maybe it's only
                            // different by the last bit?
                            let prior_affirmation_pox_id = PoxId::new(
                                affirmation_pox_id.clone().into_inner()
                                    [0..(affirmation_pox_id.len().saturating_sub(1))]
                                    .to_vec(),
                            );
                            let prior_pox_id = PoxId::new(
                                pox_id.clone().into_inner()[0..(pox_id.len().saturating_sub(1))]
                                    .to_vec(),
                            );

                            if prior_affirmation_pox_id.has_prefix(&prior_pox_id) {
                                // this is the first reward cycle where history diverged.
                                found_diverged = true;
                                test_debug!("{} diverges from {}", &pox_id, affirmation_pox_id);

                                // careful -- we might have already procesed sortitions in this
                                // reward cycle with this PoX ID, but that were never confirmed
                                // by a subsequent prepare phase.
                                let start_height = last_invalidate_start_block;
                                let end_height = canonical_burnchain_tip.block_height;
                                for height in start_height..end_height {
                                    let snapshots_and_pox_ids =
                                        self.get_snapshots_and_pox_ids_at_height(height)?;
                                    let num_sns = snapshots_and_pox_ids.len();
                                    test_debug!("{} snapshots at {}", num_sns, height);

                                    let mut found = false;
                                    for (sn, sn_pox_id) in snapshots_and_pox_ids.into_iter() {
                                        test_debug!(
                                            "Snapshot {} height {} has PoX ID {} (is prefix of {}?)",
                                            &sn.sortition_id,
                                            sn.block_height,
                                            &sn_pox_id,
                                            &affirmation_pox_id,
                                        );
                                        if affirmation_pox_id.has_prefix(&sn_pox_id) {
                                            // have already processed this sortitoin
                                            test_debug!("Already processed sortition {} at height {} with PoX ID {} on canonical affirmation map {}", &sn.sortition_id, sn.block_height, &sn_pox_id, &heaviest_am);
                                            found = true;
                                            last_invalidate_start_block = height;
                                            valid_sortition_ids.push(sn.sortition_id);
                                            break;
                                        }
                                    }
                                    if !found && num_sns > 0 {
                                        // there are snapshots, and they're all diverged
                                        debug!("No snapshot at height {} has a PoX ID that is a prefix of {} (affirmation map {})", height, &affirmation_pox_id, &heaviest_am);
                                        break;
                                    }
                                }
                                break;
                            }
                        }

                        if !found_diverged {
                            continue;
                        }

                        // we may have processed some sortitions correctly within this reward
                        // cycle. Advance forward until we find one that we haven't.
                        info!(
                            "Re-playing sortitions starting within reward cycle {} burn height {}",
                            rc, last_invalidate_start_block
                        );

                        diverged = true;
                        break;
                    }

                    if diverged {
                        // find our ancestral sortition ID that's the end of the last reward cycle
                        // the new affirmation map would have in common with the old affirmation
                        // map, and invalidate its descendants
                        let ic = self.sortition_db.index_conn();
                        let sortition_id = self.canonical_sortition_tip.as_ref().expect(
                            "FAIL: processing an affirmation reorg, but don't have a canonical sortition tip",
                        );

                        // first snapshot in which we'll invalidate all descendant snapshots, but retain some previously-invalidated snapshots
                        let revalidate_sn = SortitionDB::get_ancestor_snapshot(
                            &ic,
                            first_invalidate_start_block - 1,
                            &sortition_id,
                        )?
                        .expect(&format!(
                            "BUG: no ancestral sortition at height {}",
                            first_invalidate_start_block - 1
                        ));

                        // first snapshot at which we'll invalidate all descendant snapshots
                        let invalidate_sn = SortitionDB::get_ancestor_snapshot(
                            &ic,
                            last_invalidate_start_block - 1,
                            &sortition_id,
                        )?
                        .expect(&format!(
                            "BUG: no ancestral sortition at height {}",
                            last_invalidate_start_block - 1
                        ));

                        let invalidation_height = revalidate_sn.block_height;
                        let mut chainstate_db_tx = self.chain_state_db.db_tx_begin()?;

                        debug!("Invalidate all descendants of {} (after height {} sortition {}), revalidate some sortitions at and after height {}, and retry all orphaned Stacks blocks at or after height {}",
                               &revalidate_sn.burn_header_hash, revalidate_sn.block_height, &revalidate_sn.sortition_id, invalidate_sn.block_height, first_invalidate_start_block);

                        self.sortition_db.invalidate_descendants_with_closure(
                            &revalidate_sn.burn_header_hash,
                            |sort_tx, burn_header, invalidate_queue| {
                                // do this once in the transaction, after we've invalidated all other
                                // sibling blocks to these now-valid sortitions
                                test_debug!(
                                    "Invalidate all sortitions for {} ({} remaining)",
                                    &burn_header,
                                    invalidate_queue.len()
                                );
                                if invalidate_queue.len() == 0 {
                                    // last time this method will be called
                                    for valid_sn in valid_sortition_ids.iter() {
                                        test_debug!("Revalidate snapshot {}", valid_sn);
                                        SortitionDB::revalidate_snapshot(sort_tx, valid_sn).expect(
                                            &format!(
                                                "FATAL: failed to revalidate sortition {}",
                                                valid_sn
                                            ),
                                        );
                                    }
                                }

                                // permit re-processing of any associated stacks blocks if they're
                                // orphaned
                                forget_orphan_stacks_blocks(
                                    sort_tx,
                                    &mut chainstate_db_tx,
                                    burn_header,
                                    invalidation_height,
                                );
                            },
                        )?;

                        for burn_height in
                            first_invalidate_start_block..(last_invalidate_start_block + 1)
                        {
                            // retry this orphan
                            let ic = self.sortition_db.index_conn();
                            let handle = ic.as_handle(&sortition_id);
                            let sn = handle
                                .get_block_snapshot_by_height(burn_height)?
                                .expect("BUG: no ancestral snapshot");

                            forget_orphan_stacks_blocks(
                                &self.sortition_db.conn(),
                                &mut chainstate_db_tx,
                                &sn.burn_header_hash,
                                burn_height.saturating_sub(1),
                            );
                        }

                        // re-process the anchor block state for this reward cycle
                        let pox_id = affirmation_pox_id;

                        let highest_valid_sortition_id = valid_sortition_ids
                            .last()
                            .unwrap_or(&invalidate_sn.sortition_id)
                            .to_owned();
                        let highest_valid_snapshot = SortitionDB::get_block_snapshot(
                            &self.sortition_db.conn(),
                            &highest_valid_sortition_id,
                        )?
                        .expect(&format!(
                            "BUG: no such sortition {}",
                            &highest_valid_sortition_id
                        ));

                        let (canonical_ch, canonical_bhh) =
                            SortitionDB::get_canonical_stacks_chain_tip_hash(
                                &self.sortition_db.conn(),
                            )?;

                        debug!(
                            "Highest valid sortition is {} ({} in height {}); Stacks tip is {}/{}",
                            &highest_valid_snapshot.sortition_id,
                            &highest_valid_snapshot.burn_header_hash,
                            highest_valid_snapshot.block_height,
                            &canonical_ch,
                            &canonical_bhh
                        );

                        // by holding this lock as long as we do, we ensure that the sortition DB's
                        // view of the canonical stacks chain tip can't get changed (since no
                        // Stacks blocks can be processed).
                        chainstate_db_tx
                            .commit()
                            .map_err(|e| DBError::SqliteError(e))?;

                        self.canonical_chain_tip =
                            Some(StacksBlockId::new(&canonical_ch, &canonical_bhh));

                        self.canonical_sortition_tip = Some(highest_valid_snapshot.sortition_id);
                        self.canonical_pox_id = Some(pox_id);
                        self.heaviest_anchor_block_affirmation_map = Some(heaviest_am);
                    }
                } else {
                    self.heaviest_anchor_block_affirmation_map = Some(heaviest_am);
                }
            } else {
                self.heaviest_anchor_block_affirmation_map = Some(heaviest_am);
            }
        } else {
            self.heaviest_anchor_block_affirmation_map = Some(heaviest_am);
        }

        Ok(())
    }

    /// Use the network's affirmations to re-interpret our local PoX anchor block status into what
    /// the network affirmed was their PoX anchor block statuses.
    /// If we're blocked on receiving a new anchor block that we don't have (i.e. the network
    /// affirmed that it exists), then indicate so by returning its hash.
    fn reinterpret_affirmed_pox_anchor_block_status(
        &mut self,
        canonical_affirmation_map: &AffirmationMap,
        header: &BurnchainBlockHeader,
        rc_info: &mut RewardCycleInfo,
    ) -> Result<Option<BlockHeaderHash>, Error> {
        // re-calculate the reward cycle info's anchor block status, based on what
        // the network has affirmed in each prepare phase.

        // is this anchor block affirmed?  Only process it if so!
        let new_reward_cycle = self
            .burnchain
            .block_height_to_reward_cycle(header.block_height)
            .expect("BUG: processed block before start of epoch 2.1");

        test_debug!(
            "Verify affirmation against PoX info in reward cycle {} canonical affirmation map {}",
            new_reward_cycle,
            &canonical_affirmation_map
        );

        let new_status = if new_reward_cycle > 0
            && new_reward_cycle <= (canonical_affirmation_map.len() as u64)
        {
            // we're processing an anchor block from an earlier reward cycle,
            // meaning that we're in the middle of an affirmation reorg.
            let affirmation = canonical_affirmation_map
                .at(new_reward_cycle - 1)
                .expect("BUG: checked index overflow")
                .to_owned();
            test_debug!("Affirmation '{}' for anchor block of previous reward cycle {} canonical affirmation map {}", &affirmation, new_reward_cycle - 1, &canonical_affirmation_map);

            // switch reward cycle info assessment based on what the network
            // affirmed.
            match &rc_info.anchor_status {
                PoxAnchorBlockStatus::SelectedAndKnown(block_hash, reward_set) => {
                    match affirmation {
                        AffirmationMapEntry::PoxAnchorBlockPresent => {
                            // matches affirmation
                            PoxAnchorBlockStatus::SelectedAndKnown(
                                block_hash.clone(),
                                reward_set.clone(),
                            )
                        }
                        AffirmationMapEntry::PoxAnchorBlockAbsent => {
                            // network actually affirms that this anchor block
                            // is absent.
                            warn!("Chose PoX anchor block for reward cycle {}, but it is affirmed absent by the network", new_reward_cycle - 1; "affirmation map" => %&canonical_affirmation_map);
                            PoxAnchorBlockStatus::SelectedAndUnknown(block_hash.clone())
                        }
                        AffirmationMapEntry::Nothing => {
                            // no anchor block selected either way
                            PoxAnchorBlockStatus::NotSelected
                        }
                    }
                }
                PoxAnchorBlockStatus::SelectedAndUnknown(ref block_hash) => {
                    match affirmation {
                        AffirmationMapEntry::PoxAnchorBlockPresent => {
                            // the network affirms that this anchor block
                            // exists, but we don't have it locally.  Stop
                            // processing here and wait for it to arrive, via
                            // the downloader.
                            info!("Anchor block {} for reward cycle {} is affirmed by the network ({}), but must be downloaded", block_hash, canonical_affirmation_map, new_reward_cycle - 1);
                            return Ok(Some(block_hash.clone()));
                        }
                        AffirmationMapEntry::PoxAnchorBlockAbsent => {
                            // matches affirmation
                            PoxAnchorBlockStatus::SelectedAndUnknown(block_hash.clone())
                        }
                        AffirmationMapEntry::Nothing => {
                            // no anchor block selected either way
                            PoxAnchorBlockStatus::NotSelected
                        }
                    }
                }
                PoxAnchorBlockStatus::NotSelected => {
                    // no anchor block selected either way
                    PoxAnchorBlockStatus::NotSelected
                }
            }
        } else {
            // no-op: our view of the set of anchor blocks is consistent with
            // the canonical affirmation map, so the status of this new anchor
            // block is whatever it was calculated to be.
            rc_info.anchor_status.clone()
        };

        // update new status
        debug!(
            "Update anchor block status for reawrd cycle {} from {:?} to {:?}",
            new_reward_cycle, &rc_info.anchor_status, &new_status
        );
        rc_info.anchor_status = new_status;
        Ok(None)
    }

    /// Determine if we have the block data for a given block-commit.
    /// Used to see if we have the block data for an unaffirmed PoX anchor block
    /// (hence the test_debug! macros referring to PoX anchor blocks)
    fn has_stacks_block_for(&self, block_commit: LeaderBlockCommitOp) -> bool {
        let tip = SortitionDB::get_canonical_burn_chain_tip(self.sortition_db.conn())
            .expect("BUG: failed to query chain tip from sortition DB");
        let ic = self.sortition_db.index_conn();
        if let Some(sn) = SortitionDB::get_block_snapshot_for_winning_stacks_block(
            &ic,
            &tip.sortition_id,
            &block_commit.block_header_hash,
        )
        .expect("BUG: failed to query sortition DB")
        {
            // it exists on this sortition history, but do we have it in the chainstate?
            let present = StacksChainState::has_stacks_block(
                &self.chain_state_db.db(),
                &StacksBlockHeader::make_index_block_hash(
                    &sn.consensus_hash,
                    &block_commit.block_header_hash,
                ),
            )
            .expect("BUG: failed to query chainstate DB");
            if present {
                test_debug!(
                    "Have processed unaffirmed PoX anchor block {}/{} (burn height {})",
                    &sn.consensus_hash,
                    &block_commit.block_header_hash,
                    sn.block_height
                );
                present
            } else {
                // have we instead maybe downloaded it but not processed it yet?
                // NOTE: if the anchor block is unprocessable, it will eventually get orphaned
                test_debug!(
                    "Have NOT processed unaffirmed PoX anchor block {}/{} (burn height {})",
                    &sn.consensus_hash,
                    &block_commit.block_header_hash,
                    sn.block_height
                );
                let has_staging = StacksChainState::has_staging_block(
                    &self.chain_state_db.db(),
                    &sn.consensus_hash,
                    &block_commit.block_header_hash,
                )
                .expect("BUG: failed to query chainstate DB");
                if has_staging {
                    test_debug!(
                        "Have unprocessed staging PoX anchor block {}/{} (burn height {})",
                        &sn.consensus_hash,
                        &block_commit.block_header_hash,
                        sn.block_height
                    );
                    true
                } else {
                    test_debug!(
                        "Do NOT have unprocessed staging PoX anchor block {}/{} (burn height {})",
                        &sn.consensus_hash,
                        &block_commit.block_header_hash,
                        sn.block_height
                    );
                    false
                }
            }
        } else {
            test_debug!(
                "No block snapshot for PoX anchor block {} off of sortition {}",
                &block_commit.block_header_hash,
                &tip.sortition_id
            );
            return false;
        }
    }

    pub fn get_canonical_affirmation_map(&self) -> Result<AffirmationMap, Error> {
        BurnchainDB::get_canonical_affirmation_map(
            self.burnchain_blocks_db.conn(),
            &self.burnchain,
            |anchor_block_commit, _anchor_block_metadata| {
                // if we don't have an unaffirmed anchor block, and we're no longer in the initial block
                // download, then assume that it's absent.  Otherwise, if we are in the initial block
                // download but we don't have it yet, assume that it's present.
                self.has_stacks_block_for(anchor_block_commit)
            },
        )
        .map_err(|e| e.into())
    }

    pub fn handle_new_burnchain_block(&mut self) -> Result<Option<BlockHeaderHash>, Error> {
        self.inner_handle_new_burnchain_block(false)
    }

    pub fn retry_burnchain_block(&mut self) -> Result<Option<BlockHeaderHash>, Error> {
        self.inner_handle_new_burnchain_block(true)
    }

    /// Handle a new burnchain block, optionally rolling back the canonical PoX sortition history
    /// and setting it up to be replayed in the event the network affirms a different history.  If
    /// this happens, *and* if re-processing the new affirmed history is *blocked on* the
    /// unavailability of a PoX anchor block that *must now* exist, then return the hash of this
    /// anchor block.
    ///
    /// `retry` just controls log verbosity
    fn inner_handle_new_burnchain_block(
        &mut self,
        retry: bool,
    ) -> Result<Option<BlockHeaderHash>, Error> {
        // first, see if the canonical affirmation map has changed.  If so, this will wind back the
        // canonical sortition and stacks chain tips.
        self.handle_affirmation_reorg()?;

        // Retrieve canonical burnchain chain tip from the BurnchainBlocksDB
        let canonical_burnchain_tip = self.burnchain_blocks_db.get_canonical_chain_tip()?;
        let canonical_affirmation_map = self.get_canonical_affirmation_map()?;

        if !retry {
            debug!("Handle new canonical burnchain tip";
                   "height" => %canonical_burnchain_tip.block_height,
                   "block_hash" => %canonical_burnchain_tip.block_hash.to_string());
        }

        // Retrieve all the direct ancestors of this block with an unprocessed sortition
        let mut cursor = canonical_burnchain_tip.block_hash.clone();
        let mut sortitions_to_process = VecDeque::new();

        // We halt the ancestry research as soon as we find a processed parent
        let mut last_processed_ancestor = loop {
            if let Some(found_sortition) = self.sortition_db.is_sortition_processed(&cursor)? {
                debug!(
                    "Ancestor sortition {} of block {} is processed",
                    &found_sortition, &cursor
                );
                break found_sortition;
            }

            let current_block =
                BurnchainDB::get_burnchain_block(&self.burnchain_blocks_db.conn(), &cursor)
                    .map_err(|e| {
                        warn!(
                            "ChainsCoordinator: could not retrieve  block burnhash={}",
                            &cursor
                        );
                        Error::NonContiguousBurnchainBlock(e)
                    })?;

            let parent = current_block.header.parent_block_hash.clone();
            sortitions_to_process.push_front(current_block);
            cursor = parent;
        };

        let burn_header_hashes: Vec<_> = sortitions_to_process
            .iter()
            .map(|block| block.header.block_hash.to_string())
            .collect();

        debug!(
            "Unprocessed burn chain blocks [{}]",
            burn_header_hashes.join(", ")
        );

        for unprocessed_block in sortitions_to_process.into_iter() {
            let BurnchainBlockData { header, ops } = unprocessed_block;

            let _reward_cycle = self
                .burnchain
                .block_height_to_reward_cycle(header.block_height)
                .unwrap_or(u64::MAX);
            test_debug!(
                "Process burn block {} reward cycle {} in {}",
                header.block_height,
                _reward_cycle,
                &self.burnchain.working_dir
            );

            // calculate paid rewards during this burnchain block if we announce
            //  to an events dispatcher
            let paid_rewards = if self.dispatcher.is_some() {
                calculate_paid_rewards(&ops)
            } else {
                PaidRewards {
                    pox: vec![],
                    burns: 0,
                }
            };

            // at this point, we need to figure out if the sortition we are
            //  about to process is the first block in reward cycle.
            let mut reward_cycle_info = self.get_reward_cycle_info(&header)?;

            if let Some(rc_info) = reward_cycle_info.as_mut() {
                let cur_epoch =
                    SortitionDB::get_stacks_epoch(self.sortition_db.conn(), header.block_height)?
                        .expect(&format!(
                            "BUG: no epoch defined at height {}",
                            header.block_height
                        ));

                if cur_epoch.epoch_id >= StacksEpochId::Epoch21
                    || self.config.always_use_affirmation_maps
                {
                    // potentially have an anchor block, but only process the next reward cycle (and
                    // subsequent reward cycles) with it if the prepare-phase block-commits affirm its
                    // presence.  This only gets checked in Stacks 2.1 or later (unless overridden
                    // in the config)

                    // NOTE: this mutates rc_info
                    if let Some(missing_anchor_block) = self
                        .reinterpret_affirmed_pox_anchor_block_status(
                            &canonical_affirmation_map,
                            &header,
                            rc_info,
                        )?
                    {
                        // missing this anchor block -- cannot proceed until we have it
                        if !retry {
                            info!("Burnchain block processing stops due to missing affirmed anchor block {}", &missing_anchor_block);
                        }
                        return Ok(Some(missing_anchor_block));
                    }
                }

                test_debug!(
                    "Reward cycle info at height {}: {:?}",
                    &header.block_height,
                    &rc_info
                );
            }

            // bind a reference here to avoid tripping up the borrow-checker
            let dispatcher_ref = &self.dispatcher;
            let (next_snapshot, _) = self
                .sortition_db
                .evaluate_sortition(
                    &header,
                    ops,
                    &self.burnchain,
                    &last_processed_ancestor,
                    reward_cycle_info,
                    |reward_set_info| {
                        if let Some(dispatcher) = dispatcher_ref {
                            dispatcher_announce_burn_ops(
                                *dispatcher,
                                &header,
                                paid_rewards,
                                reward_set_info,
                            );
                        }
                    },
                )
                .map_err(|e| {
                    error!("ChainsCoordinator: unable to evaluate sortition {:?}", e);
                    Error::FailedToProcessSortition(e)
                })?;

            let sortition_id = next_snapshot.sortition_id;

            self.notifier.notify_sortition_processed();

            debug!(
                "Sortition processed";
                "sortition_id" => &sortition_id.to_string(),
                "burn_header_hash" => &next_snapshot.burn_header_hash.to_string(),
                "burn_height" => next_snapshot.block_height
            );

            // we may already have the associated Stacks block, but linked to a different sortition
            // history.  For example, if an anchor block was selected but PoX was voted disabled or
            // not voted to activate, then the same Stacks blocks could be chosen but with
            // different consensus hashes.  So, check here if we happen to already have the block
            // stored, and proceed to put it into staging again.
            if next_snapshot.sortition {
                let staging_block_chs = StacksChainState::get_staging_block_consensus_hashes(
                    self.chain_state_db.db(),
                    &next_snapshot.winning_stacks_block_hash,
                )?;

                let mut found = false;
                for ch in staging_block_chs.iter() {
                    if *ch == next_snapshot.consensus_hash {
                        found = true;
                        break;
                    }
                }

                if !found && staging_block_chs.len() > 0 {
                    // we have seen this block before, but in a different consensus fork.
                    // queue it for re-processing -- it might still be valid if it's in a reward
                    // cycle that exists on the new PoX fork.
                    debug!("Sortition re-processes Stacks block {}, which is present on a different PoX fork", &next_snapshot.winning_stacks_block_hash);

                    self.replay_stacks_blocks(vec![next_snapshot
                        .winning_stacks_block_hash
                        .clone()])?;
                }
            }

            // always bump canonical sortition tip:
            //   if this code path is invoked, the canonical burnchain tip
            //   has moved, so we should move our canonical sortition tip as well.
            self.canonical_sortition_tip = Some(sortition_id.clone());
            last_processed_ancestor = sortition_id;

            if let Some(pox_anchor) = self.process_ready_blocks()? {
                if let Some(expected_anchor_block_hash) = self.process_new_pox_anchor(pox_anchor)? {
                    info!(
                        "Burnchain block processing stops due to missing affirmed anchor block {}",
                        &expected_anchor_block_hash
                    );
                    return Ok(Some(expected_anchor_block_hash));
                }
            }
        }

        Ok(None)
    }

    /// returns None if this burnchain block is _not_ the start of a reward cycle
    ///         otherwise, returns the required reward cycle info for this burnchain block
    ///                     in our current sortition view:
    ///           * PoX anchor block
    ///           * Was PoX anchor block known?
    pub fn get_reward_cycle_info(
        &mut self,
        burn_header: &BurnchainBlockHeader,
    ) -> Result<Option<RewardCycleInfo>, Error> {
        let sortition_tip_id = self
            .canonical_sortition_tip
            .as_ref()
            .expect("FATAL: Processing anchor block, but no known sortition tip");

        get_reward_cycle_info(
            burn_header.block_height,
            &burn_header.parent_block_hash,
            sortition_tip_id,
            &self.burnchain,
            &mut self.chain_state_db,
            &self.sortition_db,
            &self.reward_set_provider,
        )
    }

    /// Process any Atlas attachment events and forward them to the Atlas subsystem
    fn process_atlas_attachment_events(
        &self,
        block_receipt: &StacksEpochReceipt,
        canonical_stacks_tip_height: u64,
    ) {
        let mut attachments_instances = HashSet::new();
        for receipt in block_receipt.tx_receipts.iter() {
            if let TransactionOrigin::Stacks(ref transaction) = receipt.transaction {
                if let TransactionPayload::ContractCall(ref contract_call) = transaction.payload {
                    let contract_id = contract_call.to_clarity_contract_id();
                    increment_contract_calls_processed();
                    if self.atlas_config.contracts.contains(&contract_id) {
                        for event in receipt.events.iter() {
                            if let StacksTransactionEvent::SmartContractEvent(ref event_data) =
                                event
                            {
                                let res = AttachmentInstance::try_new_from_value(
                                    &event_data.value,
                                    &contract_id,
                                    block_receipt.header.index_block_hash(),
                                    block_receipt.header.stacks_block_height,
                                    receipt.transaction.txid(),
                                    Some(canonical_stacks_tip_height),
                                );
                                if let Some(attachment_instance) = res {
                                    attachments_instances.insert(attachment_instance);
                                }
                            }
                        }
                    }
                }
            }
        }
        if !attachments_instances.is_empty() {
            info!(
                "Atlas: {} attachment instances emitted from events",
                attachments_instances.len()
            );
            match self.attachments_tx.send(attachments_instances) {
                Ok(_) => {}
                Err(e) => {
                    error!("Atlas: error dispatching attachments {}", e);
                }
            };
        }
    }

    /// Replay any existing Stacks blocks we have that arose on a different PoX fork.
    /// This is best-effort -- if a block isn't found or can't be loaded, it's skipped.
    pub fn replay_stacks_blocks(&mut self, blocks: Vec<BlockHeaderHash>) -> Result<(), Error> {
        let tip = SortitionDB::get_canonical_burn_chain_tip(self.sortition_db.conn())?;
        for bhh in blocks.into_iter() {
            let staging_block_chs = StacksChainState::get_staging_block_consensus_hashes(
                self.chain_state_db.db(),
                &bhh,
            )?;
            let mut processed = false;

            debug!("Consider replaying {} from {:?}", &bhh, &staging_block_chs);

            for alt_ch in staging_block_chs.into_iter() {
                let alt_id = StacksBlockHeader::make_index_block_hash(&alt_ch, &bhh);
                if !StacksChainState::has_block_indexed(&self.chain_state_db.blocks_path, &alt_id)
                    .unwrap_or(false)
                {
                    continue;
                }

                // does this consensus hash exist somewhere? Doesn't have to be on the canonical
                // PoX fork.
                let ch_height_opt = self.sortition_db.get_consensus_hash_height(&alt_ch)?;
                let ch_height = if let Some(ch_height) = ch_height_opt {
                    ch_height
                } else {
                    continue;
                };

                // Find the corresponding snapshot on the canonical PoX fork.
                let ancestor_sn = if let Some(sn) = SortitionDB::get_ancestor_snapshot(
                    &self.sortition_db.index_conn(),
                    ch_height,
                    &tip.sortition_id,
                )? {
                    sn
                } else {
                    continue;
                };

                // the new consensus hash
                let ch = ancestor_sn.consensus_hash;

                if let Ok(Some(block)) =
                    StacksChainState::load_block(&self.chain_state_db.blocks_path, &alt_ch, &bhh)
                {
                    let ic = self.sortition_db.index_conn();
                    if let Some(parent_snapshot) = ic
                        .find_parent_snapshot_for_stacks_block(&ch, &bhh)
                        .unwrap_or(None)
                    {
                        // replay in this consensus hash history
                        debug!("Replay Stacks block from {} to {}/{}", &alt_ch, &ch, &bhh);
                        let _ = self.chain_state_db.preprocess_anchored_block(
                            &self.sortition_db.index_conn(),
                            &ch,
                            &block,
                            &parent_snapshot.consensus_hash,
                            get_epoch_time_secs(),
                        );
                        processed = true;
                        break;
                    }
                }
            }

            if !processed {
                test_debug!("Did NOT replay {}", &bhh);
            }
        }
        Ok(())
    }

    /// Verify that a PoX anchor block candidate is affirmed by the network.
    /// Returns Ok(Some(pox_anchor)) if so.
    /// Returns Ok(None) if not.
    /// Returns Err(Error::NotPoXAnchorBlock) if this block got F*w confirmations but is not the
    /// heaviest-confirmed burnchain block.
    fn check_pox_anchor_affirmation(
        &mut self,
        pox_anchor: BlockHeaderHash,
        winner_snapshot: &BlockSnapshot,
    ) -> Result<Option<BlockHeaderHash>, Error> {
        if BurnchainDB::is_anchor_block(
            self.burnchain_blocks_db.conn(),
            &winner_snapshot.burn_header_hash,
            &winner_snapshot.winning_block_txid,
        )? {
            // affirmed?
            let canonical_am = self.get_canonical_affirmation_map()?;

            let commit = BurnchainDB::get_block_commit(
                self.burnchain_blocks_db.conn(),
                &winner_snapshot.winning_block_txid,
            )?
            .expect("BUG: no commit metadata in DB for existing commit");

            let reward_cycle = self
                .burnchain
                .block_height_to_reward_cycle(commit.block_height)
                .expect(
                    "BUG: accepted block commit has a block height before the first reward cycle",
                );

            if canonical_am
                .at(reward_cycle)
                .unwrap_or(&AffirmationMapEntry::PoxAnchorBlockAbsent)
                == &AffirmationMapEntry::PoxAnchorBlockPresent
            {
                // yup, we're expecting this
                info!("Discovered an old anchor block: {}", &pox_anchor);
                return Ok(Some(pox_anchor));
            } else {
                // nope -- can ignore
                debug!("Discovered unaffirmed old anchor block: {}", &pox_anchor);
                return Ok(None);
            }
        } else {
            debug!("Stacks block {} received F*w confirmations but is not the heaviest-confirmed burnchain block, so treating as non-anchor block", &pox_anchor);
            return Err(Error::NotPoXAnchorBlock);
        }
    }

    ///
    /// Process any ready staging blocks until there are either:
    ///   * there are no more to process
    ///   * a PoX anchor block is processed which invalidates the current PoX fork
    ///
    /// Returns Some(BlockHeaderHash) if such an anchor block is discovered,
    ///   otherwise returns None
    ///
    fn process_ready_blocks(&mut self) -> Result<Option<BlockHeaderHash>, Error> {
        let canonical_sortition_tip = self.canonical_sortition_tip.clone().expect(
            "FAIL: processing a new Stacks block, but don't have a canonical sortition tip",
        );

        let sortdb_handle = self
            .sortition_db
            .tx_handle_begin(&canonical_sortition_tip)?;
        let mut processed_blocks =
            self.chain_state_db
                .process_blocks(sortdb_handle, 1, self.dispatcher)?;

        while let Some(block_result) = processed_blocks.pop() {
            if let (Some(block_receipt), _) = block_result {
                // only bump the coordinator's state if the processed block
                //   is in our sortition fork
                //  TODO: we should update the staging block logic to prevent
                //    blocks like these from getting processed at all.
                let in_sortition_set = self.sortition_db.is_stacks_block_in_sortition_set(
                    &canonical_sortition_tip,
                    &block_receipt.header.anchored_header.block_hash(),
                )?;
                if in_sortition_set {
                    let new_canonical_block_snapshot = SortitionDB::get_block_snapshot(
                        self.sortition_db.conn(),
                        &canonical_sortition_tip,
                    )?
                    .expect(&format!(
                        "FAIL: could not find data for the canonical sortition {}",
                        &canonical_sortition_tip
                    ));
                    let new_canonical_stacks_block =
                        new_canonical_block_snapshot.get_canonical_stacks_block_id();
                    self.canonical_chain_tip = Some(new_canonical_stacks_block);
                    debug!("Bump blocks processed");
                    self.notifier.notify_stacks_block_processed();
                    increment_stx_blocks_processed_counter();

                    self.process_atlas_attachment_events(
                        &block_receipt,
                        new_canonical_block_snapshot.canonical_stacks_tip_height,
                    );

                    let block_hash = block_receipt.header.anchored_header.block_hash();
                    let winner_snapshot = SortitionDB::get_block_snapshot_for_winning_stacks_block(
                        &self.sortition_db.index_conn(),
                        &canonical_sortition_tip,
                        &block_hash,
                    )
                    .expect("FAIL: could not find block snapshot for winning block hash")
                    .expect("FAIL: could not find block snapshot for winning block hash");

                    // update cost estimator
                    if let Some(ref mut estimator) = self.cost_estimator {
                        let stacks_epoch = self
                            .sortition_db
                            .index_conn()
                            .get_stacks_epoch_by_epoch_id(&block_receipt.evaluated_epoch)
                            .expect("Could not find a stacks epoch.");
                        estimator.notify_block(
                            &block_receipt.tx_receipts,
                            &stacks_epoch.block_limit,
                            &stacks_epoch.epoch_id,
                        );
                    }

                    // update fee estimator
                    if let Some(ref mut estimator) = self.fee_estimator {
                        let stacks_epoch = self
                            .sortition_db
                            .index_conn()
                            .get_stacks_epoch_by_epoch_id(&block_receipt.evaluated_epoch)
                            .expect("Could not find a stacks epoch.");
                        if let Err(e) =
                            estimator.notify_block(&block_receipt, &stacks_epoch.block_limit)
                        {
                            warn!("FeeEstimator failed to process block receipt";
                                  "stacks_block" => %block_hash,
                                  "stacks_height" => %block_receipt.header.stacks_block_height,
                                  "error" => %e);
                        }
                    }

                    // Was this block sufficiently confirmed by the prepare phase that it was a PoX
                    // anchor block?  And if we're in epoch 2.1, does it match the heaviest-confirmed
                    // block-commit in the burnchain DB, and is it affirmed by the majority of the
                    // network?
                    if let Some(pox_anchor) = self
                        .sortition_db
                        .is_stacks_block_pox_anchor(&block_hash, &canonical_sortition_tip)?
                    {
                        // what epoch is this block in?
                        let cur_epoch = SortitionDB::get_stacks_epoch(
                            self.sortition_db.conn(),
                            winner_snapshot.block_height,
                        )?
                        .expect(&format!(
                            "BUG: no epoch defined at height {}",
                            winner_snapshot.block_height
                        ));

                        match cur_epoch.epoch_id {
                            StacksEpochId::Epoch10 => {
                                panic!("BUG: Snapshot predates Stacks 2.0");
                            }
                            StacksEpochId::Epoch20 | StacksEpochId::Epoch2_05 => {
                                if self.config.always_use_affirmation_maps {
                                    // use affirmation maps even if they're not supported yet.
                                    // if the chain is healthy, this won't cause a chain split.
                                    match self
                                        .check_pox_anchor_affirmation(pox_anchor, &winner_snapshot)
                                    {
                                        Ok(Some(pox_anchor)) => {
                                            // yup, affirmed. unwind the sortition history at this
                                            // block
                                            return Ok(Some(pox_anchor));
                                        }
                                        Ok(None) => {
                                            // unaffirmed old anchor block, so no rewind is needed.
                                            return Ok(None);
                                        }
                                        Err(Error::NotPoXAnchorBlock) => {
                                            panic!("FATAL: found Stacks block that 2.0/2.05 rules would treat as an anchor block, but that 2.1+ would not");
                                        }
                                        Err(e) => {
                                            error!("Failed to check PoX affirmation: {:?}", &e);
                                            return Err(e);
                                        }
                                    }
                                } else {
                                    // 2.0/2.05 behavior: only consult the sortition DB
                                    // if, just after processing the block, we _know_ that this block is a pox anchor, that means
                                    //   that sortitions have already begun processing that didn't know about this pox anchor.
                                    //   we need to trigger an unwind
                                    info!("Discovered an old anchor block: {}", &pox_anchor);
                                    return Ok(Some(pox_anchor));
                                }
                            }
                            StacksEpochId::Epoch21 => {
                                // 2.1 behavior: the anchor block must also be the
                                // heaviest-confirmed anchor block by BTC weight, and the highest
                                // such anchor block if there are multiple contenders.
                                match self
                                    .check_pox_anchor_affirmation(pox_anchor, &winner_snapshot)
                                {
                                    Ok(Some(pox_anchor)) => {
                                        // yup, affirmed. unwind the sortition history at this
                                        // block
                                        return Ok(Some(pox_anchor));
                                    }
                                    Ok(None) => {
                                        // unaffirmed old anchor block, so no rewind is needed.
                                        return Ok(None);
                                    }
                                    Err(Error::NotPoXAnchorBlock) => {
                                        // keep going -- this actually isn't an anchor block
                                    }
                                    Err(e) => {
                                        error!("Failed to check PoX affirmation: {:?}", &e);
                                        return Err(e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // TODO: do something with a poison result

            let sortdb_handle = self
                .sortition_db
                .tx_handle_begin(&canonical_sortition_tip)?;
            // Right before a block is set to processed, the event dispatcher will emit a new block event
            processed_blocks =
                self.chain_state_db
                    .process_blocks(sortdb_handle, 1, self.dispatcher)?;
        }

        Ok(None)
    }

    /// Process a new PoX anchor block, possibly resulting in the PoX history being unwound and
    /// replayed through a different sequence of consensus hashes.  If the new anchor block causes
    /// the node to reach a prepare-phase that elects a network-affirmed anchor block that we don't
    /// have, then return its block hash so the caller can go download and process it.
    fn process_new_pox_anchor(
        &mut self,
        block_id: BlockHeaderHash,
    ) -> Result<Option<BlockHeaderHash>, Error> {
        // get the last sortition in the prepare phase that chose this anchor block
        //   that sortition is now the current canonical sortition,
        //   and now that we have process the anchor block for the corresponding reward phase,
        //   update the canonical pox bitvector.
        let sortition_id = self.canonical_sortition_tip.as_ref().expect(
            "FAIL: processing a new anchor block, but don't have a canonical sortition tip",
        );

        let mut prep_end = self
            .sortition_db
            .get_prepare_end_for(sortition_id, &block_id)?
            .expect(&format!(
                "FAIL: expected to get a sortition for a chosen anchor block {}, but not found.",
                &block_id
            ));

        // was this block a pox anchor for an even earlier reward cycle?
        while let Some(older_prep_end) = self
            .sortition_db
            .get_prepare_end_for(&prep_end.sortition_id, &block_id)?
        {
            prep_end = older_prep_end;
        }

        info!(
            "Reprocessing with anchor block information, starting at block height: {}",
            prep_end.block_height
        );
        let mut pox_id = self.sortition_db.get_pox_id(sortition_id)?;
        pox_id.extend_with_present_block();

        // invalidate all the sortitions > canonical_sortition_tip, in the same burnchain fork
        self.sortition_db
            .invalidate_descendants_of(&prep_end.burn_header_hash)?;

        // roll back to the state as of prep_end
        self.canonical_chain_tip = Some(StacksBlockId::new(
            &prep_end.consensus_hash,
            &prep_end.canonical_stacks_tip_hash,
        ));
        self.canonical_sortition_tip = Some(prep_end.sortition_id);
        self.canonical_pox_id = Some(pox_id);

        // Start processing from the beginning of the new PoX reward set
        self.handle_new_burnchain_block()
    }
}

/// Determine whether or not the current chainstate databases are up-to-date with the current
/// epoch.
pub fn check_chainstate_db_versions(
    epochs: &[StacksEpoch],
    sortdb_path: &str,
    chainstate_path: &str,
) -> Result<bool, DBError> {
    let mut cur_epoch_opt = None;
    if fs::metadata(&sortdb_path).is_ok() {
        // check sortition DB and load up the current epoch
        let max_height = SortitionDB::get_highest_block_height_from_path(&sortdb_path)
            .expect("FATAL: could not query sortition DB for maximum block height");
        let cur_epoch_idx = StacksEpoch::find_epoch(epochs, max_height).expect(&format!(
            "FATAL: no epoch defined for burn height {}",
            max_height
        ));
        let cur_epoch = epochs[cur_epoch_idx].epoch_id;

        // save for later
        cur_epoch_opt = Some(cur_epoch.clone());
        let db_version = SortitionDB::get_db_version_from_path(&sortdb_path)?
            .expect("FATAL: could not load sortition DB version");

        if !SortitionDB::is_db_version_supported_in_epoch(cur_epoch, &db_version) {
            error!(
                "Sortition DB at {} does not support epoch {}",
                &sortdb_path, cur_epoch
            );
            return Ok(false);
        }
    } else {
        warn!("Sortition DB {} does not exist; assuming it will be instantiated with the correct version", sortdb_path);
    }

    if fs::metadata(&chainstate_path).is_ok() {
        let cur_epoch = cur_epoch_opt.expect(
            "FATAL: chainstate corruption: sortition DB does not exist, but chainstate does.",
        );
        let db_config = StacksChainState::get_db_config_from_path(&chainstate_path)?;
        if !db_config.supports_epoch(cur_epoch) {
            error!(
                "Chainstate DB at {} does not support epoch {}",
                &chainstate_path, cur_epoch
            );
            return Ok(false);
        }
    } else {
        warn!("Chainstate DB {} does not exist; assuming it will be instantiated with the correct version", chainstate_path);
    }

    Ok(true)
}

/// Migrate all databases to their latest schemas.
/// Verifies that this is possible as well
pub fn migrate_chainstate_dbs(
    epochs: &[StacksEpoch],
    sortdb_path: &str,
    chainstate_path: &str,
    chainstate_marf_opts: Option<MARFOpenOpts>,
) -> Result<(), Error> {
    if !check_chainstate_db_versions(epochs, sortdb_path, chainstate_path)? {
        warn!("Unable to migrate chainstate DBs to the latest schemas in the current epoch");
        return Err(DBError::TooOldForEpoch.into());
    }

    if fs::metadata(&sortdb_path).is_ok() {
        info!("Migrating sortition DB to the latest schema version");
        SortitionDB::migrate_if_exists(&sortdb_path, epochs)?;
    }
    if fs::metadata(&chainstate_path).is_ok() {
        info!("Migrating chainstate DB to the latest schema version");
        let db_config = StacksChainState::get_db_config_from_path(&chainstate_path)?;

        // this does the migration internally
        let _ = StacksChainState::open(
            db_config.mainnet,
            db_config.chain_id,
            chainstate_path,
            chainstate_marf_opts,
        )?;
    }
    Ok(())
}
