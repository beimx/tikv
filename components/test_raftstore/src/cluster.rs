// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::path::Path;
use std::sync::{self, Arc, RwLock};
use std::time::*;
use std::{result, thread};

use futures::Future;
use kvproto::errorpb::Error as PbError;
use kvproto::metapb::{self, Peer, RegionEpoch};
use kvproto::pdpb;
use kvproto::raft_cmdpb::*;
use kvproto::raft_serverpb::{RaftApplyState, RaftMessage, RaftTruncatedState};
use tempfile::{Builder, TempDir};

use engine::rocks;
use engine::rocks::DB;
use engine::Engines;
use engine::Peekable;
use engine::CF_DEFAULT;
use tikv::config::TiKvConfig;
use tikv::pd::PdClient;
use tikv::raftstore::store::fsm::{create_raft_batch_system, RaftBatchSystem, RaftRouter};
use tikv::raftstore::store::*;
use tikv::raftstore::{Error, Result};
use tikv::server::Result as ServerResult;
use tikv_util::collections::{HashMap, HashSet};
use tikv_util::HandyRwLock;

use super::*;

// We simulate 3 or 5 nodes, each has a store.
// Sometimes, we use fixed id to test, which means the id
// isn't allocated by pd, and node id, store id are same.
// E,g, for node 1, the node id and store id are both 1.

pub trait Simulator {
    // Pass 0 to let pd allocate a node id if db is empty.
    // If node id > 0, the node must be created in db already,
    // and the node id must be the same as given argument.
    // Return the node id.
    // TODO: we will rename node name here because now we use store only.
    fn run_node(
        &mut self,
        node_id: u64,
        cfg: TiKvConfig,
        engines: Engines,
        router: RaftRouter,
        system: RaftBatchSystem,
    ) -> ServerResult<u64>;
    fn stop_node(&mut self, node_id: u64);
    fn get_node_ids(&self) -> HashSet<u64>;
    fn async_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        cb: Callback,
    ) -> Result<()>;
    fn send_raft_msg(&mut self, msg: RaftMessage) -> Result<()>;
    fn get_snap_dir(&self, node_id: u64) -> String;
    fn get_router(&self, node_id: u64) -> Option<RaftRouter>;
    fn add_send_filter(&mut self, node_id: u64, filter: Box<dyn Filter>);
    fn clear_send_filters(&mut self, node_id: u64);
    fn add_recv_filter(&mut self, node_id: u64, filter: Box<dyn Filter>);
    fn clear_recv_filters(&mut self, node_id: u64);

    fn call_command(&self, request: RaftCmdRequest, timeout: Duration) -> Result<RaftCmdResponse> {
        let node_id = request.get_header().get_peer().get_store_id();
        self.call_command_on_node(node_id, request, timeout)
    }
    fn call_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let (cb, rx) = make_cb(&request);

        match self.async_command_on_node(node_id, request, cb) {
            Ok(()) => {}
            Err(e) => {
                let mut resp = RaftCmdResponse::new();
                resp.mut_header().set_error(e.into());
                return Ok(resp);
            }
        }
        rx.recv_timeout(timeout)
            .map_err(|_| Error::Timeout(format!("request timeout for {:?}", timeout)))
    }
}

pub struct Cluster<T: Simulator> {
    pub cfg: TiKvConfig,
    leaders: HashMap<u64, metapb::Peer>,
    count: usize,

    pub paths: Vec<TempDir>,
    pub dbs: Vec<Engines>,
    pub engines: HashMap<u64, Engines>,

    pub sim: Arc<RwLock<T>>,
    pub pd_client: Arc<TestPdClient>,
}

