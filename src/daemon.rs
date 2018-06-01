use base64;
use bitcoin::blockdata::block::{Block, BlockHeader};
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::network::serialize::BitcoinHash;
use bitcoin::network::serialize::{deserialize, serialize};
use bitcoin::util::hash::Sha256dHash;
use hex;
use serde_json::{from_str, from_value, Value};
use std::env::home_dir;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use util::{HeaderEntry, HeaderList};

use errors::*;

#[derive(Debug, Copy, Clone)]
pub enum Network {
    Mainnet,
    Testnet,
}

fn read_cookie(network: Network) -> Result<Vec<u8>> {
    let mut path = home_dir().unwrap();
    path.push(".bitcoin");
    if let Network::Testnet = network {
        path.push("testnet3");
    }
    path.push(".cookie");
    fs::read(&path).chain_err(|| format!("failed to read cookie from {:?}", path))
}

fn parse_hash(value: &Value) -> Result<Sha256dHash> {
    Ok(
        Sha256dHash::from_hex(value.as_str().chain_err(|| "non-string value")?)
            .chain_err(|| "non-hex value")?,
    )
}

pub struct Daemon {
    addr: String,
    cookie_b64: String,
}

pub struct MempoolEntry {
    fee: u64,   // in satoshis
    vsize: u32, // in virtual bytes (= weight/4)
    fee_per_vbyte: f32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BlockchainInfo {
    chain: String,
    blocks: usize,
    headers: usize,
    bestblockhash: String,
    size_on_disk: usize,
    pruned: bool,
}

impl MempoolEntry {
    fn new(fee: u64, vsize: u32) -> MempoolEntry {
        MempoolEntry {
            fee,
            vsize,
            fee_per_vbyte: fee as f32 / vsize as f32,
        }
    }

    pub fn fee_per_vbyte(&self) -> f32 {
        self.fee_per_vbyte
    }

    pub fn fee(&self) -> u64 {
        self.fee
    }

    pub fn vsize(&self) -> u32 {
        self.vsize
    }
}

impl Daemon {
    pub fn new(network: Network) -> Result<Daemon> {
        Ok(Daemon {
            addr: match network {
                Network::Mainnet => "localhost:8332",
                Network::Testnet => "localhost:18332",
            }.to_string(),
            cookie_b64: base64::encode(&read_cookie(network)?),
        })
    }

    fn call_jsonrpc(&self, request: &Value) -> Result<Value> {
        let mut conn = TcpStream::connect(&self.addr)
            .chain_err(|| format!("failed to connect to {}", self.addr))?;
        let request = request.to_string();
        let msg = format!(
            "POST / HTTP/1.1\nAuthorization: Basic {}\nContent-Length: {}\n\n{}",
            self.cookie_b64,
            request.len(),
            request,
        );
        conn.write_all(msg.as_bytes())
            .chain_err(|| "failed to send request")?;

        let mut in_header = true;
        let mut contents: Option<String> = None;
        for line in BufReader::new(conn).lines() {
            let line = line.chain_err(|| "failed to read")?;
            if line.is_empty() {
                in_header = false;
            } else if !in_header {
                contents = Some(line);
                break;
            }
        }
        let contents = contents.chain_err(|| "no reply")?;
        let reply: Value = from_str(&contents).chain_err(|| "invalid JSON")?;
        Ok(reply)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let req = json!({"method": method, "params": params});
        let mut reply = self.call_jsonrpc(&req)
            .chain_err(|| format!("RPC failed: {}", req))?;
        let err = reply["error"].take();
        if !err.is_null() {
            bail!("{} RPC error: {}", method, err);
        }
        Ok(reply["result"].take())
    }

    // bitcoind JSONRPC API:

    pub fn getblockchaininfo(&self) -> Result<BlockchainInfo> {
        let info: Value = self.request("getblockchaininfo", json!([]))?;
        Ok(from_value(info).chain_err(|| "invalid blockchain info")?)
    }

    pub fn getbestblockhash(&self) -> Result<Sha256dHash> {
        parse_hash(&self.request("getbestblockhash", json!([]))?).chain_err(|| "invalid blockhash")
    }

