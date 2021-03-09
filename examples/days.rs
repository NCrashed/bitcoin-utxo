// The example calculates average days UTXO set left unspend
extern crate bitcoin;
extern crate bitcoin_utxo;

use futures::pin_mut;
use futures::SinkExt;
use futures::stream;

use rocksdb::DB;

use std::{env, process};
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use bitcoin::{BlockHeader, Transaction};
use bitcoin::consensus::encode;
use bitcoin::consensus::encode::{Decodable, Encodable};
use bitcoin::network::constants;

use bitcoin_utxo::cache::utxo::new_cache;
use bitcoin_utxo::connection::connect;
use bitcoin_utxo::storage::init_storage;
use bitcoin_utxo::sync::headers::sync_headers;
use bitcoin_utxo::sync::utxo::sync_utxo;
use bitcoin_utxo::utxo::UtxoState;

#[derive(Debug, Copy, Clone)]
struct DaysCoin {
    created: u32,
}

impl UtxoState for DaysCoin {
    fn new_utxo(_height: u32, header: &BlockHeader, _tx: &Transaction, _vout: u32) -> Self {
        DaysCoin {
            created: header.time,
        }
    }
}

impl Encodable for DaysCoin {
    fn consensus_encode<W: io::Write>(&self, writer: W) -> Result<usize, io::Error> {
        let len = self.created.consensus_encode(writer)?;
        Ok(len)
    }
}
impl Decodable for DaysCoin {
     fn consensus_decode<D: io::Read>(mut d: D) -> Result<Self, encode::Error> {
         Ok(DaysCoin {
             created: Decodable::consensus_decode(&mut d)?,
         })
     }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("not enough arguments");
        process::exit(1);
    }

    let str_address = &args[1];

    let address: SocketAddr = str_address.parse().unwrap_or_else(|error| {
        eprintln!("Error parsing address: {:?}", error);
        process::exit(1);
    });

    let db = Arc::new(init_storage("./days_utxo_db")?);
    let cache = Arc::new(new_cache::<DaysCoin>());

    let (headers_stream, headers_sink) = sync_headers(db.clone()).await;
    pin_mut!(headers_sink);
    let (sync_future, utxo_stream, utxo_sink) = sync_utxo(db.clone(), cache).await;
    pin_mut!(utxo_sink);
    let days_future = calc_days(db);


    let msg_stream = stream::select(headers_stream, utxo_stream);
    let msg_sink = headers_sink.fanout(utxo_sink);
    let conn_future = connect(
        &address,
        constants::Network::Bitcoin,
        "rust-client".to_string(),
        0,
        msg_stream,
        msg_sink,
    );

    tokio::spawn(async move {
        sync_future.await;
    });
    tokio::spawn(async move {
        days_future.await;
    });
    conn_future.await.unwrap();

    Ok(())
}

async fn calc_days(db: Arc<DB>) {

}
