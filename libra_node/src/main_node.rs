// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use admission_control_proto::proto::admission_control_grpc::{
    create_admission_control, AdmissionControlClient,
};
use admission_control_service::admission_control_service::AdmissionControlService;
use config::config::{NodeConfig, RoleType};
use consensus::consensus_provider::{make_consensus_provider, ConsensusProvider};
use debug_interface::{node_debug_service::NodeDebugService, proto::node_debug_interface_grpc};
use execution_proto::proto::execution_grpc;
use execution_service::ExecutionService;
use grpc_helpers::ServerHandle;
use grpcio::{ChannelBuilder, EnvBuilder, ServerBuilder};
use grpcio_sys;
use logger::prelude::*;
use mempool::{proto::mempool_grpc::MempoolClient, MempoolRuntime};
use metrics::metric_server;
use network::{
    validator_network::{
        network_builder::{NetworkBuilder, TransportType},
        ConsensusNetworkEvents, ConsensusNetworkSender, MempoolNetworkEvents, MempoolNetworkSender,
        StateSynchronizerEvents, StateSynchronizerSender, CONSENSUS_DIRECT_SEND_PROTOCOL,
        CONSENSUS_RPC_PROTOCOL, MEMPOOL_DIRECT_SEND_PROTOCOL, STATE_SYNCHRONIZER_MSG_PROTOCOL,
    },
    NetworkPublicKeys, ProtocolId,
};
use nextgen_crypto::ed25519::*;
use state_synchronizer::StateSynchronizer;
use std::{
    cmp::min,
    convert::{TryFrom, TryInto},
    sync::Arc,
    thread,
    time::Instant,
};
use storage_client::{StorageRead, StorageReadServiceClient, StorageWriteServiceClient};
use storage_service::start_storage_service;
use tokio::runtime::{Builder, Runtime};
use types::account_address::AccountAddress as PeerId;
use vm_validator::vm_validator::VMValidator;

pub struct LibraHandle {
    _ac: ServerHandle,
    _mempool: Option<MempoolRuntime>,
    _state_synchronizer: StateSynchronizer,
    _network: Runtime,
    consensus: Option<Box<dyn ConsensusProvider>>,
    _execution: ServerHandle,
    _storage: ServerHandle,
    _debug: ServerHandle,
}

impl Drop for LibraHandle {
    fn drop(&mut self) {
        if let Some(consensus) = &mut self.consensus {
            consensus.stop();
        }
    }
}

fn setup_ac(config: &NodeConfig) -> (::grpcio::Server, AdmissionControlClient) {
    let env = Arc::new(
        EnvBuilder::new()
            .name_prefix("grpc-ac-")
            .cq_count(unsafe { min(grpcio_sys::gpr_cpu_num_cores() as usize * 2, 32) })
            .build(),
    );
    let port = config.admission_control.admission_control_service_port;

    // Create mempool client
    let mempool_client = match config.base.get_role() {
        RoleType::FullNode => None,
        RoleType::Validator => {
            let connection_str = format!("localhost:{}", config.mempool.mempool_service_port);
            let env2 = Arc::new(EnvBuilder::new().name_prefix("grpc-ac-mem-").build());
            Some(Arc::new(MempoolClient::new(
                ChannelBuilder::new(env2).connect(&connection_str),
            )))
        }
    };

    // Create storage read client
    let storage_client: Arc<dyn StorageRead> = Arc::new(StorageReadServiceClient::new(
        Arc::new(EnvBuilder::new().name_prefix("grpc-ac-sto-").build()),
        "localhost",
        config.storage.port,
    ));

    let vm_validator = Arc::new(VMValidator::new(&config, Arc::clone(&storage_client)));

    let handle = AdmissionControlService::new(
        mempool_client,
        storage_client,
        vm_validator,
        config
            .admission_control
            .need_to_check_mempool_before_validation,
    );
    let service = create_admission_control(handle);
    let server = ServerBuilder::new(Arc::clone(&env))
        .register_service(service)
        .bind(config.admission_control.address.clone(), port)
        .build()
        .expect("Unable to create grpc server");

    let connection_str = format!("localhost:{}", port);
    let client = AdmissionControlClient::new(ChannelBuilder::new(env).connect(&connection_str));
    (server, client)
}

