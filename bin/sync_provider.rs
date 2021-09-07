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

#[macro_use]
extern crate tracing;

use snarkos::{
    cli::CLI,
    config::{Config, ConfigCli},
    display::{initialize_logger, print_welcome},
    errors::NodeError,
    init::{init_miner, init_node, init_rpc, init_storage, init_sync},
};
use snarkos_consensus::{Consensus, ConsensusParameters, DeserializedLedger, DynLedger, MemoryPool, MerkleLedger};
use snarkos_network::{config::Config as NodeConfig, MinerInstance, Node, NodeType, Sync};
use snarkos_rpc::start_rpc_server;
use snarkos_storage::{
    export_canon_blocks,
    key_value::KeyValueStore,
    AsyncStorage,
    DynStorage,
    RocksDb,
    SerialBlock,
    SqliteStorage,
    VMBlock,
};

use snarkvm_algorithms::{MerkleParameters, CRH, SNARK};
use snarkvm_dpc::{
    testnet1::{
        instantiated::{Components, Testnet1DPC, Testnet1Transaction},
        Testnet1Components,
    },
    Address,
    Block,
    DPCScheme,
    Network,
};
use snarkvm_parameters::{testnet1::GenesisBlock, Genesis, LedgerMerkleTreeParameters, Parameter};
use snarkvm_posw::PoswMarlin;
use snarkvm_utilities::{to_bytes_le, FromBytes, ToBytes};

use std::{fs, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use tokio::runtime;

///
/// Builds a node from configuration parameters.
///
/// 1. Creates new storage database or uses existing.
/// 2. Creates new memory pool or uses existing from storage.
/// 3. Creates sync parameters.
/// 4. Creates network server and starts the listener.
/// 5. Starts rpc server thread.
/// 6. Starts miner thread.
///
async fn start_server(config: Config) -> anyhow::Result<()> {
    initialize_logger(&config);

    print_welcome(&config);

    let storage = match init_storage(&config).await? {
        Some(storage) => storage,
        None => return Ok(()), // Return if no storage was returned (usually in case of validation).
    };

    let sync = init_sync(&config, storage.clone()).await?;

    // Construct the node instance. Note this does not start the network services.
    // This is done early on, so that the local address can be discovered
    // before any other object (miner, RPC) needs to use it.
    let mut node = init_node(&config, Some(storage.clone())).await?;

    // Enable the sync layer.
    node.set_sync(sync);

    // Initialize metrics framework.
    node.initialize_metrics().await?;

    // Start listening for incoming connections.
    node.listen().await?;

    // Start RPC thread, if the RPC configuration is enabled.
    if config.rpc.json_rpc {
        init_rpc(&config, node.clone(), Some(storage))?;
    }

    // Start the network services
    node.start_services().await;

    // Start the miner task if mining configuration is enabled.
    if config.miner.is_miner {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        init_miner(&config, node);
    }

    std::future::pending::<()>().await;

    Ok(())
}

fn main() -> Result<(), NodeError> {
    let arguments = ConfigCli::args();

    let mut config: Config = ConfigCli::parse(&arguments)?;
    config.node.kind = NodeType::SyncProvider;
    config.check().map_err(|e| NodeError::Message(e.to_string()))?;

    let runtime = runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?;

    runtime.block_on(start_server(config))?;

    Ok(())
}
