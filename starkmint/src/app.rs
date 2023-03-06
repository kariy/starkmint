use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use color_eyre::Result;
use futures::{Future, FutureExt};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use tendermint::abci::request::{self, Request};
use tendermint::abci::{self, response, Response};
use tendermint::block::Height;
use tower::Service;
use tower_abci::BoxError;
use tracing::{debug, info};

use crate::transaction::{Transaction, TransactionType};

const HEIGHT_PATH: &str = "/tmp/starkmint/abci.height";

static mut TRANSACTIONS: usize = 0;
static mut TIMER: Lazy<Instant> = Lazy::new(Instant::now);

#[derive(Debug, Clone)]
pub struct StarknetApp {
    hasher: Arc<Mutex<Sha256>>,
}

impl StarknetApp {
    pub fn new() -> Self {
        std::fs::create_dir("/tmp/starkmint").expect("must be able to create temp dir");
        std::fs::write(HEIGHT_PATH, bincode::serialize(&Height::default()).unwrap()).unwrap();

        Self {
            hasher: Arc::new(Mutex::new(Sha256::new())),
        }
    }

    fn info(&self, request: request::Info) -> response::Info {
        debug!(
            "Got info request. Tendermint version: {}; Block version: {}; P2P version: {}",
            request.version, request.block_version, request.p2p_version
        );

        response::Info {
            data: "cairo-app".to_string(),
            version: "0.1.0".to_string(),
            app_version: 1,
            last_block_height: HeightFile::read_or_create(),

            // using a fixed hash, see the commit() hook
            last_block_app_hash: Default::default(),
        }
    }

    /// This hook is to query the application for data at the current or past height.
    fn query(&self, _request: request::Query) -> response::Query {
        let query_result = Err("Query hook needs implementation");

        match query_result {
            Ok(value) => response::Query {
                value,
                ..Default::default()
            },
            Err(e) => response::Query {
                code: 1.into(),
                log: format!("Error running query: {e}"),
                info: format!("Error running query: {e}"),
                ..Default::default()
            },
        }
    }

    /// This ABCI hook validates an incoming transaction before inserting it in the
    /// mempool and relaying it to other nodes.
    fn check_tx(&self, request: request::CheckTx) -> response::CheckTx {
        let tx: Transaction = bincode::deserialize(&request.tx).unwrap();

        match tx.transaction_type {
            TransactionType::FunctionExecution {
                program: _,
                function,
                program_name,
                enable_trace: _,
            } => {
                info!(
                    "Received execution transaction. Function: {}, program {}",
                    function, program_name
                );
            }
        }

        response::CheckTx {
            ..Default::default()
        }
    }

    /// This hook is called before the app starts processing transactions on a block.
    /// Used to store current proposer and the previous block's voters to assign fees and coinbase
    /// credits when the block is committed.
    fn begin_block(&self, _request: request::BeginBlock) -> response::BeginBlock {
        unsafe {
            TRANSACTIONS = 0;

            info!(
                "{} ms passed between previous begin_block() and current begin_block()",
                (*TIMER).elapsed().as_millis()
            );

            *TIMER = Instant::now();
        }

        Default::default()
    }

    /// This ABCI hook validates a transaction and applies it to the application state,
    /// for example storing the program verifying keys upon a valid deployment.
    /// Here is also where transactions are indexed for querying the blockchain.
    fn deliver_tx(&self, request: request::DeliverTx) -> response::DeliverTx {
        let tx: Transaction = bincode::deserialize(&request.tx).unwrap();

        // Validation consists of getting the hash and checking whether it is equal
        // to the tx id. The hash executes the program and hashes the trace.

        let tx_hash = tx
            .transaction_type
            .compute_and_hash()
            .map(|x| x == tx.transaction_hash);

        unsafe {
            TRANSACTIONS += 1;
        }

        match tx_hash {
            Ok(true) => {
                let _ = self
                    .hasher
                    .lock()
                    .map(|mut hash| hash.update(tx.transaction_hash.clone()));

                // prepare this transaction to be queried by app.tx_id
                let index_event = abci::Event {
                    kind: "app".to_string(),
                    attributes: vec![abci::EventAttribute {
                        index: true,
                        key: "tx_id".to_string(),
                        value: tx.transaction_hash.to_string(),
                    }],
                };
                let mut events = vec![index_event];

                match tx.transaction_type {
                    TransactionType::FunctionExecution {
                        program: _program,
                        function,
                        program_name: _,
                        enable_trace: _,
                    } => {
                        let function_event = abci::Event {
                            kind: "function".to_string(),
                            attributes: vec![abci::EventAttribute {
                                key: "function".to_string(),
                                value: function,
                                index: true,
                            }],
                        };
                        events.push(function_event);
                    }
                }

                response::DeliverTx {
                    events,
                    data: tx.transaction_hash.into(),
                    ..Default::default()
                }
            }
            Ok(false) => response::DeliverTx {
                code: 1.into(),
                log: "Error delivering transaction. Integrity check failed.".to_string(),
                info: "Error delivering transaction. Integrity check failed.".to_string(),
                ..Default::default()
            },
            Err(e) => response::DeliverTx {
                code: 1.into(),
                log: format!("Error delivering transaction: {e}"),
                info: format!("Error delivering transaction: {e}"),
                ..Default::default()
            },
        }
    }