fn setup_executor(config: &NodeConfig) -> ::grpcio::Server {
    let client_env = Arc::new(EnvBuilder::new().name_prefix("grpc-exe-sto-").build());
    let storage_read_client = Arc::new(StorageReadServiceClient::new(
        Arc::clone(&client_env),
        &config.storage.address,
        config.storage.port,
    ));
    let storage_write_client = Arc::new(StorageWriteServiceClient::new(
        Arc::clone(&client_env),
        &config.storage.address,
        config.storage.port,
        config.storage.grpc_max_receive_len,
    ));

    let handle = ExecutionService::new(storage_read_client, storage_write_client, config);
    let service = execution_grpc::create_execution(handle);
    ::grpcio::ServerBuilder::new(Arc::new(EnvBuilder::new().name_prefix("grpc-exe-").build()))
        .register_service(service)
        .bind(config.execution.address.clone(), config.execution.port)
        .build()
        .expect("Unable to create grpc server")
}

fn setup_debug_interface(config: &NodeConfig) -> ::grpcio::Server {
    let env = Arc::new(EnvBuilder::new().name_prefix("grpc-debug-").build());
    // Start Debug interface
    let debug_service =
        node_debug_interface_grpc::create_node_debug_interface(NodeDebugService::new());
    ::grpcio::ServerBuilder::new(env)
        .register_service(debug_service)
        .bind(
            config.debug_interface.address.clone(),
            config.debug_interface.admission_control_node_debug_port,
        )
        .build()
        .expect("Unable to create grpc server")
}

pub fn setup_network(
    config: &mut NodeConfig,
) -> (
    (MempoolNetworkSender, MempoolNetworkEvents),
    (ConsensusNetworkSender, ConsensusNetworkEvents),
    (StateSynchronizerSender, StateSynchronizerEvents),
    Runtime,
) {
    let runtime = Builder::new()
        .name_prefix("network-")
        .build()
        .expect("Failed to start runtime. Won't be able to start networking.");
    let peer_id = PeerId::try_from(config.base.peer_id.clone()).expect("Invalid PeerId");
    let listen_addr = config.network.listen_address.clone();
    let advertised_addr = config.network.advertised_address.clone();
    let trusted_peers = config
        .base
        .trusted_peers
        .get_trusted_network_peers()
        .clone()
        .into_iter()
        .map(|(peer_id, (signing_public_key, identity_public_key))| {
            (
                peer_id,
                NetworkPublicKeys {
                    signing_public_key,
                    identity_public_key,
                },
            )
        })
        .collect();
    let seed_peers = config
        .network
        .seed_peers
        .seed_peers
        .clone()
        .into_iter()
        .map(|(peer_id, addrs)| (peer_id.try_into().expect("Invalid PeerId"), addrs))
        .collect();
    let network_signing_private = config.base.peer_keypairs.take_network_signing_private()
        .expect("Failed to move network signing private key out of NodeConfig, key not set or moved already");

    let network_signing_public: Ed25519PublicKey = (&network_signing_private).into();
    let network_identity_keypair = config.base.peer_keypairs.get_network_identity_keypair();
    let (
        (mempool_network_sender, mempool_network_events),
        (consensus_network_sender, consensus_network_events),
        (state_sync_network_sender, state_sync_network_events),
        _listen_addr,
    ) = NetworkBuilder::new(runtime.executor(), peer_id, listen_addr)
        .transport(if config.network.enable_encryption_and_authentication {
            TransportType::TcpNoise
        } else {
            TransportType::Tcp
        })
        .advertised_address(advertised_addr)
        .seed_peers(seed_peers)
        .signing_keys((network_signing_private, network_signing_public))
        .identity_keys(network_identity_keypair)
        .trusted_peers(trusted_peers)
        .discovery_interval_ms(config.network.discovery_interval_ms)
        .connectivity_check_interval_ms(config.network.connectivity_check_interval_ms)
        .consensus_protocols(vec![
            ProtocolId::from_static(CONSENSUS_RPC_PROTOCOL),
            ProtocolId::from_static(CONSENSUS_DIRECT_SEND_PROTOCOL),
        ])
        .mempool_protocols(vec![ProtocolId::from_static(MEMPOOL_DIRECT_SEND_PROTOCOL)])
        .state_sync_protocols(vec![ProtocolId::from_static(
            STATE_SYNCHRONIZER_MSG_PROTOCOL,
        )])
        .direct_send_protocols(vec![
            ProtocolId::from_static(CONSENSUS_DIRECT_SEND_PROTOCOL),
            ProtocolId::from_static(MEMPOOL_DIRECT_SEND_PROTOCOL),
            ProtocolId::from_static(STATE_SYNCHRONIZER_MSG_PROTOCOL),
        ])
        .rpc_protocols(vec![ProtocolId::from_static(CONSENSUS_RPC_PROTOCOL)])
        .build();

    (
        (mempool_network_sender, mempool_network_events),
        (consensus_network_sender, consensus_network_events),
        (state_sync_network_sender, state_sync_network_events),
        runtime,
    )
}