impl<T: Simulator> Cluster<T> {
    // Create the default Store cluster.
    pub fn new(
        id: u64,
        count: usize,
        sim: Arc<RwLock<T>>,
        pd_client: Arc<TestPdClient>,
    ) -> Cluster<T> {
        // TODO: In the future, maybe it's better to test both case where `use_delete_range` is true and false
        Cluster {
            cfg: new_tikv_config(id),
            leaders: HashMap::default(),
            paths: vec![],
            dbs: vec![],
            count,
            engines: HashMap::default(),
            sim,
            pd_client,
        }
    }

    pub fn id(&self) -> u64 {
        self.cfg.server.cluster_id
    }

    pub fn create_engines(&mut self) {
        for _ in 0..self.count {
            let dir = Builder::new().prefix("test_cluster").tempdir().unwrap();
            let kv_path = dir.path().join("kv");
            let cache = self.cfg.storage.block_cache.build_shared_cache();
            let kv_db_opt = self.cfg.rocksdb.build_opt();
            let kv_cfs_opt = self.cfg.rocksdb.build_cf_opts(&cache);
            let engine = Arc::new(
                rocks::util::new_engine_opt(kv_path.to_str().unwrap(), kv_db_opt, kv_cfs_opt)
                    .unwrap(),
            );
            let raft_path = dir.path().join(Path::new("raft"));
            let raft_engine = Arc::new(
                rocks::util::new_engine(raft_path.to_str().unwrap(), None, &[CF_DEFAULT], None)
                    .unwrap(),
            );
            let engines = Engines::new(engine, raft_engine, cache.is_some());
            self.dbs.push(engines);
            self.paths.push(dir);
        }
    }

    pub fn start(&mut self) -> ServerResult<()> {
        // Try recover from last shutdown.
        let node_ids: Vec<u64> = self.engines.iter().map(|(&id, _)| id).collect();
        for node_id in node_ids {
            self.run_node(node_id)?;
        }

        // Try start new nodes.
        let mut sim = self.sim.wl();
        for _ in 0..self.count - self.engines.len() {
            let (router, system) = create_raft_batch_system(&self.cfg.raft_store);
            let (engines, path) = create_test_engine(None, router.clone(), &self.cfg);
            self.dbs.push(engines.clone());
            self.paths.push(path.unwrap());
            let node_id = sim.run_node(0, self.cfg.clone(), engines.clone(), router, system)?;
            self.engines.insert(node_id, engines);
        }
        Ok(())
    }

    pub fn compact_data(&self) {
        for engine in self.engines.values() {
            let handle = rocks::util::get_cf_handle(&engine.kv, "default").unwrap();
            rocks::util::compact_range(&engine.kv, handle, None, None, false, 1);
        }
    }

    // Bootstrap the store with fixed ID (like 1, 2, .. 5) and
    // initialize first region in all stores, then start the cluster.
    pub fn run(&mut self) {
        self.create_engines();
        self.bootstrap_region().unwrap();
        self.start().unwrap();
    }

    // Bootstrap the store with fixed ID (like 1, 2, .. 5) and
    // initialize first region in store 1, then start the cluster.
    pub fn run_conf_change(&mut self) -> u64 {
        self.create_engines();
        let region_id = self.bootstrap_conf_change();
        self.start().unwrap();
        region_id
    }

    pub fn get_node_ids(&self) -> HashSet<u64> {
        self.sim.rl().get_node_ids()
    }

    pub fn run_node(&mut self, node_id: u64) -> ServerResult<()> {
        debug!("starting node {}", node_id);
        let engines = self.engines[&node_id].clone();
        let (router, system) = create_raft_batch_system(&self.cfg.raft_store);
        debug!("calling run node"; "node_id" => node_id);
        // FIXME: rocksdb event listeners may not work, because we change the router.
        self.sim
            .wl()
            .run_node(node_id, self.cfg.clone(), engines, router, system)?;
        debug!("node {} started", node_id);
        Ok(())
    }

    pub fn stop_node(&mut self, node_id: u64) {
        debug!("stopping node {}", node_id);
        self.sim.wl().stop_node(node_id);
        debug!("node {} stopped", node_id);
    }

