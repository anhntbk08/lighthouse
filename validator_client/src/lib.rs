mod attestation_service;
mod block_service;
mod cli;
mod config;
mod duties_service;
mod fork_service;
mod validator_store;

pub mod validator_directory;

pub use cli::cli_app;
pub use config::Config;

use attestation_service::{AttestationService, AttestationServiceBuilder};
use block_service::{BlockService, BlockServiceBuilder};
use clap::ArgMatches;
use config::{Config as ClientConfig, KeySource};
use duties_service::{DutiesService, DutiesServiceBuilder};
use environment::RuntimeContext;
use exit_future::Signal;
use fork_service::{ForkService, ForkServiceBuilder};
use futures::{
    future::{self, loop_fn, Loop},
    Future, IntoFuture,
};
use parking_lot::RwLock;
use remote_beacon_node::RemoteBeaconNode;
use slog::{error, info, Logger};
use slot_clock::SlotClock;
use slot_clock::SystemTimeSlotClock;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::timer::Delay;
use types::EthSpec;
use validator_store::ValidatorStore;

const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct ProductionValidatorClient<T: EthSpec> {
    context: RuntimeContext<T>,
    duties_service: DutiesService<SystemTimeSlotClock, T>,
    fork_service: ForkService<SystemTimeSlotClock, T>,
    block_service: BlockService<SystemTimeSlotClock, T>,
    attestation_service: AttestationService<SystemTimeSlotClock, T>,
    exit_signals: Arc<RwLock<Vec<Signal>>>,
}

impl<T: EthSpec> ProductionValidatorClient<T> {
    /// Instantiates the validator client, _without_ starting the timers to trigger block
    /// and attestation production.
    pub fn new_from_cli(
        context: RuntimeContext<T>,
        cli_args: &ArgMatches,
    ) -> impl Future<Item = Self, Error = String> {
        ClientConfig::from_cli(&cli_args)
            .into_future()
            .map_err(|e| format!("Unable to initialize config: {}", e))
            .and_then(|client_config| Self::new(context, client_config))
    }

    /// Instantiates the validator client, _without_ starting the timers to trigger block
    /// and attestation production.
    pub fn new(
        mut context: RuntimeContext<T>,
        client_config: ClientConfig,
    ) -> impl Future<Item = Self, Error = String> {
        let log_1 = context.log.clone();
        let log_2 = context.log.clone();
        let log_3 = context.log.clone();

        info!(
            log_1,
            "Starting validator client";
            "datadir" => format!("{:?}", client_config.data_dir),
        );

        format!(
            "{}:{}",
            client_config.server, client_config.server_http_port
        )
        .parse()
        .map_err(|e| format!("Unable to parse server address: {:?}", e))
        .into_future()
        .and_then(move |http_server_addr| {
            info!(
                log_1,
                "Beacon node connection info";
                "http_server" => format!("{}", http_server_addr),
            );

            RemoteBeaconNode::new(http_server_addr)
                .map_err(|e| format!("Unable to init beacon node http client: {}", e))
        })
        .and_then(move |beacon_node| wait_for_node(beacon_node, log_2))
        .and_then(|beacon_node| {
            beacon_node
                .http
                .spec()
                .get_eth2_config()
                .map(|eth2_config| (beacon_node, eth2_config))
                .map_err(|e| format!("Unable to read eth2 config from beacon node: {:?}", e))
        })
        .and_then(|(beacon_node, eth2_config)| {
            beacon_node
                .http
                .beacon()
                .get_genesis_time()
                .map(|genesis_time| (beacon_node, eth2_config, genesis_time))
                .map_err(|e| format!("Unable to read genesis time from beacon node: {:?}", e))
        })
        .and_then(move |(beacon_node, remote_eth2_config, genesis_time)| {
            // Do not permit a connection to a beacon node using different spec constants.
            if context.eth2_config.spec_constants != remote_eth2_config.spec_constants {
                return Err(format!(
                    "Beacon node is using an incompatible spec. Got {}, expected {}",
                    remote_eth2_config.spec_constants, context.eth2_config.spec_constants
                ));
            }

            // Note: here we just assume the spec variables of the remote node. This is very useful
            // for testnets, but perhaps a security issue when it comes to mainnet.
            //
            // A damaging attack would be for a beacon node to convince the validator client of a
            // different `SLOTS_PER_EPOCH` variable. This could result in slashable messages being
            // produced. We are safe from this because `SLOTS_PER_EPOCH` is a type-level constant
            // for Lighthouse.
            context.eth2_config = remote_eth2_config;

            let slot_clock = SystemTimeSlotClock::new(
                context.eth2_config.spec.genesis_slot,
                Duration::from_secs(genesis_time),
                Duration::from_millis(context.eth2_config.spec.milliseconds_per_slot),
            );

            let validator_store: ValidatorStore<T> = match &client_config.key_source {
                // Load pre-existing validators from the data dir.
                //
                // Use the `account_manager` to generate these files.
                KeySource::Disk => ValidatorStore::load_from_disk(
                    client_config.data_dir.clone(),
                    context.eth2_config.spec.clone(),
                    log_3.clone(),
                )?,
                // Generate ephemeral insecure keypairs for testing purposes.
                //
                // Do not use in production.
                KeySource::TestingKeypairRange(range) => {
                    ValidatorStore::insecure_ephemeral_validators(
                        range.clone(),
                        context.eth2_config.spec.clone(),
                        log_3.clone(),
                    )?
                }
            };

            info!(
                log_3,
                "Loaded validator keypair store";
                "voting_validators" => validator_store.num_voting_validators()
            );

            let duties_service = DutiesServiceBuilder::new()
                .slot_clock(slot_clock.clone())
                .validator_store(validator_store.clone())
                .beacon_node(beacon_node.clone())
                .runtime_context(context.service_context("duties"))
                .build()?;

            let fork_service = ForkServiceBuilder::new()
                .slot_clock(slot_clock.clone())
                .beacon_node(beacon_node.clone())
                .runtime_context(context.service_context("fork"))
                .build()?;

            let block_service = BlockServiceBuilder::new()
                .duties_service(duties_service.clone())
                .fork_service(fork_service.clone())
                .slot_clock(slot_clock.clone())
                .validator_store(validator_store.clone())
                .beacon_node(beacon_node.clone())
                .runtime_context(context.service_context("block"))
                .build()?;

            let attestation_service = AttestationServiceBuilder::new()
                .duties_service(duties_service.clone())
                .fork_service(fork_service.clone())
                .slot_clock(slot_clock)
                .validator_store(validator_store)
                .beacon_node(beacon_node)
                .runtime_context(context.service_context("attestation"))
                .build()?;

            Ok(Self {
                context,
                duties_service,
                fork_service,
                block_service,
                attestation_service,
                exit_signals: Arc::new(RwLock::new(vec![])),
            })
        })
    }

