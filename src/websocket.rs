use super::models::{WsMessage, WsOp, WsPayload};
use super::serde_json;
use regex::Regex;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::thread;
use threadpool::ThreadPool;
use ws::{listen, CloseCode, Handler, Handshake, Message, Result, Sender};

use crate::middleware_result::MiddlewareResult;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub payload: WsPayload,
    pub data: serde_json::Value,
}

type Object = String;
type VanillaSub = HashSet<Client>;
type ObjectSubFwd = HashMap<Client, HashSet<Object>>;
type ObjectSubRvs = HashMap<Object, VanillaSub>;

#[derive(Debug)]
pub struct Subscriptions {
    pub kb_sub: VanillaSub,
    pub mb_sub: VanillaSub,
    pub tx_sub: VanillaSub,
    pub tu_sub: VanillaSub,
    pub object_subs_fwd: ObjectSubFwd,
    pub object_subs_rvs: ObjectSubRvs,
}

impl Subscriptions {
    pub fn new() -> Self {
        Self {
            kb_sub: VanillaSub::new(),
            mb_sub: VanillaSub::new(),
            tx_sub: VanillaSub::new(),
            tu_sub: VanillaSub::new(),
            object_subs_fwd: ObjectSubFwd::new(),
            object_subs_rvs: ObjectSubRvs::new(),
        }
    }

    pub fn get_subscription(&self, kind: WsPayload) -> Option<VanillaSub> {
        match kind {
            WsPayload::KeyBlocks => Some(self.kb_sub.clone()),
            WsPayload::MicroBlocks => Some(self.mb_sub.clone()),
            WsPayload::Transactions => Some(self.tx_sub.clone()),
            WsPayload::TxUpdate => Some(self.tu_sub.clone()),
            _ => None,
        }
    }

    pub fn vanilla_subscribe(&mut self, kind: &WsPayload, client: &Client) {
        match kind {
            WsPayload::KeyBlocks => self.kb_sub.insert(client.clone()),
            WsPayload::MicroBlocks => self.mb_sub.insert(client.clone()),
            WsPayload::Transactions => self.tx_sub.insert(client.clone()),
            WsPayload::TxUpdate => self.tu_sub.insert(client.clone()),
            _ => false,
        };
        debug!("Sub is {:?}", self);
    }

    pub fn vanilla_unsubscribe(&mut self, kind: &WsPayload, client: &Client) {
        match kind {
            WsPayload::KeyBlocks => self.kb_sub.remove(&client),
            WsPayload::MicroBlocks => self.mb_sub.remove(&client),
            WsPayload::Transactions => self.tx_sub.remove(&client),
            WsPayload::TxUpdate => self.tu_sub.remove(&client),
            _ => false,
        };
        debug!("Sub is {:?}", self);
    }

    pub fn object_subscribe(&mut self, client: Client, object: Object) {
        let mut v: VanillaSub = match self.object_subs_rvs.get(&object) {
            Some(x) => (*x).to_owned(),
            None => VanillaSub::new().to_owned(),
        };
        v.insert(client.clone());
        self.object_subs_rvs.insert(object.clone(), v);
        let mut objs: HashSet<Object> = match self.object_subs_fwd.get(&client) {
            Some(x) => (*x).to_owned(),
            None => HashSet::new(),
        };
        objs.insert(object);
        self.object_subs_fwd.insert(client, objs);
    }

    pub fn object_unsubscribe(&mut self, client: Client, object: Object) {
        let mut v: VanillaSub = match self.object_subs_rvs.get(&object) {
            Some(x) => (*x).to_owned(),
            None => VanillaSub::new().to_owned(),
        };
        v.remove(&client);
        self.object_subs_rvs.insert(object.clone(), v);
        let mut objs: HashSet<Object> = match self.object_subs_fwd.get(&client) {
            Some(x) => (*x).to_owned(),
            None => HashSet::new(),
        };
        objs.remove(&object);
        self.object_subs_fwd.insert(client, objs);
    }