    pub fn get_engine(&self, node_id: u64) -> Arc<DB> {
        Arc::clone(&self.engines[&node_id].kv)
    }

    pub fn get_raft_engine(&self, node_id: u64) -> Arc<DB> {
        Arc::clone(&self.engines[&node_id].raft)
    }

    pub fn get_all_engines(&self, node_id: u64) -> Engines {
        self.engines[&node_id].clone()
    }

    pub fn send_raft_msg(&mut self, msg: RaftMessage) -> Result<()> {
        self.sim.wl().send_raft_msg(msg)
    }

    pub fn call_command_on_node(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        match self
            .sim
            .rl()
            .call_command_on_node(node_id, request.clone(), timeout)
        {
            Err(e) => {
                warn!("failed to call command {:?}: {:?}", request, e);
                Err(e)
            }
            a => a,
        }
    }

    pub fn call_command(
        &self,
        request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        match self.sim.rl().call_command(request.clone(), timeout) {
            Err(e) => {
                warn!("failed to call command {:?}: {:?}", request, e);
                Err(e)
            }
            a => a,
        }
    }

    pub fn call_command_on_leader(
        &mut self,
        mut request: RaftCmdRequest,
        timeout: Duration,
    ) -> Result<RaftCmdResponse> {
        let timer = Instant::now();
        let region_id = request.get_header().get_region_id();
        loop {
            let leader = match self.leader_of_region(region_id) {
                None => return Err(box_err!("can't get leader of region {}", region_id)),
                Some(l) => l,
            };
            request.mut_header().set_peer(leader);
            let resp = match self.call_command(request.clone(), timeout) {
                e @ Err(_) => return e,
                Ok(resp) => resp,
            };
            if self.refresh_leader_if_needed(&resp, region_id) && timer.elapsed() < timeout {
                warn!(
                    "{:?} is no longer leader, let's retry",
                    request.get_header().get_peer()
                );
                continue;
            }
            return Ok(resp);
        }
    }

    fn valid_leader_id(&self, region_id: u64, leader_id: u64) -> bool {
        let store_ids = match self.store_ids_of_region(region_id) {
            None => return false,
            Some(ids) => ids,
        };
        let node_ids = self.sim.rl().get_node_ids();
        store_ids.contains(&leader_id) && node_ids.contains(&leader_id)
    }

    fn store_ids_of_region(&self, region_id: u64) -> Option<Vec<u64>> {
        self.pd_client
            .get_region_by_id(region_id)
            .wait()
            .unwrap()
            .map(|region| region.get_peers().iter().map(Peer::get_store_id).collect())
    }

    pub fn query_leader(
        &self,
        store_id: u64,
        region_id: u64,
        timeout: Duration,
    ) -> Option<metapb::Peer> {
        // To get region leader, we don't care real peer id, so use 0 instead.
        let peer = new_peer(store_id, 0);
        let find_leader = new_status_request(region_id, peer, new_region_leader_cmd());
        let mut resp = match self.call_command(find_leader, timeout) {
            Ok(resp) => resp,
            Err(err) => {
                error!(
                    "fail to get leader of region {} on store {}, error: {:?}",
                    region_id, store_id, err
                );
                return None;
            }
        };
        let mut region_leader = resp.take_status_response().take_region_leader();
        // NOTE: node id can't be 0.
        if self.valid_leader_id(region_id, region_leader.get_leader().get_store_id()) {
            Some(region_leader.take_leader())
        } else {
            None
        }
    }

