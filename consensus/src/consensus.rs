// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{error::ConsensusError, ConsensusParameters, MemoryPool, MerkleTreeLedger};
use itertools::izip;
use snarkos_metrics::misc::BLOCK_HEIGHT;
use snarkos_storage::BlockPath;
use snarkvm_algorithms::CRH;
use snarkvm_dpc::{Account, Address, PrivateKey, AccountScheme, AleoAmount, Block, DPCComponents, DPCScheme, LedgerScheme, Storage, Transactions as DPCTransactions, testnet1::{
        instantiated::*,
        payload::Payload as RecordPayload,
        Record as DPCRecord,
    }};
use snarkvm_posw::txids_to_roots;
use snarkvm_utilities::{to_bytes, ToBytes};

use rand::{CryptoRng, Rng};

use std::sync::Arc;

pub struct Consensus<S: Storage> {
    pub parameters: ConsensusParameters,
    pub public_parameters: Testnet1DPC,
    pub ledger: Arc<MerkleTreeLedger<S>>,
    pub memory_pool: MemoryPool<Testnet1Transaction>,
}

impl<S: Storage> Consensus<S> {
    /// Check if the transaction is valid.
    pub fn verify_transaction(&self, transaction: &Testnet1Transaction) -> Result<bool, ConsensusError> {
        if !self
            .parameters
            .authorized_inner_snark_ids
            .contains(&to_bytes![transaction.inner_circuit_id]?)
        {
            return Ok(false);
        }

        Ok(Testnet1DPC::verify(
            &self.public_parameters,
            transaction,
            &*self.ledger,
        ))
    }

    /// Check if the transactions are valid.
    pub fn verify_transactions(&self, transactions: &[Testnet1Transaction]) -> Result<bool, ConsensusError> {
        for tx in transactions {
            if !self
                .parameters
                .authorized_inner_snark_ids
                .contains(&to_bytes![tx.inner_circuit_id]?)
            {
                return Ok(false);
            }
        }

        Ok(Testnet1DPC::verify_transactions(
            &self.public_parameters,
            transactions,
            &*self.ledger,
        ))
    }

    /// Check if the block is valid.
    /// Verify transactions and transaction fees.
    pub fn verify_block(&self, block: &Block<Testnet1Transaction>) -> Result<bool, ConsensusError> {
        let transaction_ids: Vec<_> = block.transactions.to_transaction_ids()?;
        let (merkle_root, pedersen_merkle_root, _) = txids_to_roots(&transaction_ids);

        // Verify the block header
        if !crate::is_genesis(&block.header) {
            let parent_block = self.ledger.get_latest_block()?;
            if let Err(err) =
                self.parameters
                    .verify_header(&block.header, &parent_block.header, &merkle_root, &pedersen_merkle_root)
            {
                error!("block header failed to verify: {:?}", err);
                return Ok(false);
            }
        }
        // Verify block amounts and check that there is a single coinbase transaction

        let mut coinbase_transaction_count = 0;
        let mut total_value_balance = AleoAmount::ZERO;

        for transaction in block.transactions.iter() {
            let value_balance = transaction.value_balance;

            if value_balance.is_negative() {
                coinbase_transaction_count += 1;
            }

            total_value_balance = total_value_balance.add(value_balance);
        }

        // Check that there is only 1 coinbase transaction
        if coinbase_transaction_count > 1 {
            error!("multiple coinbase transactions");
            return Ok(false);
        }

        // Check that the block value balances are correct
        let expected_block_reward = crate::get_block_reward(self.ledger.len() as u32).0;
        if total_value_balance.0 + expected_block_reward != 0 {
            trace!("total_value_balance: {:?}", total_value_balance);
            trace!("expected_block_reward: {:?}", expected_block_reward);

            return Ok(false);
        }

        // Check that all the transaction proofs verify
        self.verify_transactions(&block.transactions.0)
    }