    pub fn client_unsubscribe(&mut self, client: Client) {
        debug!("Unsubscribing client {:?}", client);
        let objs: HashSet<Object> = match self.object_subs_fwd.get(&client) {
            Some(x) => (*x).to_owned(),
            None => HashSet::new(),
        };
        for object in objs.iter() {
            self.object_unsubscribe(client.clone(), object.to_string());
        }
        for payload in &[
            WsPayload::KeyBlocks,
            WsPayload::MicroBlocks,
            WsPayload::Transactions,
            WsPayload::TxUpdate,
        ] {
            self.vanilla_unsubscribe(payload, &client);
        }
        debug!("Subs for client now {:?}", self.subs_for_client(client));
    }

    pub fn subs_for_client(&self, client: Client) -> Vec<Object> {
        let mut subs = Vec::new();
        if self.kb_sub.contains(&client) {
            subs.push(WsPayload::KeyBlocks.to_string());
        }
        if self.mb_sub.contains(&client) {
            subs.push(WsPayload::MicroBlocks.to_string());
        }
        if self.tx_sub.contains(&client) {
            subs.push(WsPayload::Transactions.to_string());
        }
        if self.tx_sub.contains(&client) {
            subs.push(WsPayload::TxUpdate.to_string());
        }
        let objs: HashSet<Object> = match self.object_subs_fwd.get(&client) {
            Some(x) => (*x).to_owned(),
            None => HashSet::new(),
        };
        for obj in objs.iter() {
            subs.push(obj.to_string());
        }
        debug!("Subs for client {:?} are {:?}", client, subs);
        subs
    }

    pub fn clients_for_object(&self, candidate: &Candidate) -> Vec<Client> {
        if let Some(vanilla_sub) = self.get_subscription(candidate.payload.clone()) {
            debug!("Found and returning sub {:?}", vanilla_sub);
            return vanilla_sub.clone().iter().map(|x| x.clone()).collect();
        }
        if candidate.payload != WsPayload::Object {
            return vec![];
        }
        // it's a tx of some kind
        let objects = get_objects(candidate.data.to_string());
        debug!("Objects found: {:?}", objects);
        let mut clients: HashSet<Client> = HashSet::new();
        for object in objects.iter() {
            debug!("Checking object {:?}", object);
            if let Some(sub) = self.object_subs_rvs.get(object) {
                debug!("Found subscription {:?}", sub);
                for client in sub.iter() {
                    clients.insert(client.clone());
                }
            }
        }
        debug!("Clients with matching subscriptions: {:?}", clients);
        clients.iter().map(|x| x.clone()).collect()
    }
}

#[test]
fn test_subs() {
    let subs = Subscriptions::new();
    let sub = subs.get_subscription(WsPayload::KeyBlocks).unwrap();
    assert_eq!(sub.len(), 0);
}

lazy_static! {
    static ref SUBSCRIPTIONS: Arc<Mutex<RefCell<Subscriptions>>> =
        Arc::new(Mutex::new(RefCell::new(Subscriptions::new())));
}

pub fn subs_for_client(client: Client) -> Vec<Object> {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().subs_for_client(client)
    } else {
        Vec::new()
    }
}

pub fn vanilla_subscribe(kind: &WsPayload, client: Client) {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().vanilla_subscribe(kind, &client);
    }
}

pub fn vanilla_unsubscribe(kind: &WsPayload, client: Client) {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().vanilla_unsubscribe(kind, &client);
    }
}

pub fn object_subscribe(client: Client, object: Object) {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().object_subscribe(client, object);
    }
}

pub fn object_unsubscribe(client: Client, object: Object) {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().object_unsubscribe(client, object);
    }
}

pub fn client_unsubscribe(client: Client) {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().client_unsubscribe(client);
    }
}

pub fn clients_for_object(candidate: &Candidate) -> Vec<Client> {
    if let Ok(x) = (*SUBSCRIPTIONS).lock() {
        (*x).borrow_mut().clients_for_object(candidate)
    } else {
        error!("Error locking subscriptions");
        vec![]
    }
}