    pub fn leader_of_region(&mut self, region_id: u64) -> Option<metapb::Peer> {
        let store_ids = match self.store_ids_of_region(region_id) {
            None => return None,
            Some(ids) => ids,
        };
        if let Some(l) = self.leaders.get(&region_id) {
            // leader may be stopped in some tests.
            if self.valid_leader_id(region_id, l.get_store_id()) {
                return Some(l.clone());
            }
        }
        self.reset_leader_of_region(region_id);
        let mut leader = None;
        let mut leaders = HashMap::new();

        let node_ids = self.sim.rl().get_node_ids();
        // For some tests, we stop the node but pd still has this information,
        // and we must skip this.
        let alive_store_ids: Vec<_> = store_ids
            .iter()
            .filter(|id| node_ids.contains(id))
            .cloned()
            .collect();
        for _ in 0..500 {
            for store_id in &alive_store_ids {
                let l = match self.query_leader(*store_id, region_id, Duration::from_secs(1)) {
                    None => continue,
                    Some(l) => l,
                };
                leaders
                    .entry(l.get_id())
                    .or_insert_with(|| (l, vec![]))
                    .1
                    .push(*store_id);
            }
            if let Some((_, (l, c))) = leaders.drain().max_by_key(|(_, (_, c))| c.len()) {
                // It may be a step down leader.
                if c.contains(&l.get_store_id()) {
                    leader = Some(l);
                    if c.len() > store_ids.len() / 2 {
                        break;
                    }
                }
            }
            sleep_ms(10);
        }

        if let Some(l) = leader {
            self.leaders.insert(region_id, l);
        }

        self.leaders.get(&region_id).cloned()
    }

    pub fn check_regions_number(&self, len: u32) {
        assert_eq!(self.pd_client.get_regions_number() as u32, len)
    }

    // For test when a node is already bootstraped the cluster with the first region
    // But another node may request bootstrap at same time and get is_bootstrap false
    // Add Region but not set bootstrap to true
    pub fn add_first_region(&self) -> Result<()> {
        let mut region = metapb::Region::new();
        let region_id = self.pd_client.alloc_id().unwrap();
        let peer_id = self.pd_client.alloc_id().unwrap();
        region.set_id(region_id);
        region.set_start_key(keys::EMPTY_KEY.to_vec());
        region.set_end_key(keys::EMPTY_KEY.to_vec());
        region.mut_region_epoch().set_version(INIT_EPOCH_VER);
        region.mut_region_epoch().set_conf_ver(INIT_EPOCH_CONF_VER);
        let peer = new_peer(peer_id, peer_id);
        region.mut_peers().push(peer.clone());
        self.pd_client.add_region(&region);
        Ok(())
    }

    // Multiple nodes with fixed node id, like node 1, 2, .. 5,
    // First region 1 is in all stores with peer 1, 2, .. 5.
    // Peer 1 is in node 1, store 1, etc.
    fn bootstrap_region(&mut self) -> Result<()> {
        for (id, engines) in self.dbs.iter().enumerate() {
            let id = id as u64 + 1;
            self.engines.insert(id, engines.clone());
        }

        let mut region = metapb::Region::new();
        region.set_id(1);
        region.set_start_key(keys::EMPTY_KEY.to_vec());
        region.set_end_key(keys::EMPTY_KEY.to_vec());
        region.mut_region_epoch().set_version(INIT_EPOCH_VER);
        region.mut_region_epoch().set_conf_ver(INIT_EPOCH_CONF_VER);

        for (&id, engines) in &self.engines {
            let peer = new_peer(id, id);
            region.mut_peers().push(peer.clone());
            bootstrap_store(engines, self.id(), id).unwrap();
        }

        for engines in self.engines.values() {
            prepare_bootstrap_cluster(engines, &region)?;
        }

        self.bootstrap_cluster(region);

        Ok(())
    }

    // Return first region id.
    fn bootstrap_conf_change(&mut self) -> u64 {
        for (id, engines) in self.dbs.iter().enumerate() {
            let id = id as u64 + 1;
            self.engines.insert(id, engines.clone());
        }

        for (&id, engines) in &self.engines {
            bootstrap_store(engines, self.id(), id).unwrap();
        }

        let node_id = 1;
        let region_id = 1;
        let peer_id = 1;

        let region = initial_region(node_id, region_id, peer_id);
        prepare_bootstrap_cluster(&self.engines[&node_id], &region).unwrap();
        self.bootstrap_cluster(region);
        region_id
    }

