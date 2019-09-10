#[macro_use]
extern crate clap;
extern crate bytes;
extern crate futures;
extern crate http;
extern crate hyper;
#[macro_use]
extern crate log;
extern crate ensicoin_messages;
extern crate ensicoin_serializer;
extern crate num_bigint;
extern crate prost;
extern crate sha2;
extern crate simplelog;
extern crate tokio;
extern crate tokio_sync;
extern crate tower_grpc;
extern crate tower_hyper;
extern crate tower_request_modifier;
extern crate tower_service;
extern crate tower_util;

mod cli;

use ensicoin_messages::resource::{
    script::{Script, OP},
    tx::{TransactionInput, TransactionOutput},
    Block, BlockHeader, Outpoint, Transaction,
};
use ensicoin_serializer::{hash_to_string, Deserialize, Deserializer, Serialize, Sha256Result};
use futures::{prelude::*, task, Future};
use hyper::client::connect::{Destination, HttpConnector};
use rpc::{BlockTemplate, Tx};
use sha2::{Digest, Sha256};
use tokio_sync::mpsc::{self, Receiver, Sender};
use tower_grpc::Request;
use tower_hyper::{client, util};
use tower_util::MakeService;

pub mod rpc {
    include!(concat!(env!("OUT_DIR"), "/ensicoin_rpc.rs"));
}

fn main() {
    let matches = cli::build_cli().get_matches();

    match simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
    ) {
        Err(_) => {
            simplelog::SimpleLogger::init(
                simplelog::LevelFilter::Info,
                simplelog::Config::default(),
            )
            .unwrap();
            ()
        }
        Ok(_) => (),
    }

    match matches.subcommand() {
        ("completions", Some(sub_matches)) => {
            let shell = sub_matches.value_of("SHELL").unwrap();

            cli::build_cli().gen_completions_to(
                "ensicoin-simon",
                shell.parse().unwrap(),
                &mut std::io::stdout(),
            );
        }
        ("", _) => {
            info!("ensicoin simon version 0.1.0");

            let uri: http::Uri = matches.value_of("uri").unwrap().parse().unwrap();
            let address: String = matches.value_of("address").unwrap().parse().unwrap();
            let address: secp256k1::PublicKey = match address.parse() {
                Ok(a) => a,
                Err(e) => {
                    warn!("Invalid address: {}", e);
                    return;
                }
            };
            use ripemd160::Ripemd160;
            let mut hasher = Ripemd160::new();
            hasher.input(&address.serialize()[..]);
            let pub_key_hash_code: Vec<_> =
                (&hasher.result()).iter().map(|b| OP::Byte(*b)).collect();

            let dst = Destination::try_from_uri(uri.clone()).unwrap();
            let connector = util::Connector::new(HttpConnector::new(4));
            let settings = client::Builder::new().http2_only(true).clone();
            let mut make_client = client::Connect::with_builder(connector, settings);

            let service = make_client
                .make_service(dst)
                .map_err(|e| {
                    panic!("HTTP/2 connection failed; err={:?}", e);
                })
                .map(move |conn| {
                    use rpc::client::Node;

                    let conn = tower_request_modifier::Builder::new()
                        .set_origin(uri)
                        .build(conn)
                        .unwrap();

                    Node::new(conn)
                })
                .and_then(|mut client| {
                    use rpc::GetBlockTemplateRequest;

                    client
                        .get_block_template(Request::new(GetBlockTemplateRequest {}))
                        .map_err(|e| panic!("gRPC request failed: {}", e))
                        .map(move |response| (client, response))
                })
                .and_then(|(mut client, response)| {
                    let (blocks_sender, blocks_receiver) = mpsc::channel(5);
                    let (results_sender, results_receiver) = mpsc::channel(5);
                    tokio::spawn(
                        results_receiver
                            .for_each(move |block: Block| {
                                info!(
                                    "block found! hash = {}",
                                    hash_to_string(&block.double_hash())
                                );

                                let raw_block = block.serialize();

                                use rpc::PublishRawBlockRequest;

                                tokio::spawn(
                                    client
                                        .publish_raw_block(Request::new(PublishRawBlockRequest {
                                            raw_block: raw_block.to_vec(),
                                        }))
                                        .map_err(|e| panic!("gRPC request failed: {}", e))
                                        .map(|_| ()),
                                );
                                Ok(())
                            })
                            .map_err(|e| panic!("miner is dead: {}", e)),
                    );
                    tokio::spawn(Miner::new(1000, blocks_receiver, results_sender));

                    let inbound = response.into_inner();

                    inbound
                        .for_each(move |reply| {
                            if let Some(bt) = reply.block_template {
                                let version = 0;
                                let flags = vec![format!("{}", bt.height)];
                                let inputs = Vec::new();
                                let value = if bt.height == 1 {
                                    0x3ffff
                                } else {
                                    0x200000000000 >> ((bt.height - 1) / 0x4000)
                                };
                                let mut script = vec![OP::Dup, OP::Hash160, OP::Push(20)];

                                script.extend_from_slice(&pub_key_hash_code);

                                script.extend_from_slice(&vec![
                                    OP::Equal,
                                    OP::Verify,
                                    OP::Checksig,
                                ]);
                                let mut txs = vec![Transaction {
                                    version,
                                    flags,
                                    inputs,
                                    outputs: vec![TransactionOutput {
                                        value,
                                        script: Script::from(script),
                                    }],
                                }];

                                for tx in reply.txs {
                                    if let Some(t) = rpc_tx_to_transaction(tx) {
                                        txs.push(t);
                                    }
                                }

                                tokio::spawn(
                                    blocks_sender
                                        .clone()
                                        .send(block_template_and_txs_to_block(bt, txs))
                                        .map(|_| ())
                                        .map_err(|e| {
                                            error!("error sending blocks to miner: {}", e)
                                        }),
                                );
                            }
                            Ok(())
                        })
                        .map_err(|e| eprintln!("gRPC inbound stream error: {:?}", e))
                })
                .map_err(|e| {
                    error!("ERR = {:?}", e);
                });

            tokio::run(service);
        }
        (_, _) => (),
    }

    info!("good bye!");
}