pub fn get_objects(tx: String) -> HashSet<String> {
    lazy_static! {
        static ref OBJECT_REGEX: Regex = Regex::new(
            "[a-z][a-z]_[123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz]{38,60}"
        )
        .unwrap();
    }

    OBJECT_REGEX
        .find_iter(&tx)
        .map(|mat| mat.as_str().to_string())
        .collect()
}

#[test]
fn test_get_objects() {
    let tx: serde_json::Value = serde_json::from_str(
        r#"
{
    "block_hash": "mh_7iCkawgwm9akyXaBaEgfoL2Uhgz9k5b8vbSqx97spp9Ae1mLa",
    "block_height": 85113,
    "hash": "th_2vjbhonApccV6r7PjbR6qojfa2gyZ84xzTzX37g6vXgzZ9UKUn",
    "time": 1558700970133,
    "tx": {
      "amount": 13370000000000000,
      "fee": 16900000000000,
      "nonce": 32,
      "payload": "ba_Xfbg4g==",
      "recipient_id": "ak_gxMtcfvnd7aN9XdpmdNgRRETnLL4TNQ4uJgyLzcbBFa3vx6Da",
      "sender_id": "ak_2eid5UDLCVxNvqL95p9UtHmHQKbiFQahRfoo839DeQuBo8A3Qc",
      "type": "SpendTx",
      "version": 1
    }
  }
"#,
    )
    .unwrap();
    println!("{}", tx.to_string());
    let objects = get_objects(tx.to_string());
    println!("{:?}", objects);
    assert!(objects.contains("mh_7iCkawgwm9akyXaBaEgfoL2Uhgz9k5b8vbSqx97spp9Ae1mLa"));
}

#[test]
fn test_get_objects2() {
    let tx: serde_json::Value = serde_json::from_str(
        r#"
{"fee": 452020000000000, "gas": 1579000, "type": "ContractCallTx", "nonce": 49, "amount": 0, "version": 1, "call_data": "cb_KxGyMFZfP9Js5cM=", "caller_id": "ak_UQkorD6ZG4u2Ac8J2bEGEaE5jLABvWo6VHJhRDR9N7UnWHvzb", "gas_price": 1000000000, "abi_version": 3, "contract_id": "ct_ouZib4wT9cNwgRA1pxgA63XEUd8eQRrG8PcePDEYogBc1VYTq"}
"#,
    )
    .unwrap();
    println!("{}", tx.to_string());
    let objects = get_objects(tx.to_string());
    println!("{:?}", objects);
    assert!(objects.contains("ct_ouZib4wT9cNwgRA1pxgA63XEUd8eQRrG8PcePDEYogBc1VYTq"));
}

#[derive(Clone, Debug)]
pub struct Client {
    out: Sender,
}

impl PartialEq for Client {
    fn eq(&self, other: &Client) -> bool {
        self.out.token() == other.out.token()
    }
}

impl Eq for Client {}

impl Hash for Client {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.out.token().hash(state)
    }
}

impl Handler for Client {
    fn on_close(&mut self, code: CloseCode, reason: &str) {
        debug!("WebSocket closing with code {:?} because {}", code, reason);
        client_unsubscribe(self.clone());
    }

    fn on_message(&mut self, msg: Message) -> Result<()> {
        let value: WsMessage = match unpack_message(msg.to_owned()) {
            Ok(x) => x,
            Err(err) => {
                error!("Error unpacking message: {:?}", err);
                return Err(ws::Error {
                    kind: ws::ErrorKind::Custom(Box::new(err)),
                    details: Cow::from("error unpacking message"),
                });
            }
        };
        debug!("Value is {:?}", value);
        match value.op {
            Some(WsOp::Subscribe) => {
                debug!("Subscription with payload {:?}", value.payload);
                match value.payload {
                    Some(WsPayload::KeyBlocks)
                    | Some(WsPayload::MicroBlocks)
                    | Some(WsPayload::Transactions)
                    | Some(WsPayload::TxUpdate) => {
                        vanilla_subscribe(&value.payload.unwrap(), self.clone())
                    }
                    Some(WsPayload::Object) => {
                        if let Some(target) = value.target {
                            object_subscribe(self.clone(), target)
                        }
                    }
                    _ => (),
                }
            }
            Some(WsOp::Unsubscribe) => {
                debug!("Unsubscription with payload {:?}", value.payload);
                match value.payload {
                    Some(WsPayload::KeyBlocks)
                    | Some(WsPayload::MicroBlocks)
                    | Some(WsPayload::Transactions)
                    | Some(WsPayload::TxUpdate) => {
                        vanilla_unsubscribe(&value.payload.unwrap(), self.clone())
                    }
                    Some(WsPayload::Object) => {
                        if let Some(target) = value.target {
                            object_unsubscribe(self.clone(), target)
                        }
                    }
                    _ => (),
                }
            }
            _ => (),
        }
        self.out
            .send(json!(subs_for_client(self.clone())).to_string())?;
        Ok(())
    }

