// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    Chunk, Command, CommittableBlockBatch, ExecutableBlock, ExecutedTrees, ProcessedVMOutput,
    TransactionData, OP_COUNTERS,
};
use failure::prelude::*;
use futures::channel::oneshot;
use libra_config::config::VMConfig;
use libra_crypto::{
    hash::{CryptoHash, EventAccumulatorHasher},
    HashValue,
};
use libra_logger::prelude::*;
use libra_types::{
    account_address::AccountAddress,
    account_state_blob::AccountStateBlob,
    proof::{accumulator::InMemoryAccumulator, definition::LeafCount, SparseMerkleProof},
    transaction::{
        Transaction, TransactionInfo, TransactionOutput, TransactionPayload, TransactionStatus,
        TransactionToCommit, Version,
    },
    validator_set::ValidatorSet,
    write_set::{WriteOp, WriteSet},
};
use scratchpad::{ProofRead, SparseMerkleTree};
use std::{
    collections::{hash_map, BTreeMap, HashMap, HashSet, VecDeque},
    convert::TryFrom,
    marker::PhantomData,
    sync::{mpsc, Arc, Mutex},
};
use storage_client::{StorageRead, StorageWrite, VerifiedStateView};
use vm_runtime::VMExecutor;

#[derive(Debug)]
enum Mode {
    Normal,
    Syncing,
}

fn error_when_syncing(id: HashValue) -> Error {
    let message = format!("Syncing. Unable to serve request for block {:x}.", id);
    warn!("{}", message);
    format_err!("{}", message)
}

pub(crate) struct BlockProcessor<V> {
    /// Where the processor receives commands.
    command_receiver: mpsc::Receiver<Command>,

    /// The timestamp of the last committed ledger info.
    committed_timestamp_usecs: u64,

    committed_trees: Arc<Mutex<ExecutedTrees>>,

    /// The cached executable blocks.
    blocks_to_execute: VecDeque<(ExecutableBlock, oneshot::Sender<Result<ProcessedVMOutput>>)>,

    /// The blocks that are ready to be sent to storage.
    block_batch_to_commit: Option<(CommittableBlockBatch, oneshot::Sender<Result<()>>)>,

    /// Client to storage service.
    storage_read_client: Arc<dyn StorageRead>,
    storage_write_client: Arc<dyn StorageWrite>,

    /// The current mode. If we are doing state synchronization, we will refuse to serve normal
    /// execute_block and commit_block requests.
    mode: Mode,

    /// Configuration for the VM. The block processor currently creates a new VM for each block.
    vm_config: VMConfig,

    phantom: PhantomData<V>,
}