    /// Applies validator set updates based on staking transactions included in the block.
    /// For details about validator set update semantics see:
    /// https://github.com/tendermint/tendermint/blob/v0.34.x/spec/abci/apps.md#endblock
    fn end_block(&self, _request: request::EndBlock) -> response::EndBlock {
        unsafe {
            info!(
                "Committing block with {} transactions in {} ms. TPS: {}",
                TRANSACTIONS,
                (*TIMER).elapsed().as_millis(),
                (TRANSACTIONS * 1000) as f32 / ((*TIMER).elapsed().as_millis() as f32)
            );
        }
        response::EndBlock {
            ..Default::default()
        }
    }

    /// This hook commits is called when the block is comitted (after deliver_tx has been called for each transaction).
    /// Changes to application should take effect here. Tendermint guarantees that no transaction is processed while this
    /// hook is running.
    /// The result includes a hash of the application state which will be included in the block header.
    /// This hash should be deterministic, different app state hashes will produce blockchain forks.
    /// New credits records are created to assign validator rewards.
    fn commit(&self) -> response::Commit {
        // the app hash is intended to capture the state of the application that's not contained directly
        // in the blockchain transactions (as tendermint already accounts for that with other hashes).
        // https://github.com/tendermint/tendermint/issues/1179
        // https://github.com/tendermint/tendermint/blob/v0.34.x/spec/abci/apps.md#query-proofs

        let app_hash = self
            .hasher
            .lock()
            .map(|hasher| hasher.clone().finalize().as_slice().to_vec());

        let height = HeightFile::increment();

        info!("Committing height {}", height,);

        match app_hash {
            Ok(hash) => response::Commit {
                data: hash.into(),
                retain_height: Height::default(),
            },
            // error should be handled here
            _ => response::Commit {
                data: vec![].into(),
                retain_height: Height::default(),
            },
        }
    }
}

impl Service<Request> for StarknetApp {
    type Response = Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response, BoxError>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request) -> Self::Future {
        info!(?request);

        let response = match request {
            // handled messages
            Request::Commit => Response::Commit(self.commit()),
            Request::Info(info) => Response::Info(self.info(info)),
            Request::Query(query) => Response::Query(self.query(query)),
            Request::CheckTx(check_tx) => Response::CheckTx(self.check_tx(check_tx)),
            Request::EndBlock(end_block) => Response::EndBlock(self.end_block(end_block)),
            Request::DeliverTx(deliver_tx) => Response::DeliverTx(self.deliver_tx(deliver_tx)),
            Request::BeginBlock(begin_block) => Response::BeginBlock(self.begin_block(begin_block)),

            // unhandled messages
            Request::Flush => Response::Flush,
            Request::Echo(_) => Response::Echo(Default::default()),
            Request::InitChain(_) => Response::InitChain(Default::default()),
            Request::ListSnapshots => Response::ListSnapshots(Default::default()),
            Request::OfferSnapshot(_) => Response::OfferSnapshot(Default::default()),
            Request::LoadSnapshotChunk(_) => Response::LoadSnapshotChunk(Default::default()),
            Request::ApplySnapshotChunk(_) => Response::ApplySnapshotChunk(Default::default()),
            Request::SetOption(_) => Response::SetOption(response::SetOption {
                code: 0.into(),
                log: String::from("N/A"),
                info: String::from("N/A"),
            }),
        };

        tracing::info!(?response);

        async move { Ok(response) }.boxed()
    }
}

/// Local file used to track the last block height seen by the abci application.
struct HeightFile;

impl HeightFile {
    fn read_or_create() -> Height {
        // if height file is missing or unreadable, create a new one from zero height
        if let Ok(bytes) = std::fs::read(HEIGHT_PATH) {
            // if contents are not readable, crash intentionally
            bincode::deserialize(&bytes).expect("Contents of height file are not readable")
        } else {
            let height = Height::default();
            std::fs::write(HEIGHT_PATH, bincode::serialize(&height).unwrap()).unwrap();
            height
        }
    }

    fn increment() -> Height {
        // if the file is missing or contents are unexpected, we crash intentionally;
        let height = bincode::deserialize::<Height>(&std::fs::read(HEIGHT_PATH).unwrap())
            .unwrap()
            .increment();
        std::fs::write(HEIGHT_PATH, bincode::serialize(&height).unwrap()).unwrap();
        height
    }
}
