use std::collections::HashMap;
use std::env::join_paths;
use std::io::{Error, ErrorKind};
use std::time;
use std::ops::FnMut;
use actix_web::dev::ResourcePath;
use crc::{Crc, CRC_32_ISCSI, Digest};
use crate::etcdserver::api::rafthttp::snap;
use lazy_static::lazy_static;
use prost::Message;
use protobuf::Message as ProtoMessage;
use slog::{info, warn};
// use proto::snappb::Snapshot;
use proto::snappb::{Snapshot as Snap};
use raft::eraftpb::Snapshot;
use crate::etcdserver::api::rafthttp::snap::default_logger;
use crate::etcdserver::api::rafthttp::util::util::write_and_sync_file;

pub type Result<T> = std::result::Result<T, Error>;
const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);
const digest: Digest<u32> =CASTAGNOLI.digest();
const snap_suffix: &str =".snap";
lazy_static! {
    static ref valid_files: HashMap<String, bool> = {
        let mut map = HashMap::new();
        map.insert(String::from("db"), true);
        map
    };
}



pub struct SnapShotter{
    dir: String,
    logger: slog::Logger
}

impl SnapShotter{
    pub fn new(dir: String, logger: slog::Logger) -> Self {
        SnapShotter{
            dir: dir,
            logger:logger
        }
    }

    pub async fn save_snap(&self, snap:&Snapshot)-> Result<()>{
        if snap.is_empty(){
            panic!("empty snapshot");
        }

        return self.save(snap).await;
    }
    async fn save(&self, snap:&Snapshot) -> Result<()>{
        let mut fname = format!("{}-{}{}", snap.get_metadata().get_index(), snap.get_metadata().get_term(), snap_suffix);

        let data = ProtoMessage::write_to_bytes(snap).unwrap();
        // let vec = serialize(snap).unwrap();
        digest.update(&data);
        let crc = digest.finalize();

        let mut snapshot = Snap::default();
        snapshot.crc = crc;
        snapshot.data = data;

        let d = ProtoMessage::write_to_bytes(&snapshot).unwrap();
        // let spath = join_paths(&[self.dir, &fname]).unwrap();
        let spath =join_paths(&[&self.dir, &fname]).unwrap();
        let fwrite = write_and_sync_file(&spath, &d, 0o600);
        match fwrite.await {
            Ok(()) => {
                println!("write_and_sync_file succeeded");
                return Ok(())
                // Do something else
            },
            Err(e) => {
                warn!(default_logger(),"write_and_sync_file failed with error: {}", e);
                return Err(e)
                // Handle the error
            }
        }

        return Ok(())
    }
    fn snap_names(&self) -> Result<Vec<String>>{
        let mut snaps = Vec::new();
        let dir = std::fs::read_dir(&self.dir)?;
        for entry in dir {
            let entry = entry?;
            let path = entry.path();
            let path_str = path.to_str().unwrap();
            if path_str.ends_with(snap_suffix){
                snaps.push(path_str.to_string());
            }
        }
        return Ok(snaps)
    }
    fn cleanup_snapdir(&self, file_names:Vec<String>) -> Result<Vec<String>>{
        let mut names = Vec::with_capacity(file_names.len());
        for name in file_names {
            if name.starts_with("db.tmp"){
                info!(default_logger(),"found orphaned defragmentation file; deleting path={}", name);
                if let Err(e) = std::fs::remove_file(&name){
                    warn!(default_logger(),"failed to remove orphaned defragmentation file"; "path" => name, "err" => ?e);
                    return Err(e);
                }
            }
            else {
                names.push(name);
            }
        }
        return Ok(names)
    }
    fn release_snapdbs(&self, snap:Snapshot) -> Result<()>{
        let mut dir = std::fs::read_dir(&self.dir)?;
        let filenames: Vec<String> = dir
            .filter_map(Result::ok)
            .filter(|dir_entry| dir_entry.file_type().is_ok())
            .map(|dir_entry| {
                let file_name = dir_entry.file_name();
                file_name.to_string_lossy().into_owned()
            })
            .collect();

        for name in filenames{
            if name.ends_with(snap_suffix){
                let hex_index = name.trim_end_matches(snap_suffix);
                let index = u64::from_str_radix(hex_index, 16).unwrap();
                if index < snap.get_metadata().get_index(){
                    info!(default_logger(),"found orphaned .snap.db file; deleting path={}", name);
                    if let Err(e) = std::fs::remove_file(&name){
                        warn!(default_logger(),"failed to remove orphaned .snap.db file"; "path" => name, "err" => ?e);
                        return Err(e);
                    }
                }
            }
        }
        return Ok(())
    }
    fn load_matching<F> (&self ,mut match_fn: F) ->Result<Snapshot>
    where F: FnMut(&Snapshot) -> bool,
    {
        let names = self.snap_names().unwrap();
        for name in names {
            if let Ok(snap) = load_snap(self.dir.clone(), name.clone()) {
                if  match_fn(&snap) {
                    return Ok(snap)
                }
            }
        }
        return Err(Error::new(ErrorKind::NotFound, "no matching snapshot found"))
    }
    fn load(&self) -> Result<Snapshot>{
        return self.load_matching(|s| return true)
    }
}

    fn read(logger: slog::Logger, snap_name: String) -> Result<Snapshot>{
        let snap = std::fs::read(snap_name.clone())?;
        if snap.is_empty() {
            warn!(logger, "failed to read empty snapshot file {}", snap_name);
        }
        let mut serializedSnap = Snap::default();
        // serializedSnap = deserialize(&snap).unwrap();
        serializedSnap = ProtoMessage::parse_from_bytes(&snap).unwrap();
        // ProtoMessage::parse_from_bytes(&snap).unwrap();


        if serializedSnap.data.len() == 0 || serializedSnap.crc == 0 {
            warn!(logger, "failed to read empty snapshot file {}", snap_name);
        }
        digest.update(&serializedSnap.data);
        let crc = digest.finalize();
        if crc != serializedSnap.crc {
            warn!(logger, "crc mismatch, want {}, got {}", serializedSnap.crc, crc);
            return Err(Error::new(std::io::ErrorKind::Other, "crc mismatch"));
        }

        let mut snap = Snapshot::default();
        snap = ProtoMessage::parse_from_bytes(&serializedSnap.data).unwrap();
        return Ok(snap);
    }

    fn check_suffix(logger: slog::Logger, names: Vec<String>) -> Result<Vec<String>>{
        let mut snaps = Vec::new();
        for name in names {
            if name.ends_with(snap_suffix){
                snaps.push(name);
            }
            else {
                if !valid_files.contains_key(&*name) {
                    warn!(logger,"found unexpected non-snap file; skipping path: {}", name)
                }
            }

        }
        return Ok(snaps);
    }

    fn load_snap(dir :String,name: String) -> Result<Snapshot>{
        let fpath = join_paths(&[&dir, &name]).unwrap();
        let fpath_str = fpath.to_str().unwrap().to_string();
        let snap = read(default_logger(), fpath.to_str().unwrap().to_string());


        if let Err(ref e) = snap{
            let mut broken_path = fpath.clone();
            broken_path.push(".broken");
            let broken_path_str = broken_path.to_str().unwrap().to_string();
            warn!(default_logger(),"failed to read a snap file"; "path" => fpath_str.clone(), "err" => ?e);
            if let Err(e) = std::fs::rename(&fpath, &broken_path){
                warn!(default_logger(),"failed to rename broken snap file"; "path" => fpath_str, "broken_path" => broken_path_str, "err" => ?e);
            }
            else {
                warn!(default_logger(),"renamed broken snap file"; "path" => fpath_str.clone(), "broken_path" => broken_path_str.clone()); }
        }
        return Ok(snap.unwrap().clone());

    }