    // This is only for fixed id test.
    fn bootstrap_cluster(&mut self, region: metapb::Region) {
        self.pd_client
            .bootstrap_cluster(new_store(1, "".to_owned()), region)
            .unwrap();

        for &id in self.engines.keys() {
            self.pd_client
                .put_store(new_store(id, "".to_owned()))
                .unwrap();
        }
    }

    pub fn reset_leader_of_region(&mut self, region_id: u64) {
        self.leaders.remove(&region_id);
    }

    pub fn assert_quorum<F: FnMut(&DB) -> bool>(&self, mut condition: F) {
        if self.engines.is_empty() {
            return;
        }
        let half = self.engines.len() / 2;
        let mut qualified_cnt = 0;
        for (id, engines) in &self.engines {
            if !condition(&engines.kv) {
                debug!("store {} is not qualified yet.", id);
                continue;
            }
            debug!("store {} is qualified", id);
            qualified_cnt += 1;
            if half < qualified_cnt {
                return;
            }
        }

        panic!(
            "need at lease {} qualified stores, but only got {}",
            half + 1,
            qualified_cnt
        );
    }

    pub fn shutdown(&mut self) {
        debug!("about to shutdown cluster");
        let keys;
        match self.sim.try_read() {
            Ok(s) => keys = s.get_node_ids(),
            Err(sync::TryLockError::Poisoned(e)) => {
                let s = e.into_inner();
                keys = s.get_node_ids();
            }
            Err(sync::TryLockError::WouldBlock) => unreachable!(),
        }
        for id in keys {
            self.stop_node(id);
        }
        self.leaders.clear();
        debug!("all nodes are shut down.");
    }

    // If the resp is "not leader error", get the real leader.
    // Sometimes, we may still can't get leader even in "not leader error",
    // returns a INVALID_PEER for this.
    fn refresh_leader_if_needed(&mut self, resp: &RaftCmdResponse, region_id: u64) -> bool {
        if !is_error_response(resp) {
            return false;
        }

        let err = resp.get_header().get_error();
        if err.has_stale_command() {
            // command got truncated, leadership may have changed.
            self.reset_leader_of_region(region_id);
            return true;
        }
        // Not match epoch can be introduced by wrong leader.
        if err.has_epoch_not_match() {
            self.reset_leader_of_region(region_id);
        }
        if !err.has_not_leader() {
            return false;
        }

        let err = err.get_not_leader();
        if !err.has_leader() {
            self.reset_leader_of_region(region_id);
            return true;
        }
        self.leaders.insert(region_id, err.get_leader().clone());
        true
    }

    pub fn request(
        &mut self,
        key: &[u8],
        reqs: Vec<Request>,
        read_quorum: bool,
        timeout: Duration,
    ) -> RaftCmdResponse {
        let timer = Instant::now();
        let mut tried_times = 0;
        while tried_times < 10 || timer.elapsed() < timeout {
            tried_times += 1;
            let mut region = self.get_region(key);
            let region_id = region.get_id();
            let req = new_request(
                region_id,
                region.take_region_epoch(),
                reqs.clone(),
                read_quorum,
            );
            let result = self.call_command_on_leader(req, timeout);

            if let Err(Error::Timeout(_)) = result {
                warn!("call command timeout, let's retry");
                sleep_ms(100);
                continue;
            }

            let resp = result.unwrap();
            if resp.get_header().get_error().has_epoch_not_match() {
                warn!("seems split, let's retry");
                sleep_ms(100);
                continue;
            }
            if resp
                .get_header()
                .get_error()
                .get_message()
                .contains("merging mode")
            {
                warn!("seems waiting for merge, let's retry");
                sleep_ms(100);
                continue;
            }
            return resp;
        }
        panic!("request timeout");
    }