    /// Receive a block from an external source and process it based on ledger state.
    pub async fn receive_block(&self, block: &Block<Testnet1Transaction>, batch_import: bool) -> Result<(), ConsensusError> {
        // Block is an unknown orphan
        if !self.ledger.previous_block_hash_exists(block) && !self.ledger.is_previous_block_canon(&block.header) {
            debug!("Processing a block that is an unknown orphan");

            // There are two possible cases for an unknown orphan.
            // 1) The block is a genesis block, or
            // 2) The block is unknown and does not correspond with the canon chain.
            if crate::is_genesis(&block.header) && self.ledger.is_empty() {
                self.process_block(block).await?;
            } else {
                self.ledger.insert_only(block)?;
            }
        } else {
            // If the block is not an unknown orphan, find the origin of the block
            match self.ledger.get_block_path(&block.header)? {
                BlockPath::ExistingBlock => {
                    debug!("Received a pre-existing block");
                    return Err(ConsensusError::PreExistingBlock);
                }
                BlockPath::CanonChain(block_height) => {
                    debug!("Processing a block that is on canon chain. Height {}", block_height);

                    self.process_block(block).await?;

                    if !batch_import {
                        // Attempt to fast forward the block state if the node already stores
                        // the children of the new canon block.
                        let child_path = self.ledger.longest_child_path(block.header.get_hash())?;

                        if child_path.len() > 1 {
                            debug!(
                                "Attempting to canonize the descendants of block at height {}.",
                                block_height
                            );
                        }

                        for child_block_hash in child_path.into_iter().skip(1) {
                            let new_block = self.ledger.get_block(&child_block_hash)?;

                            debug!(
                                "Processing the next known descendant. Height {}",
                                self.ledger.get_current_block_height() + 1
                            );
                            self.process_block(&new_block).await?;
                        }
                    }
                }
                BlockPath::SideChain(side_chain_path) => {
                    debug!(
                        "Processing a block that is on side chain. Height {}",
                        side_chain_path.new_block_number
                    );

                    // If the side chain is now longer than the canon chain,
                    // perform a fork to the side chain.
                    if side_chain_path.new_block_number > self.ledger.get_current_block_height() {
                        debug!(
                            "Determined side chain is longer than canon chain by {} blocks",
                            side_chain_path.new_block_number - self.ledger.get_current_block_height()
                        );
                        warn!("A valid fork has been detected. Performing a fork to the side chain.");

                        // Fork to superior side chain
                        self.ledger.revert_for_fork(&side_chain_path)?;

                        // Update the current block height metric.
                        metrics::gauge!(BLOCK_HEIGHT, self.ledger.get_current_block_height() as f64);

                        if !side_chain_path.path.is_empty() {
                            for block_hash in side_chain_path.path {
                                if block_hash == block.header.get_hash() {
                                    self.process_block(block).await?
                                } else {
                                    let new_block = self.ledger.get_block(&block_hash)?;
                                    self.process_block(&new_block).await?;
                                }
                            }
                        }
                    } else {
                        // If the sidechain is not longer than the main canon chain, simply store the block
                        self.ledger.insert_only(block)?;
                    }
                }
            };
        }

        Ok(())
    }

    /// Return whether or not the given block is valid and insert it.
    /// 1. Verify that the block header is valid.
    /// 2. Verify that the transactions are valid.
    /// 3. Insert/canonize block.
    pub async fn process_block(&self, block: &Block<Testnet1Transaction>) -> Result<(), ConsensusError> {
        if self.ledger.is_canon(&block.header.get_hash()) {
            return Ok(());
        }

        // 1. Verify that the block valid
        if !self.verify_block(block)? {
            return Err(ConsensusError::InvalidBlock(block.header.get_hash().0.to_vec()));
        }

        // 2. Insert/canonize block
        self.ledger.insert_and_commit(block)?;

        // Increment the current block height metric
        metrics::increment_gauge!(BLOCK_HEIGHT, 1.0);

        // 3. Remove transactions from the mempool
        for transaction_id in block.transactions.to_transaction_ids()? {
            self.memory_pool.remove_by_hash(&transaction_id).await?;
        }

        Ok(())
    }