impl<V> BlockProcessor<V>
where
    V: VMExecutor,
{
    /// Constructs a new `BlockProcessor`.
    pub fn new(
        command_receiver: mpsc::Receiver<Command>,
        committed_timestamp_usecs: u64,
        committed_trees: Arc<Mutex<ExecutedTrees>>,
        storage_read_client: Arc<dyn StorageRead>,
        storage_write_client: Arc<dyn StorageWrite>,
        vm_config: VMConfig,
    ) -> Self {
        BlockProcessor {
            command_receiver,
            committed_timestamp_usecs,
            committed_trees,
            blocks_to_execute: VecDeque::new(),
            block_batch_to_commit: None,
            storage_read_client,
            storage_write_client,
            mode: Mode::Normal,
            vm_config,
            phantom: PhantomData,
        }
    }

    /// Keeps processing blocks until the command sender is disconnected.
    pub fn run(&mut self) {
        loop {
            // Fetch and process all commands sent by consensus until there is no more left in the
            // channel.
            while let Ok(cmd) = self.command_receiver.try_recv() {
                self.process_command(cmd);
            }

            // Check if there are blocks waiting to be committed.
            // Continue if this function made progress (Committed some blocks).
            if self.maybe_commit_blocks() {
                continue;
            }

            // If we do not have anything else to do, check if there is a block pending execution.
            // Continue if this function made progress (executed one block).
            if self.maybe_execute_block() {
                continue;
            }

            // We really have nothing to do. Just block the thread until consensus sends us new
            // command.
            match self.command_receiver.recv() {
                Ok(cmd) => self.process_command(cmd),
                Err(mpsc::RecvError) => break,
            }
        }
    }

    fn maybe_commit_blocks(&mut self) -> bool {
        // Note: If save_blocks_to_storage below fails, these blocks will stay in
        // `self.block_batches_to_store`. This is okay because consensus will not retry committing
        // these blocks after it receives the errors. Instead it will try to commit a
        // descendant block later, which will be found in the block tree and cause the entire
        // chain to be saved if storage has recovered. (If consensus retries committing these
        // moved blocks, we won't find these blocks in the block tree because we only look up
        // the blocks in the block tree, so we will return an error.)
        let (block_batch, resp_sender) = match self.block_batch_to_commit.take() {
            Some((block_batch, resp_sender)) => (block_batch, resp_sender),
            None => return false,
        };

        let res = self.commit_block_batch(block_batch);
        if let Err(_err) = resp_sender.send(res) {
            warn!("Failed to send commit block batch response.");
        };
        true
    }

    /// Processes a single command from consensus. Note that this only modifies the block tree, the
    /// actual block execution and commit may happen later.
    fn process_command(&mut self, cmd: Command) {
        match cmd {
            Command::ExecuteBlock {
                executable_block,
                resp_sender,
            } => {
                if let Mode::Syncing = self.mode {
                    if let Err(_err) =
                        resp_sender.send(Err(error_when_syncing(executable_block.id)))
                    {
                        warn!("Failed to send execute block error (sync mode).");
                    };
                    return;
                }
                self.blocks_to_execute
                    .push_back((executable_block, resp_sender));
            }
            Command::CommitBlockBatch {
                committable_block_batch,
                resp_sender,
            } => {
                let id = committable_block_batch
                    .finality_proof
                    .ledger_info()
                    .consensus_block_id();
                if let Mode::Syncing = self.mode {
                    if let Err(_err) = resp_sender.send(Err(error_when_syncing(id))) {
                        warn!("Failed to send commit blocks error (sync mode).");
                    };
                    return;
                }
                assert!(self
                    .block_batch_to_commit
                    .replace((committable_block_batch, resp_sender))
                    .is_none());
            }
            Command::ExecuteAndCommitChunk { chunk, resp_sender } => {
                let res = self.execute_and_commit_chunk(chunk.clone()).map_err(|e| {
                    security_log(SecurityEvent::InvalidChunkExecutor)
                        .error(&e)
                        .data(chunk.txn_list_with_proof)
                        .data(chunk.ledger_info_with_sigs)
                        .log();
                    e
                });
                if let Err(_err) = resp_sender.send(res) {
                    warn!("Failed to send execute and commit chunk response.");
                }
            }
        }
    }

    /// Verifies the transactions based on the provided proofs and ledger info. If the transactions
    /// are valid, executes them and commits immediately if execution results match the proofs.
    fn execute_and_commit_chunk(&mut self, chunk: Chunk) -> Result<()> {
        if chunk.ledger_info_with_sigs.ledger_info().timestamp_usecs()
            <= self.committed_timestamp_usecs
        {
            warn!(
                "Ledger info is too old: local timestamp: {}, timestamp in request: {}.",
                self.committed_timestamp_usecs,
                chunk.ledger_info_with_sigs.ledger_info().timestamp_usecs(),
            );
            return Ok(());
        }

        if let Mode::Normal = self.mode {
            self.mode = Mode::Syncing;
            info!("Start syncing...");
        }

        let mut committed_trees = self.committed_trees.lock().unwrap();
        info!(
            "Local version: {}. First transaction version in request: {:?}. \
             Number of transactions in request: {}.",
            committed_trees.txn_accumulator().num_leaves() - 1,
            chunk.txn_list_with_proof.first_transaction_version,
            chunk.txn_list_with_proof.transactions.len(),
        );

        let (num_txns_to_skip, first_version) =
            Self::verify_chunk(&chunk, committed_trees.txn_accumulator().num_leaves())?;

        let (txn_list_with_proof, li_with_sigs) =
            (chunk.txn_list_with_proof, chunk.ledger_info_with_sigs);
        info!("Skipping the first {} transactions.", num_txns_to_skip);
        let transactions: Vec<_> = txn_list_with_proof
            .transactions
            .into_iter()
            .skip(num_txns_to_skip as usize)
            .collect();

        // Construct a StateView and pass the transactions to VM.
        let state_view = VerifiedStateView::new(
            Arc::clone(&self.storage_read_client),
            committed_trees.version(),
            committed_trees.state_root(),
            committed_trees.state_tree(),
        );
        let vm_outputs = {
            let _timer = OP_COUNTERS.timer("vm_execute_chunk_time_s");
            V::execute_block(transactions.to_vec(), &self.vm_config, &state_view)?
        };

        // Since other validators have committed these transactions, their status should all be
        // TransactionStatus::Keep.
        for output in &vm_outputs {
            if let TransactionStatus::Discard(_) = output.status() {
                bail!("Syncing transactions that should be discarded.");
            }
        }

        let (account_to_btree, account_to_proof) = state_view.into();

        let output = Self::process_vm_outputs(
            account_to_btree,
            account_to_proof,
            &transactions,
            vm_outputs,
            &committed_trees,
        )?;

        // Since we have verified the proofs, we just need to verify that each TransactionInfo
        // object matches what we have computed locally.
        let mut txns_to_commit = vec![];
        for (txn, txn_data) in itertools::zip_eq(transactions, output.transaction_data()) {
            txns_to_commit.push(TransactionToCommit::new(
                txn,
                txn_data.account_blobs().clone(),
                txn_data.events().to_vec(),
                txn_data.gas_used(),
                txn_data.status().vm_status().major_status,
            ));
        }

        // If this is the last chunk corresponding to this ledger info, send the ledger info to
        // storage.
        let ledger_info_to_commit = if committed_trees.txn_accumulator().num_leaves()
            + txns_to_commit.len() as LeafCount
            == li_with_sigs.ledger_info().version() + 1
        {
            ensure!(
                li_with_sigs.ledger_info().transaction_accumulator_hash()
                    == output.executed_trees().txn_accumulator().root_hash(),
                "Root hash in ledger info does not match local computation."
            );
            Some(li_with_sigs)
        } else {
            // This means that the current chunk is not the last one. If it's empty, there's
            // nothing to write to storage. Since storage expect either new transaction or new
            // ledger info, we need to return here.
            if txns_to_commit.is_empty() {
                return Ok(());
            }
            None
        };
        self.storage_write_client.save_transactions(
            txns_to_commit,
            first_version,
            ledger_info_to_commit.clone(),
        )?;

        *committed_trees = output.executed_trees().clone();
        // Drop the read lock explicitely to avoid wrapping the code above into a code block.
        drop(committed_trees);

        if let Some(ledger_info_with_sigs) = ledger_info_to_commit {
            self.committed_timestamp_usecs = ledger_info_with_sigs.ledger_info().timestamp_usecs();
            self.mode = Mode::Normal;
            info!(
                "Synced to version {}.",
                ledger_info_with_sigs.ledger_info().version()
            );
        }
        Ok(())
    }

    /// Verifies proofs using provided ledger info. Also verifies that the version of the first
    /// transaction matches the latest committed transaction. If the first few transaction happens
    /// to be older, returns how many need to be skipped and the first version to be committed.
    fn verify_chunk(chunk: &Chunk, num_committed_txns: u64) -> Result<(LeafCount, Version)> {
        let txn_list_with_proof = &chunk.txn_list_with_proof;
        let ledger_info_with_sigs = &chunk.ledger_info_with_sigs;
        txn_list_with_proof.verify(
            ledger_info_with_sigs.ledger_info(),
            txn_list_with_proof.first_transaction_version,
        )?;

        if txn_list_with_proof.transactions.is_empty() {
            return Ok((0, num_committed_txns as Version /* first_version */));
        }

        let first_txn_version = txn_list_with_proof
            .first_transaction_version
            .expect("first_transaction_version should exist.")
            as Version;

        ensure!(
            first_txn_version <= num_committed_txns,
            "Transaction list too new. Expected version: {}. First transaction version: {}.",
            num_committed_txns,
            first_txn_version
        );
        Ok((
            num_committed_txns - first_txn_version,
            num_committed_txns as Version,
        ))
    }

    /// Saves eligible blocks to persistent storage. If the blocks are successfully persisted, they
    /// will be taken from `self.block_batch_to_store` and the in-memory Sparse Merkle Trees in
    /// these blocks will be pruned. Otherwise nothing happens.
    ///
    /// If we have multiple blocks and not all of them have signatures, we may send them to storage
    /// in a few batches. For example, if we have
    /// ```text
    /// A <- B <- C <- D <- E
    /// ```
    /// and only `C` and `E` have signatures, we will send `A`, `B` and `C` in the first batch,
    /// then `D` and `E` later in the another batch.
    fn commit_block_batch(&mut self, block_batch: CommittableBlockBatch) -> Result<()> {
        // All transactions that need to go to storage. In the above example, this means all the
        // transactions in A, B and C whose status == TransactionStatus::Keep.
        let mut txns_to_commit = vec![];
        let mut num_accounts_created = 0;
        for block in &block_batch.blocks {
            for (txn, txn_data) in
                itertools::zip_eq(&block.transactions, block.output.transaction_data())
            {
                if let TransactionStatus::Keep(_) = txn_data.status() {
                    txns_to_commit.push(TransactionToCommit::new(
                        txn.clone(),
                        txn_data.account_blobs().clone(),
                        txn_data.events().to_vec(),
                        txn_data.gas_used(),
                        txn_data.status().vm_status().major_status,
                    ));
                    num_accounts_created += txn_data.num_account_created();
                }
            }
        }

        let last_block = block_batch
            .blocks
            .last()
            .expect("CommittableBlockBatch has at least 1 block.");

        // Check that the version in ledger info (computed by consensus) matches the version
        // computed by us. TODO: we should also verify signatures and check that timestamp is
        // strictly increasing.
        let ledger_info_with_sigs = block_batch.finality_proof;
        let version = ledger_info_with_sigs.ledger_info().version();
        let num_txns_in_accumulator = last_block
            .output
            .executed_trees()
            .txn_accumulator()
            .num_leaves();
        assert_eq!(
            version + 1,
            num_txns_in_accumulator as Version,
            "Number of transactions in ledger info ({}) does not match number of transactions \
             in accumulator ({}).",
            version + 1,
            num_txns_in_accumulator,
        );

        let num_txns_to_commit = txns_to_commit.len() as u64;
        {
            let _timer = OP_COUNTERS.timer("storage_save_transactions_time_s");
            OP_COUNTERS.observe(
                "storage_save_transactions.count",
                txns_to_commit.len() as f64,
            );
            self.storage_write_client.save_transactions(
                txns_to_commit,
                version + 1 - num_txns_to_commit, /* first_version */
                Some(ledger_info_with_sigs.clone()),
            )?;
        }
        // Only bump the counter when the commit succeeds.
        OP_COUNTERS.inc_by("num_accounts", num_accounts_created);

        // Now that the blocks are persisted successfully, we can reply to consensus and update
        // in-memory state.
        self.committed_timestamp_usecs = ledger_info_with_sigs.ledger_info().timestamp_usecs();
        *self.committed_trees.lock().unwrap() = last_block.output.executed_trees().clone();
        for block in block_batch.blocks {
            for txn_data in block.output.transaction_data() {
                txn_data.prune_state_tree();
            }
        }

        Ok(())
    }

    /// Checks if there is a block in the tree ready for execution, if so run it by calling the VM.
    /// Returns `true` if a block was successfully executed, `false` if there was no block to
    /// execute.
    fn maybe_execute_block(&mut self) -> bool {
        let (executable_block, resp_sender) = match self.blocks_to_execute.pop_front() {
            Some((block, resp_sender)) => (block, resp_sender),
            None => return false,
        };

        {
            let _timer = OP_COUNTERS.timer("block_execute_time_s");
            let res = self.execute_block(executable_block);
            if let Err(_err) = resp_sender.send(res) {
                warn!("Failed to send execute block response.");
            };
        }
        true
    }

    fn execute_block(&mut self, executable_block: ExecutableBlock) -> Result<ProcessedVMOutput> {
        // Construct a StateView and pass the transactions to VM.
        let state_view = {
            let committed_trees = self.committed_trees.lock().unwrap();
            VerifiedStateView::new(
                Arc::clone(&self.storage_read_client),
                committed_trees.version(),
                committed_trees.state_root(),
                executable_block.parent_trees.state_tree(),
            )
        };

        let vm_outputs = {
            let _timer = OP_COUNTERS.timer("vm_execute_block_time_s");
            V::execute_block(
                executable_block.transactions.clone(),
                &self.vm_config,
                &state_view,
            )?
        };

        let status: Vec<_> = vm_outputs
            .iter()
            .map(TransactionOutput::status)
            .cloned()
            .collect();
        if !status.is_empty() {
            debug!("Execution status: {:?}", status);
        }

        let (account_to_btree, account_to_proof) = state_view.into();
        let output = Self::process_vm_outputs(
            account_to_btree,
            account_to_proof,
            &executable_block.transactions,
            vm_outputs,
            &executable_block.parent_trees,
        )
        .map_err(|err| format_err!("Failed to execute block: {}", err))?;

        Ok(output)
    }

    /// Post-processing of what the VM outputs. Returns the entire block's output.
    fn process_vm_outputs(
        mut account_to_btree: HashMap<AccountAddress, BTreeMap<Vec<u8>, Vec<u8>>>,
        account_to_proof: HashMap<HashValue, SparseMerkleProof>,
        transactions: &[Transaction],
        vm_outputs: Vec<TransactionOutput>,
        parent_trees: &ExecutedTrees,
    ) -> Result<ProcessedVMOutput> {
        // The data of each individual transaction. For convenience purpose, even for the
        // transactions that will be discarded, we will compute its in-memory Sparse Merkle Tree
        // (it will be identical to the previous one).
        let mut txn_data = vec![];
        let mut current_state_tree = Arc::clone(parent_trees.state_tree());
        // The hash of each individual TransactionInfo object. This will not include the
        // transactions that will be discarded, since they do not go into the transaction
        // accumulator.
        let mut txn_info_hashes = vec![];
        let mut next_validator_set = None;

        let proof_reader = ProofReader::new(account_to_proof);
        for (vm_output, txn) in itertools::zip_eq(vm_outputs.into_iter(), transactions.iter()) {
            let (blobs, state_tree, num_accounts_created) = Self::process_write_set(
                txn,
                &mut account_to_btree,
                &proof_reader,
                vm_output.write_set().clone(),
                &current_state_tree,
            )?;

            let event_tree = {
                let event_hashes: Vec<_> =
                    vm_output.events().iter().map(CryptoHash::hash).collect();
                InMemoryAccumulator::<EventAccumulatorHasher>::from_leaves(&event_hashes)
            };

            match vm_output.status() {
                TransactionStatus::Keep(status) => {
                    ensure!(
                        !vm_output.write_set().is_empty(),
                        "Transaction with empty write set should be discarded.",
                    );
                    // Compute hash for the TransactionInfo object. We need the hash of the
                    // transaction itself, the state root hash as well as the event root hash.
                    let txn_info = TransactionInfo::new(
                        txn.as_signed_user_txn()?.hash(),
                        state_tree.root_hash(),
                        event_tree.root_hash(),
                        vm_output.gas_used(),
                        status.major_status,
                    );
                    txn_info_hashes.push(txn_info.hash());
                }
                TransactionStatus::Discard(_) => {
                    ensure!(
                        vm_output.write_set().is_empty(),
                        "Discarded transaction has non-empty write set.",
                    );
                    ensure!(
                        vm_output.events().is_empty(),
                        "Discarded transaction has non-empty events.",
                    );
                }
            }

            txn_data.push(TransactionData::new(
                blobs,
                vm_output.events().to_vec(),
                vm_output.status().clone(),
                Arc::clone(&state_tree),
                Arc::new(event_tree),
                vm_output.gas_used(),
                num_accounts_created,
            ));
            current_state_tree = state_tree;

            // check for change in validator set
            let validator_set_change_event_key = ValidatorSet::change_event_key();
            for event in vm_output.events() {
                if *event.key() == validator_set_change_event_key {
                    next_validator_set = Some(ValidatorSet::from_bytes(event.event_data())?);
                    break;
                }
            }
        }

        let current_transaction_accumulator = parent_trees
            .transaction_accumulator
            .append(&txn_info_hashes);
        Ok(ProcessedVMOutput::new(
            txn_data,
            ExecutedTrees {
                state_tree: current_state_tree,
                transaction_accumulator: Arc::new(current_transaction_accumulator),
            },
            next_validator_set,
        ))
    }

    /// For all accounts modified by this transaction, find the previous blob and update it based
    /// on the write set. Returns the blob value of all these accounts as well as the newly
    /// constructed state tree.
    fn process_write_set(
        transaction: &Transaction,
        account_to_btree: &mut HashMap<AccountAddress, BTreeMap<Vec<u8>, Vec<u8>>>,
        proof_reader: &ProofReader,
        write_set: WriteSet,
        previous_state_tree: &SparseMerkleTree,
    ) -> Result<(
        HashMap<AccountAddress, AccountStateBlob>,
        Arc<SparseMerkleTree>,
        usize, /* num_account_created */
    )> {
        let mut updated_blobs = HashMap::new();
        let mut num_accounts_created = 0;

        // Find all addresses this transaction touches while processing each write op.
        let mut addrs = HashSet::new();
        for (access_path, write_op) in write_set.into_iter() {
            let address = access_path.address;
            let path = access_path.path;
            match account_to_btree.entry(address) {
                hash_map::Entry::Occupied(mut entry) => {
                    let account_btree = entry.get_mut();
                    // TODO(gzh): we check account creation here for now. Will remove it once we
                    // have a better way.
                    if account_btree.is_empty() {
                        num_accounts_created += 1;
                    }
                    Self::update_account_btree(account_btree, path, write_op);
                }
                hash_map::Entry::Vacant(entry) => {
                    // Before writing to an account, VM should always read that account. So we
                    // should not reach this code path. The exception is genesis transaction (and
                    // maybe other FTVM transactions).
                    match transaction.as_signed_user_txn()?.payload() {
                        TransactionPayload::Program(_)
                        | TransactionPayload::Module(_)
                        | TransactionPayload::Script(_) => {
                            bail!("Write set should be a subset of read set.")
                        }
                        TransactionPayload::WriteSet(_) => (),
                    }

                    let mut account_btree = BTreeMap::new();
                    Self::update_account_btree(&mut account_btree, path, write_op);
                    entry.insert(account_btree);
                }
            }
            addrs.insert(address);
        }

        for addr in addrs {
            let account_btree = account_to_btree.get(&addr).expect("Address should exist.");
            let account_blob = AccountStateBlob::try_from(account_btree)?;
            updated_blobs.insert(addr, account_blob);
        }
        let state_tree = Arc::new(
            previous_state_tree
                .update(
                    updated_blobs
                        .iter()
                        .map(|(addr, value)| (addr.hash(), value.clone()))
                        .collect(),
                    proof_reader,
                )
                .expect("Failed to update state tree."),
        );

        Ok((updated_blobs, state_tree, num_accounts_created))
    }

    fn update_account_btree(
        account_btree: &mut BTreeMap<Vec<u8>, Vec<u8>>,
        path: Vec<u8>,
        write_op: WriteOp,
    ) {
        match write_op {
            WriteOp::Value(new_value) => account_btree.insert(path, new_value),
            WriteOp::Deletion => account_btree.remove(&path),
        };
    }
}

struct ProofReader {
    account_to_proof: HashMap<HashValue, SparseMerkleProof>,
}

impl ProofReader {
    fn new(account_to_proof: HashMap<HashValue, SparseMerkleProof>) -> Self {
        ProofReader { account_to_proof }
    }
}

impl ProofRead for ProofReader {
    fn get_proof(&self, key: HashValue) -> Option<&SparseMerkleProof> {
        self.account_to_proof.get(&key)
    }
}
