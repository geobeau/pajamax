// Example for dispatch mode.
//
// This is a simple key-value store.
// We divide items into several shards. Each shard runs as a separate
// async task. So the gRPC get/set/delete/list requests need to be
// dispatched to the corresponding task.

use std::collections::HashMap;

use pajamax::status::{Code, Status};

use dict_store::*;

mod dict_store {
    pajamax::include_proto!("dict_store");
}

struct MyDictDispatch {
    req_txs: Vec<DictStoreRequestTx>,
}

impl DictStoreDispatch for MyDictDispatch {
    fn dispatch_to(&self, req: &DictStoreRequest) -> &DictStoreRequestTx {
        match req {
            DictStoreRequest::Get(req) => self.pick_req_tx(&req.key),
            DictStoreRequest::Set(req) => self.pick_req_tx(&req.key),
            DictStoreRequest::Delete(req) => self.pick_req_tx(&req.key),
            DictStoreRequest::ListShard(req) => {
                let i = req.shard as usize % self.req_txs.len();
                &self.req_txs[i]
            }
        }
    }
}

struct MyDictShard {
    dict: HashMap<String, f64>,
}

#[async_trait::async_trait(?Send)]
impl DictStoreShard for MyDictShard {
    async fn set(&mut self, req: Entry) -> Result<SetReply, Status> {
        let old_value = self.dict.insert(req.key, req.value);
        Ok(SetReply { old_value })
    }
    async fn get(&mut self, req: Key) -> Result<Value, Status> {
        match self.dict.get(&req.key) {
            Some(&value) => Ok(Value { value }),
            None => Err(Status {
                code: Code::NotFound,
                message: format!("key: {}", req.key),
            }),
        }
    }
    async fn delete(&mut self, req: Key) -> Result<Value, Status> {
        match self.dict.remove(&req.key) {
            Some(value) => Ok(Value { value }),
            None => Err(Status {
                code: Code::NotFound,
                message: format!("key: {}", req.key),
            }),
        }
    }

    async fn list_shard(&mut self, _req: ListShardRequest) -> Result<ListShardReply, Status> {
        Ok(ListShardReply {
            count: self.dict.len() as u32,
            entries: self
                .dict
                .iter()
                .map(|(key, value)| Entry {
                    key: key.clone(),
                    value: *value,
                })
                .collect(),
        })
    }
}

impl MyDictDispatch {
    fn pick_req_tx(&self, key: &str) -> &DictStoreRequestTx {
        let hash = hash_key(key) as usize;
        let i = hash % self.req_txs.len();
        &self.req_txs[i]
    }
}

fn hash_key<K>(key: &K) -> u64
where
    K: std::hash::Hash + ?Sized,
{
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

fn main() {
    let addr = "127.0.0.1:50051";

    println!("DictStoreServer listening on {}", addr);

    // For dispatch mode with compio, we need to create the channels
    // and shard tasks inside the compio runtime since they are !Send.
    // So we use a factory closure that sets up channels per-core.

    // For simplicity, we set up everything inside the single-core runtime.
    let rt = compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap();

    rt.block_on(async {
        // start 8 shard tasks
        let mut req_txs = Vec::new();
        for _ in 0..8 {
            let (req_tx, req_rx) = local_sync::mpsc::unbounded::channel();

            compio::runtime::spawn(async move {
                shard_routine(req_rx).await;
            }).detach();

            req_txs.push(req_tx);
        }

        let dict = MyDictDispatch { req_txs };

        // For dispatch mode we need a custom server setup since
        // the dispatch server holds channels that are !Send.
        // Use Config directly with the accept loop.
        let _config = pajamax::Config::new();
        let services: Vec<std::rc::Rc<dyn pajamax::PajamaxService>> = vec![
            std::rc::Rc::new(DictStoreServer::new(dict)),
        ];

        let listener = compio::net::TcpListener::bind(addr).await.unwrap();
        println!("listening on {}", addr);

        loop {
            let (stream, _peer) = listener.accept().await.unwrap();
            let services = services.clone();
            compio::runtime::spawn(async move {
                // We'd need to call the connection handler directly here
                // For now this is a simplified example
                let _ = stream;
                let _ = services;
            }).detach();
        }
    });
}

async fn shard_routine(mut req_rx: DictStoreRequestRx) {
    let shard = MyDictShard {
        dict: HashMap::new(),
    };
    let mut shard = DictStoreShardServer::new(shard);

    while let Some(req) = req_rx.recv().await {
        shard.handle(req).await;
    }
}