    fn on_open(&mut self, _shake: Handshake) -> Result<()> {
        debug!("Client connected pre");
        self.out.send("connected")?;
        debug!("Returning");
        Ok(())
    }
}

pub fn unpack_message(msg: Message) -> MiddlewareResult<WsMessage> {
    debug!("Received message {:?}", msg);
    let value = msg.into_text()?;
    Ok(serde_json::from_str(&value)?)
}

pub fn start_ws() {
    let _server = thread::spawn(move || {
        let ws_address = env::var("WEBSOCKET_ADDRESS").unwrap_or("0.0.0.0:3020".to_string());
        listen(ws_address, |out| Client { out }).expect("Unable to start the websocket server");
    });
}

/*
 * A thread pool for sending data to the websocket clients. The goal is to prevent the
 * loader thread from blocking. This is a simple solution, may not be the best though. In particular
 * a blocked client can still take up several threads before it's removed. A queue per websocket
 * connection may be better? Also, is 20 the right number of threads? Who knows?
 */
lazy_static! {
    pub static ref WS_THREADPOOL: Arc<Mutex<ThreadPool>> =
        { Arc::new(Mutex::new(ThreadPool::new(20))) };
}

/*
 * The function which actually sends the data to clients
 *
 * everything is wrapped in a JSON object with details of the
 * subscription to which it relates.
 */
pub fn broadcast_ws(candidate: &Candidate) -> MiddlewareResult<()> {
    debug!("Broadcasting candidate {:?}", candidate);
    for client in clients_for_object(candidate) {
        let _candidate = candidate.clone();
        if let Ok(threadpool) = (*WS_THREADPOOL).lock() {
            threadpool.execute(move || {
                match client.out.send(
                    json!({
                        "subscription": _candidate.payload.to_string(),
                        "payload": _candidate.data,
                    })
                    .to_string(),
                ) {
                    Ok(_) => (),
                    Err(e) => error!("Error sending data to client {:?}", e),
                };
            });
        } else {
            error!("Error locking threadpool");
        }
    }
    Ok(())
}

#[test]
fn test_unpack_message() {
    let msg: Message = Message::from(r#"{"op":"Subscribe", "payload": "MicroBlocks"}"#.to_string());
    let ws_msg = unpack_message(msg).unwrap();
    assert_eq!(ws_msg.op.unwrap(), WsOp::Subscribe);
    assert_eq!(ws_msg.payload.unwrap(), WsPayload::MicroBlocks);
    assert_eq!(ws_msg.target, None);

    let msg = Message::from(r#"{"op":"Subscribe", "payload": "Object", "target": "ak_2eid5UDLCVxNvqL95p9UtHmHQKbiFQahRfoo839DeQuBo8A3Qc"}"#.to_string());
    let ws_msg = unpack_message(msg).unwrap();
    assert_eq!(ws_msg.op.unwrap(), WsOp::Subscribe);
    assert_eq!(ws_msg.payload.unwrap(), WsPayload::Object);
    assert_eq!(
        ws_msg.target.unwrap(),
        String::from("ak_2eid5UDLCVxNvqL95p9UtHmHQKbiFQahRfoo839DeQuBo8A3Qc")
    );
}