fn double_hash(hash_a: Sha256Result, hash_b: Sha256Result) -> Sha256Result {
    let mut hasher = Sha256::default();

    let mut vec_hash = hash_a.to_vec();
    vec_hash.extend_from_slice(&hash_b);
    hasher.input(vec_hash);
    let first = hasher.result();
    hasher = Sha256::default();
    hasher.input(first);

    hasher.result()
}

fn merkle_root(mut resources: Vec<Sha256Result>) -> Sha256Result {
    if resources.len() == 0 {
        return Sha256Result::from([0; 32]);
    };

    if resources.len() == 1 {
        resources.push(resources[0]);
    };

    while resources.len() > 1 {
        if resources.len() % 2 != 0 {
            resources.push(resources.last().unwrap().clone());
        }

        // TODO: inplace
        let mut left_hash = resources[0];
        for i in 0..resources.len() {
            let hash = resources[i];

            if i % 2 == 0 {
                left_hash = hash;
            } else {
                resources[((i + 1) / 2) - 1] = double_hash(left_hash, hash);
            }
        }

        resources.split_off(resources.len() / 2);
    }

    resources[0]
}

fn rpc_tx_to_transaction(tx: Tx) -> Option<Transaction> {
    let mut inputs = Vec::new();

    for input in tx.inputs {
        let previous_output = match input.previous_output {
            Some(p) => Outpoint {
                hash: Sha256Result::clone_from_slice(&p.hash),
                index: p.index,
            },
            None => return None,
        };

        let mut de = Deserializer::new(bytes::BytesMut::from(input.script));
        let script = match Script::deserialize(&mut de) {
            Ok(s) => s,
            Err(_) => return None,
        };

        inputs.push(TransactionInput {
            previous_output,
            script,
        });
    }

    let mut outputs = Vec::new();

    for output in tx.outputs {
        let mut de = Deserializer::new(bytes::BytesMut::from(output.script));
        let script = match Script::deserialize(&mut de) {
            Ok(s) => s,
            Err(_) => return None,
        };

        outputs.push(TransactionOutput {
            value: output.value,
            script,
        });
    }

    Some(Transaction {
        version: tx.version,
        flags: tx.flags,
        inputs,
        outputs,
    })
}

fn block_template_and_txs_to_block(block_template: BlockTemplate, txs: Vec<Transaction>) -> Block {
    Block {
        header: BlockHeader {
            version: block_template.version,
            flags: block_template.flags,
            height: block_template.height,
            timestamp: block_template.timestamp,
            target: Sha256Result::clone_from_slice(&block_template.target),
            prev_block: Sha256Result::clone_from_slice(&block_template.prev_block),
            merkle_root: merkle_root(txs.iter().map(|tx| tx.double_hash()).collect()),
            nonce: 0,
        },
        txs,
    }
}

struct Miner {
    block: Option<Block>,
    last_nonce: u64,
    step: u64,
    in_channel: Receiver<Block>,
    out_channel: Sender<Block>,
}

impl Miner {
    fn new(step: u64, in_channel: Receiver<Block>, out_channel: Sender<Block>) -> Miner {
        Miner {
            block: None,
            last_nonce: 0,
            step,
            in_channel,
            out_channel,
        }
    }

    fn mine_range(&self, from: u64, to: u64) -> Option<u64> {
        let mut block = self.block.clone().unwrap();

        for nonce in from..to {
            block.header.nonce = nonce;
            if num_bigint::BigUint::from_bytes_be(&block.double_hash())
                <= num_bigint::BigUint::from_bytes_be(&block.header.target)
            {
                return Some(nonce);
            }
        }

        None
    }
}

impl Future for Miner {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.in_channel.poll() {
                Ok(Async::Ready(Some(block))) => {
                    self.block = Some(block);
                    self.last_nonce = 0;

                    info!(
                        "mining, target is {}",
                        hash_to_string(&self.block.clone().unwrap().header.target)
                    );
                }
                Ok(Async::Ready(None)) => {
                    return Ok(Async::Ready(()));
                }
                Ok(Async::NotReady) => {
                    if self.block.is_none() {
                        return Ok(Async::NotReady);
                    }
                }
                Err(e) => {
                    error!("error reading blocks stream: {}", e);
                    return Err(());
                }
            }

            match self.mine_range(self.last_nonce, self.last_nonce + self.step) {
                None => {
                    self.last_nonce += self.step;

                    task::current().notify();

                    return Ok(Async::NotReady);
                }
                Some(nonce) => {
                    self.block
                        .as_mut()
                        .map(|mut block| block.header.nonce = nonce);

                    tokio::spawn(
                        self.out_channel
                            .clone()
                            .send(self.block.take().unwrap())
                            .map(|_| ())
                            .map_err(|e| error!("error sending result: {}", e)),
                    );
                }
            }
        }
    }
}