    /// Generate a transaction by spending old records and specifying new record attributes
    #[allow(clippy::too_many_arguments)]
    pub fn create_transaction<R: Rng + CryptoRng>(
        &self,
        old_records: Vec<DPCRecord<Components>>,
        old_account_private_keys: Vec<PrivateKey<Components>>,
        new_record_owners: Vec<Address<Components>>,
        new_birth_program_ids: Vec<Vec<u8>>,
        new_death_program_ids: Vec<Vec<u8>>,
        new_is_dummy_flags: Vec<bool>,
        new_values: Vec<u64>,
        new_payloads: Vec<RecordPayload>,
        memo: [u8; 32],
        mut rng: &mut R,
    ) -> Result<(Vec<DPCRecord<Components>>, Testnet1Transaction), ConsensusError> {
        assert_eq!(old_records.len(), Components::NUM_INPUT_RECORDS);
        assert_eq!(old_account_private_keys.len(), Components::NUM_INPUT_RECORDS);
        assert_eq!(new_record_owners.len(), Components::NUM_OUTPUT_RECORDS);
        assert_eq!(new_birth_program_ids.len(), Components::NUM_OUTPUT_RECORDS);
        assert_eq!(new_death_program_ids.len(), Components::NUM_OUTPUT_RECORDS);
        assert_eq!(new_is_dummy_flags.len(), Components::NUM_OUTPUT_RECORDS);
        assert_eq!(new_values.len(), Components::NUM_OUTPUT_RECORDS);
        assert_eq!(new_payloads.len(), Components::NUM_OUTPUT_RECORDS);

        let mut joint_serial_numbers = vec![];
        for (record, key) in old_records.iter().zip(old_account_private_keys.iter()) {
            let (_, serial) = record.to_serial_number(&self.public_parameters.system_parameters.account_signature, key)?;
            joint_serial_numbers.extend_from_slice(&serial[..]);
        }

        let new_records = izip!(new_record_owners, new_birth_program_ids, new_death_program_ids, new_is_dummy_flags, new_values, new_payloads, 0..2)
            .map(|(owner, new_birth_program_id, new_death_program_id, new_is_dummy_flag, new_value, new_payload, index)| {
                DPCRecord::new_full(
                    &self.public_parameters.system_parameters.serial_number_nonce,
                    &self.public_parameters.system_parameters.record_commitment,
                    owner,
                    new_is_dummy_flag,
                    new_value,
                    new_payload,
                    new_birth_program_id,
                    new_death_program_id,
                    index,
                    joint_serial_numbers.clone(),
                    &mut rng,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        // Offline execution to generate a DPC transaction
        let transaction_kernel = <Testnet1DPC as DPCScheme<MerkleTreeLedger<S>>>::execute_offline_phase(
            &self.public_parameters,
            &old_account_private_keys,
            old_records,
            new_records,
            memo,
            rng,
        )?;

        // Construct the program proofs
        let (old_death_program_proofs, new_birth_program_proofs) =
            ConsensusParameters::generate_program_proofs::<R, S>(&self.public_parameters, &transaction_kernel, rng)?;
        let mut all_proofs = old_death_program_proofs;
        all_proofs.extend(new_birth_program_proofs);

        // Online execution to generate a DPC transaction
        let (new_records, transaction) = Testnet1DPC::execute_online_phase(
            &self.public_parameters,
            &old_account_private_keys,
            transaction_kernel,
            all_proofs,
            &*self.ledger,
            rng,
        )?;

        Ok((new_records, transaction))
    }

    /// Generate a coinbase transaction given candidate block transactions
    #[allow(clippy::too_many_arguments)]
    pub fn create_coinbase_transaction<R: Rng + CryptoRng>(
        &self,
        block_num: u32,
        transactions: &DPCTransactions<Testnet1Transaction>,
        program_vk_hash: Vec<u8>,
        new_birth_program_ids: Vec<Vec<u8>>,
        new_death_program_ids: Vec<Vec<u8>>,
        recipient: Address<Components>,
        rng: &mut R,
    ) -> Result<(Vec<DPCRecord<Components>>, Testnet1Transaction), ConsensusError> {
        let mut total_value_balance = crate::get_block_reward(block_num);

        for transaction in transactions.iter() {
            let tx_value_balance = transaction.value_balance;

            if tx_value_balance.is_negative() {
                return Err(ConsensusError::CoinbaseTransactionAlreadyExists());
            }

            total_value_balance = total_value_balance.add(transaction.value_balance);
        }

        // Generate a new account that owns the dummy input records
        let new_account = Account::new(
            &self.public_parameters.system_parameters.account_signature,
            &self.public_parameters.system_parameters.account_commitment,
            &self.public_parameters.system_parameters.account_encryption,
            rng,
        )
        .unwrap();

        // Generate dummy input records having as address the genesis address.
        let old_account_private_keys = vec![new_account.private_key.clone(); Components::NUM_INPUT_RECORDS];
        let mut old_records = Vec::with_capacity(Components::NUM_INPUT_RECORDS);
        for _ in 0..Components::NUM_INPUT_RECORDS {
            let sn_nonce_input: [u8; 4] = rng.gen();

            let old_sn_nonce = <Components as DPCComponents>::SerialNumberNonceCRH::hash(
                &self.public_parameters.system_parameters.serial_number_nonce,
                &sn_nonce_input,
            )?;

            let old_record = DPCRecord::<Components>::new(
                &self.public_parameters.system_parameters.record_commitment,
                new_account.address.clone(),
                true,
                0,
                RecordPayload::default(),
                program_vk_hash.clone(),
                program_vk_hash.clone(),
                old_sn_nonce,
                // Filler program input
                rng,
            )?;

            old_records.push(old_record);
        }

        let new_record_owners = vec![recipient; Components::NUM_OUTPUT_RECORDS];
        let new_is_dummy_flags = [vec![false], vec![true; Components::NUM_OUTPUT_RECORDS - 1]].concat();
        let new_values = [vec![total_value_balance.0 as u64], vec![
            0;
            Components::NUM_OUTPUT_RECORDS
                - 1
        ]]
        .concat();
        let new_payloads = vec![RecordPayload::default(); Components::NUM_OUTPUT_RECORDS];

        let memo: [u8; 32] = rng.gen();

        self.create_transaction(
            old_records,
            old_account_private_keys,
            new_record_owners,
            new_birth_program_ids,
            new_death_program_ids,
            new_is_dummy_flags,
            new_values,
            new_payloads,
            memo,
            rng,
        )
    }
}