    // Get region when the `filter` returns true.
    pub fn get_region_with<F>(&self, key: &[u8], filter: F) -> metapb::Region
    where
        F: Fn(&metapb::Region) -> bool,
    {
        for _ in 0..100 {
            if let Ok(region) = self.pd_client.get_region(key) {
                if filter(&region) {
                    return region;
                }
            }
            // We may meet range gap after split, so here we will
            // retry to get the region again.
            sleep_ms(20);
        }

        panic!("find no region for {}", hex::encode_upper(key));
    }

    pub fn get_region(&self, key: &[u8]) -> metapb::Region {
        self.get_region_with(key, |_| true)
    }

    pub fn get_region_id(&self, key: &[u8]) -> u64 {
        self.get_region(key).get_id()
    }

    pub fn get_down_peers(&self) -> HashMap<u64, pdpb::PeerStats> {
        self.pd_client.get_down_peers()
    }

    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_impl("default", key, false)
    }

    pub fn get_cf(&mut self, cf: &str, key: &[u8]) -> Option<Vec<u8>> {
        self.get_impl(cf, key, false)
    }

    pub fn must_get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_impl("default", key, true)
    }

    fn get_impl(&mut self, cf: &str, key: &[u8], read_quorum: bool) -> Option<Vec<u8>> {
        let mut resp = self.request(
            key,
            vec![new_get_cf_cmd(cf, key)],
            read_quorum,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CmdType::Get);
        if resp.get_responses()[0].has_get() {
            Some(resp.mut_responses()[0].mut_get().take_value())
        } else {
            None
        }
    }

    pub fn must_put(&mut self, key: &[u8], value: &[u8]) {
        self.must_put_cf("default", key, value);
    }

    pub fn must_put_cf(&mut self, cf: &str, key: &[u8], value: &[u8]) {
        let resp = self.request(
            key,
            vec![new_put_cf_cmd(cf, key, value)],
            false,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CmdType::Put);
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> result::Result<(), PbError> {
        let resp = self.request(
            key,
            vec![new_put_cf_cmd("default", key, value)],
            false,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            Err(resp.get_header().get_error().clone())
        } else {
            Ok(())
        }
    }

    pub fn must_delete(&mut self, key: &[u8]) {
        self.must_delete_cf("default", key)
    }

    pub fn must_delete_cf(&mut self, cf: &str, key: &[u8]) {
        let resp = self.request(
            key,
            vec![new_delete_cmd(cf, key)],
            false,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CmdType::Delete);
    }

    pub fn must_delete_range_cf(&mut self, cf: &str, start: &[u8], end: &[u8]) {
        let resp = self.request(
            start,
            vec![new_delete_range_cmd(cf, start, end)],
            false,
            Duration::from_secs(5),
        );
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CmdType::DeleteRange);
    }

    pub fn must_notify_delete_range_cf(&mut self, cf: &str, start: &[u8], end: &[u8]) {
        let mut req = new_delete_range_cmd(cf, start, end);
        req.mut_delete_range().set_notify_only(true);
        let resp = self.request(start, vec![req], false, Duration::from_secs(5));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CmdType::DeleteRange);
    }

    pub fn must_flush_cf(&mut self, cf: &str, sync: bool) {
        for engines in &self.dbs {
            let handle = engines.kv.cf_handle(cf).unwrap();
            engines.kv.flush_cf(handle, sync).unwrap();
        }
    }

    pub fn get_region_epoch(&self, region_id: u64) -> RegionEpoch {
        self.pd_client
            .get_region_by_id(region_id)
            .wait()
            .unwrap()
            .unwrap()
            .take_region_epoch()
    }

    pub fn region_detail(&self, region_id: u64, store_id: u64) -> RegionDetailResponse {
        let status_cmd = new_region_detail_cmd();
        let peer = new_peer(store_id, 0);
        let req = new_status_request(region_id, peer, status_cmd);
        let resp = self.call_command(req, Duration::from_secs(5));
        assert!(resp.is_ok(), "{:?}", resp);

        let mut resp = resp.unwrap();
        assert!(resp.has_status_response());
        let mut status_resp = resp.take_status_response();
        assert_eq!(status_resp.get_cmd_type(), StatusCmdType::RegionDetail);
        assert!(status_resp.has_region_detail());
        status_resp.take_region_detail()
    }

    pub fn truncated_state(&self, region_id: u64, store_id: u64) -> RaftTruncatedState {
        self.get_engine(store_id)
            .get_msg_cf::<RaftApplyState>(engine::CF_RAFT, &keys::apply_state_key(region_id))
            .unwrap()
            .unwrap()
            .take_truncated_state()
    }

    pub fn add_send_filter<F: FilterFactory>(&self, factory: F) {
        let mut sim = self.sim.wl();
        for node_id in sim.get_node_ids() {
            for filter in factory.generate(node_id) {
                sim.add_send_filter(node_id, filter);
            }
        }
    }

    pub fn transfer_leader(&mut self, region_id: u64, leader: metapb::Peer) {
        let epoch = self.get_region_epoch(region_id);
        let transfer_leader = new_admin_request(region_id, &epoch, new_transfer_leader_cmd(leader));
        let resp = self
            .call_command_on_leader(transfer_leader, Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            resp.get_admin_response().get_cmd_type(),
            AdminCmdType::TransferLeader,
            "{:?}",
            resp
        );
    }

    pub fn must_transfer_leader(&mut self, region_id: u64, leader: metapb::Peer) {
        let timer = Instant::now();
        loop {
            self.reset_leader_of_region(region_id);
            if self.leader_of_region(region_id) == Some(leader.clone()) {
                return;
            }
            if timer.elapsed() > Duration::from_secs(5) {
                panic!("failed to transfer leader to [{}] {:?}", region_id, leader);
            }
            self.transfer_leader(region_id, leader.clone());
        }
    }

    pub fn get_snap_dir(&self, node_id: u64) -> String {
        self.sim.rl().get_snap_dir(node_id)
    }

    pub fn clear_send_filters(&mut self) {
        let mut sim = self.sim.wl();
        for node_id in sim.get_node_ids() {
            sim.clear_send_filters(node_id);
        }
    }

    // It's similar to `ask_split`, the difference is the msg, it sends, is `Msg::SplitRegion`,
    // and `region` will not be embedded to that msg.
    // Caller must ensure that the `split_key` is in the `region`.
    pub fn split_region(&mut self, region: &metapb::Region, split_key: &[u8], cb: Callback) {
        let leader = self.leader_of_region(region.get_id()).unwrap();
        let router = self.sim.rl().get_router(leader.get_store_id()).unwrap();
        let split_key = split_key.to_vec();
        router
            .send(
                region.get_id(),
                PeerMsg::CasualMessage(CasualMessage::SplitRegion {
                    region_epoch: region.get_region_epoch().clone(),
                    split_keys: vec![split_key.clone()],
                    callback: cb,
                }),
            )
            .unwrap();
    }

    pub fn must_split(&mut self, region: &metapb::Region, split_key: &[u8]) {
        let mut try_cnt = 0;
        let split_count = self.pd_client.get_split_count();
        loop {
            // In case ask split message is ignored, we should retry.
            if try_cnt % 50 == 0 {
                self.reset_leader_of_region(region.get_id());
                let key = split_key.to_vec();
                let check = Box::new(move |write_resp: WriteResponse| {
                    let mut resp = write_resp.response;
                    if resp.get_header().has_error() {
                        let error = resp.get_header().get_error();
                        if error.has_epoch_not_match()
                            || error.has_not_leader()
                            || error.has_stale_command()
                        {
                            warn!("fail to split: {:?}, ignore.", error);
                            return;
                        }
                        panic!("failed to split: {:?}", resp);
                    }
                    let admin_resp = resp.mut_admin_response();
                    let split_resp = admin_resp.mut_splits();
                    let regions = split_resp.get_regions();
                    assert_eq!(regions.len(), 2);
                    assert_eq!(regions[0].get_end_key(), key.as_slice());
                    assert_eq!(regions[0].get_end_key(), regions[1].get_start_key());
                });
                self.split_region(region, split_key, Callback::Write(check));
            }

            if self.pd_client.check_split(region, split_key)
                && self.pd_client.get_split_count() > split_count
            {
                return;
            }

            if try_cnt > 250 {
                panic!(
                    "region {:?} has not been split by {}",
                    region,
                    hex::encode_upper(split_key)
                );
            }
            try_cnt += 1;
            sleep_ms(20);
        }
    }

    pub fn try_merge(&mut self, source: u64, target: u64) -> RaftCmdResponse {
        let region = self
            .pd_client
            .get_region_by_id(target)
            .wait()
            .unwrap()
            .unwrap();
        let prepare_merge = new_prepare_merge(region);
        let source = self
            .pd_client
            .get_region_by_id(source)
            .wait()
            .unwrap()
            .unwrap();
        let req = new_admin_request(source.get_id(), source.get_region_epoch(), prepare_merge);
        self.call_command_on_leader(req, Duration::from_secs(3))
            .unwrap()
    }

    /// Make sure region exists on that store.
    pub fn must_region_exist(&mut self, region_id: u64, store_id: u64) {
        let mut try_cnt = 0;
        loop {
            let find_leader =
                new_status_request(region_id, new_peer(store_id, 0), new_region_leader_cmd());
            let resp = self
                .call_command(find_leader, Duration::from_secs(5))
                .unwrap();

            if !is_error_response(&resp) {
                return;
            }

            if try_cnt > 250 {
                panic!(
                    "region {} doesn't exist on store {} after {} tries",
                    region_id, store_id, try_cnt
                );
            }
            try_cnt += 1;
            sleep_ms(20);
        }
    }

    /// Make sure region not exists on that store.
    pub fn must_region_not_exist(&mut self, region_id: u64, store_id: u64) {
        let mut try_cnt = 0;
        loop {
            let status_cmd = new_region_detail_cmd();
            let peer = new_peer(store_id, 0);
            let req = new_status_request(region_id, peer, status_cmd);
            let resp = self.call_command(req, Duration::from_secs(5)).unwrap();
            if resp.get_header().has_error() && resp.get_header().get_error().has_region_not_found()
            {
                return;
            }

            if try_cnt > 250 {
                panic!(
                    "region {} doesn't exist on store {} after {} tries: {:?}",
                    region_id, store_id, try_cnt, resp
                );
            }
            try_cnt += 1;
            sleep_ms(20);
        }
    }

    pub fn must_remove_region(&mut self, store_id: u64, region_id: u64) {
        let timer = Instant::now();
        loop {
            let peer = new_peer(store_id, 0);
            let find_leader = new_status_request(region_id, peer, new_region_leader_cmd());
            let resp = self
                .call_command(find_leader, Duration::from_secs(5))
                .unwrap();

            if is_error_response(&resp) {
                assert!(
                    resp.get_header().get_error().has_region_not_found(),
                    "unexpected error resp: {:?}",
                    resp
                );
                break;
            }
            if timer.elapsed() > Duration::from_secs(60) {
                panic!("region {} is not removed after 60s.", region_id);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    // it's so common that we provide an API for it
    pub fn partition(&self, s1: Vec<u64>, s2: Vec<u64>) {
        self.add_send_filter(PartitionFilterFactory::new(s1, s2));
    }
}

impl<T: Simulator> Drop for Cluster<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}
