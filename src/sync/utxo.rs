use bitcoin::consensus::encode;
use bitcoin::consensus::encode::{Decodable, Encodable};
use bitcoin::network::message;
use bitcoin::network::message::NetworkMessage;
use bitcoin::network::message_blockdata::Inventory;
use bitcoin::{Block, BlockHash};
use chrono::Utc;
use futures::sink;
use futures::sink::Sink;
use futures::stream;
use futures::stream::{Stream, StreamExt};
use futures::Future;
use rocksdb::DB;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::Barrier;
use tokio::time::{sleep, Duration};
use tokio_stream::wrappers::ReceiverStream;

use crate::cache::utxo::*;
use crate::storage::chain::*;
use crate::storage::utxo::utxo_height;
use crate::utxo::UtxoState;

use std::fmt::Debug;

/// Amount of blocks to process in parallel
pub const PARALLEL_BLOCK: usize = 500;

/// Future blocks until utxo height == chain height
pub async fn wait_utxo_sync(db: Arc<DB>, dur: Duration) {
    loop {
        let utxo_h = utxo_height(&db);
        let chain_h = get_chain_height(&db);
        if utxo_h == chain_h {
            break;
        } else {
            sleep(dur).await;
        }
    }
}

/// Future that blocks until current height of utxo is changed
pub async fn wait_utxo_height_changes(db: Arc<DB>, dur: Duration) {
    let start_h = utxo_height(&db);
    loop {
        let cur_h = utxo_height(&db);
        if cur_h == start_h {
            sleep(dur).await;
        } else {
            break;
        }
    }
}

pub async fn sync_utxo<T>(
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
) -> (
    impl Future<Output = ()>,
    impl Stream<Item = NetworkMessage> + Unpin,
    impl Sink<NetworkMessage, Error = encode::Error>,
)
where
    T: UtxoState + Decodable + Encodable + Clone + Debug + Sync + Send + 'static,
{
    sync_utxo_with(db, cache, |_, _| async move {}).await
}

pub async fn sync_utxo_with<T, F, U>(
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
    with: F,
) -> (
    impl Future<Output = ()>,
    impl Stream<Item = NetworkMessage> + Unpin,
    impl Sink<NetworkMessage, Error = encode::Error>,
)
where
    T: UtxoState + Decodable + Encodable + Clone + Debug + Sync + Send + 'static,
    F: FnMut(u32, &Block) -> U + Clone + Send + 'static,
    U: Future<Output = ()> + Send,
{
    const BUFFER_SIZE: usize = 100;
    let (broad_sender, _) = broadcast::channel(100);
    let (msg_sender, msg_reciver) = mpsc::channel::<NetworkMessage>(BUFFER_SIZE);
    // let with = Arc::new(with);
    let sync_future = {
        let broad_sender = broad_sender.clone();
        async move {
            wait_handshake(&broad_sender).await;
            loop {
                let utxo_h = utxo_height(&db);
                let chain_h = get_chain_height(&db);
                let barrier = Arc::new(Barrier::new(PARALLEL_BLOCK));
                println!("UTXO height {:?}, chain height {:?}", utxo_h, chain_h);
                if chain_h > utxo_h {
                    stream::iter(utxo_h + 1..chain_h + 1)
                        .for_each_concurrent(PARALLEL_BLOCK, |h| {
                            let db = db.clone();
                            let cache = cache.clone();
                            let broad_sender = broad_sender.clone();
                            let msg_sender = msg_sender.clone();
                            let with = with.clone();
                            let barrier = barrier.clone();
                            async move {
                                tokio::spawn(async move {
                                    sync_block(
                                        db.clone(),
                                        cache.clone(),
                                        h,
                                        chain_h,
                                        with,
                                        &broad_sender,
                                        &msg_sender,
                                    )
                                    .await;
                                    let res = barrier.wait().await;
                                    if res.is_leader() {
                                        finish_block(&db, &cache, h, false);
                                    }
                                    let _ = barrier.wait().await;
                                })
                                .await
                                .unwrap()
                            }
                        })
                        .await;
                }
                finish_block(&db, &cache, chain_h, true);
                chain_height_changes(&db, Duration::from_secs(10)).await;
            }
        }
    };
    let msg_stream = ReceiverStream::new(msg_reciver);
    let msg_sink = sink::unfold(broad_sender, |broad_sender, msg| async move {
        broad_sender.send(msg).unwrap_or(0);
        Ok::<_, encode::Error>(broad_sender)
    });
    (sync_future, msg_stream, msg_sink)
}

async fn sync_block<T, F, U>(
    db: Arc<DB>,
    cache: Arc<UtxoCache<T>>,
    h: u32,
    maxh: u32,
    mut with: F,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::Sender<NetworkMessage>,
) where
    T: UtxoState + Decodable + Encodable + Clone + Debug,
    F: FnMut(u32, &Block) -> U,
    U: Future<Output = ()>,
{
    let hash = get_block_hash(&db, h).unwrap();
    // println!("Requesting block {:?} and hash {:?}", h, hash);
    let block = request_block(&hash, broad_sender, msg_sender).await;
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S");
    let progress = 100.0 * h as f32 / maxh as f32;
    println!("{}: UTXO processing block {:?} ({:.2}%)", now, h, progress);
    for tx in &block.txdata {
        update_utxo_outputs(&cache, h, &block.header, &tx);
    }
    with(h, &block).await;
    for tx in block.txdata {
        update_utxo_inputs(&db, &cache, h, &tx);
    }
}

/// Ask node about block and wait for reply.
async fn request_block(
    hash: &BlockHash,
    broad_sender: &broadcast::Sender<NetworkMessage>,
    msg_sender: &mpsc::Sender<NetworkMessage>,
) -> Block {
    let block_msg = message::NetworkMessage::GetData(vec![Inventory::Block(*hash)]);
    msg_sender.send(block_msg).await.unwrap();
    let mut receiver = broad_sender.subscribe();
    let mut block = None;
    while block == None {
        let resend_future = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(resend_future);
        tokio::select!{
            _ = &mut resend_future => {
                println!("Resend request for block {:?}", hash);
                let block_msg = message::NetworkMessage::GetData(vec![Inventory::Block(*hash)]);
                msg_sender.send(block_msg).await.unwrap();
            }
            emsg = receiver.recv() => match emsg {
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let block_msg = message::NetworkMessage::GetData(vec![Inventory::Block(*hash)]);
                    msg_sender.send(block_msg).await.unwrap();
                }
                Err(e) => {
                    eprintln!("Request block {:?} failed with recv error: {:?}", hash, e);
                    panic!("Failed to request block");
                }
                Ok(msg) => match msg {
                    NetworkMessage::Block(b) if b.block_hash() == *hash => {
                        block = Some(b);
                    }
                    _ => (),
                },
            }
        }

    }
    block.unwrap()
}

async fn wait_handshake(broad_sender: &broadcast::Sender<NetworkMessage>) {
    let mut receiver = broad_sender.subscribe();
    let mut handshaked = false;
    while !handshaked {
        let msg = receiver.recv().await.unwrap();
        match msg {
            NetworkMessage::Verack => {
                handshaked = true;
            }
            _ => (),
        }
    }
}