    pub fn getblockheader(&self, blockhash: &Sha256dHash) -> Result<BlockHeader> {
        let header_hex: Value = self.request(
            "getblockheader",
            json!([blockhash.be_hex_string(), /*verbose=*/ false]),
        )?;
        Ok(deserialize(
            &hex::decode(header_hex.as_str().chain_err(|| "non-string header")?)
                .chain_err(|| "non-hex header")?,
        ).chain_err(|| format!("failed to parse blockheader {}", blockhash))?)
    }

    pub fn getblock(&self, blockhash: &Sha256dHash) -> Result<Block> {
        let block_hex: Value = self.request(
            "getblock",
            json!([blockhash.be_hex_string(), /*verbose=*/ false]),
        )?;
        let block_bytes = hex::decode(block_hex.as_str().chain_err(|| "non-string block")?)
            .chain_err(|| "non-hex block")?;
        let block: Block =
            deserialize(&block_bytes).chain_err(|| format!("failed to parse block {}", blockhash))?;
        assert_eq!(block.bitcoin_hash(), *blockhash);
        Ok(block)
    }

    pub fn gettransaction(
        &self,
        txhash: &Sha256dHash,
        blockhash: Option<Sha256dHash>,
    ) -> Result<Transaction> {
        let mut args = json!([txhash.be_hex_string(), /*verbose=*/ false]);
        if let Some(blockhash) = blockhash {
            args.as_array_mut()
                .unwrap()
                .push(json!(blockhash.be_hex_string()));
        }
        let tx_hex: Value = self.request("getrawtransaction", args)?;
        Ok(
            deserialize(&hex::decode(tx_hex.as_str().chain_err(|| "non-string tx")?)
                .chain_err(|| "non-hex tx")?)
                .chain_err(|| format!("failed to parse tx {}", txhash))?,
        )
    }

    pub fn getmempooltxids(&self) -> Result<Vec<Sha256dHash>> {
        let txids: Value = self.request("getrawmempool", json!([/*verbose=*/ false]))?;
        let mut result = vec![];
        for value in txids.as_array().chain_err(|| "non-array result")? {
            result.push(parse_hash(&value).chain_err(|| "invalid txid")?);
        }
        Ok(result)
    }

    pub fn getmempoolentry(&self, txid: &Sha256dHash) -> Result<MempoolEntry> {
        let entry = self.request("getmempoolentry", json!([txid.be_hex_string()]))?;
        let fees = entry
            .get("fees")
            .chain_err(|| "missing fees section")?
            .as_object()
            .chain_err(|| "non-object fees")?;
        let fee = (fees.get("base")
            .chain_err(|| "missing base fee")?
            .as_f64()
            .chain_err(|| "non-float fee")? * 100_000_000f64) as u64;
        let vsize = entry
            .get("size")
            .chain_err(|| "missing size")?
            .as_u64()
            .chain_err(|| "non-integer size")? as u32;
        Ok(MempoolEntry::new(fee, vsize))
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<Sha256dHash> {
        let tx = hex::encode(serialize(tx).unwrap());
        let txid = self.request("sendrawtransaction", json!([tx]))?;
        Ok(
            Sha256dHash::from_hex(txid.as_str().chain_err(|| "non-string txid")?)
                .chain_err(|| "failed to parse txid")?,
        )
    }

    pub fn get_new_headers(
        &self,
        indexed_headers: &HeaderList,
        bestblockhash: &Sha256dHash,
    ) -> Result<Vec<HeaderEntry>> {
        // Iterate back over headers until known blockash is found:
        let null_hash = Sha256dHash::default();
        let mut blockhash = *bestblockhash;
        let mut new_headers = Vec::<BlockHeader>::new();
        while blockhash != null_hash {
            if indexed_headers.header_by_blockhash(&blockhash).is_some() {
                break;
            }
            let header = self.getblockheader(&blockhash)
                .chain_err(|| format!("failed to get {} header", blockhash))?;
            trace!("downloaded {} block header", blockhash);
            new_headers.push(header);
            blockhash = header.prev_blockhash;
        }
        new_headers.reverse(); // so the tip is the last vector entry
        Ok(indexed_headers.order(new_headers))
    }
}