pub fn setup_environment(node_config: &mut NodeConfig) -> (AdmissionControlClient, LibraHandle) {
    crash_handler::setup_panic_handler();

    let mut instant = Instant::now();
    let storage = start_storage_service(&node_config);
    debug!(
        "Storage service started in {} ms",
        instant.elapsed().as_millis()
    );

    instant = Instant::now();
    let execution = ServerHandle::setup(setup_executor(&node_config));
    debug!(
        "Execution service started in {} ms",
        instant.elapsed().as_millis()
    );

    instant = Instant::now();
    let (ac_server, ac_client) = setup_ac(&node_config);
    let ac = ServerHandle::setup(ac_server);
    debug!("AC started in {} ms", instant.elapsed().as_millis());

    instant = Instant::now();
    let (
        (mempool_network_sender, mempool_network_events),
        (consensus_network_sender, consensus_network_events),
        (_state_sync_network_sender, _state_sync_network_events),
        network,
    ) = setup_network(node_config);
    debug!("Network started in {} ms", instant.elapsed().as_millis());

    // TODO: Migrate to use state_sync_network_sender.
    let state_synchronizer =
        StateSynchronizer::bootstrap(consensus_network_sender.clone(), &node_config);

    let mut mempool = None;
    let mut consensus = None;
    if let RoleType::Validator = node_config.base.get_role() {
        instant = Instant::now();
        mempool = Some(MempoolRuntime::bootstrap(
            &node_config,
            mempool_network_sender,
            mempool_network_events,
        ));
        debug!("Mempool started in {} ms", instant.elapsed().as_millis());

        instant = Instant::now();
        let mut consensus_provider = make_consensus_provider(
            node_config,
            consensus_network_sender,
            consensus_network_events,
            state_synchronizer.create_client(&node_config),
        );
        consensus_provider
            .start()
            .expect("Failed to start consensus. Can't proceed.");
        consensus = Some(consensus_provider);
        debug!("Consensus started in {} ms", instant.elapsed().as_millis());
    }

    let debug_if = ServerHandle::setup(setup_debug_interface(&node_config));

    let metrics_port = node_config.debug_interface.metrics_server_port;
    let metric_host = node_config.debug_interface.address.clone();
    thread::spawn(move || metric_server::start_server((metric_host.as_str(), metrics_port)));

    let libra_handle = LibraHandle {
        _ac: ac,
        _mempool: mempool,
        _state_synchronizer: state_synchronizer,
        _network: network,
        consensus,
        _execution: execution,
        _storage: storage,
        _debug: debug_if,
    };
    (ac_client, libra_handle)
}