    pub fn start_service(&self) -> Result<(), String> {
        let duties_exit = self
            .duties_service
            .start_update_service(&self.context.eth2_config.spec)
            .map_err(|e| format!("Unable to start duties service: {}", e))?;

        self.exit_signals.write().push(duties_exit);

        let fork_exit = self
            .fork_service
            .start_update_service(&self.context.eth2_config.spec)
            .map_err(|e| format!("Unable to start fork service: {}", e))?;

        self.exit_signals.write().push(fork_exit);

        let block_exit = self
            .block_service
            .start_update_service(&self.context.eth2_config.spec)
            .map_err(|e| format!("Unable to start block service: {}", e))?;

        self.exit_signals.write().push(block_exit);

        let attestation_exit = self
            .attestation_service
            .start_update_service(&self.context.eth2_config.spec)
            .map_err(|e| format!("Unable to start attestation service: {}", e))?;

        self.exit_signals.write().push(attestation_exit);

        Ok(())
    }
}

/// Request the version from the node, looping back and trying again on failure. Exit once the node
/// has been contacted.
fn wait_for_node<E: EthSpec>(
    beacon_node: RemoteBeaconNode<E>,
    log: Logger,
) -> impl Future<Item = RemoteBeaconNode<E>, Error = String> {
    // Try to get the version string from the node, looping until success is returned.
    loop_fn(beacon_node.clone(), move |beacon_node| {
        let log = log.clone();
        beacon_node
            .clone()
            .http
            .node()
            .get_version()
            .map_err(|e| format!("{:?}", e))
            .then(move |result| {
                let future: Box<dyn Future<Item = Loop<_, _>, Error = String> + Send> = match result
                {
                    Ok(version) => {
                        info!(
                            log,
                            "Connected to beacon node";
                            "version" => version,
                        );

                        Box::new(future::ok(Loop::Break(beacon_node)))
                    }
                    Err(e) => {
                        error!(
                            log,
                            "Unable to connect to beacon node";
                            "error" => format!("{:?}", e),
                        );

                        Box::new(
                            Delay::new(Instant::now() + RETRY_DELAY)
                                .map_err(|e| format!("Failed to trigger delay: {:?}", e))
                                .and_then(|_| future::ok(Loop::Continue(beacon_node))),
                        )
                    }
                };

                future
            })
    })
    .map(|_| beacon_node)
}