#[cfg(test)]
mod tests{
    use std::env::join_paths;
    use lazy_static::lazy_static;
    use proto::snappb::Snapshot as Snap;
    use raft::eraftpb::{Snapshot, SnapshotMetadata};
    use raft::eraftpb::ConfState;
    use std::{env, fs};
    use std::path::PathBuf;
    use crate::etcdserver::api::rafthttp::snap::default_logger;
    use crate::etcdserver::api::rafthttp::snap::snap_shotter::SnapShotter;
    lazy_static!(
        static ref TEST_SNAP: Snapshot = {
            let mut snap = Snapshot::default();
            snap.data = Vec::from("some snapshot").into();

            let mut cs = ConfState::default();
            cs.set_voters(vec![1, 2, 3]);
            let mut mt = SnapshotMetadata::default();
            mt.set_conf_state(cs);
            mt.set_index(1);
            mt.set_term(1);
            snap.set_metadata(mt);
            snap
        };
    );

    #[tokio::test]
    async fn test_save_and_load(){
        // let mut dir = join_paths(&[&std::env::temp_dir().to_str().unwrap(), "test-snap-dir"]).unwrap()
        let mut dir = PathBuf::new();
        dir.push(env::temp_dir());
        dir.push("test-snap-dir");
        fs::create_dir(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        let ss = SnapShotter::new(dir.to_str().unwrap().to_string(), default_logger());
        ss.save(&TEST_SNAP).await.unwrap();
        ss.load().unwrap();
    }
}