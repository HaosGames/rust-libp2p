use crate::frames::{SnekBootstrap, SnekPacket, SnekSetup};
use crate::tree::Root;
use crate::{Port, SnekPathId, SNEK_EXPIRY_PERIOD};
use libp2p_core::{PeerId, PublicKey};
use std::time::SystemTime;

#[derive(PartialEq, Eq, Clone, Debug, PartialOrd, Ord, Hash)]
pub(crate) struct SnekPathIndex {
    pub(crate) public_key: PeerId,
    pub(crate) path_id: SnekPathId,
}
#[derive(PartialEq, Eq, Clone, Debug)]
pub(crate) struct SnekPath {
    pub(crate) origin: PeerId,
    pub(crate) target: PeerId,
    pub(crate) source: Port,
    pub(crate) destination: Port,
    pub(crate) last_seen: SystemTime,
    pub(crate) root: Root,
    pub(crate) active: bool,
}
impl SnekPath {
    /// `valid` returns true if the update hasn't expired, or false if it has. It is
    /// required for updates to time out eventually, in the case that paths don't get
    /// torn down properly for some reason.
    pub(crate) fn valid(&self) -> bool {
        self.last_seen.elapsed().unwrap() < SNEK_EXPIRY_PERIOD
    }
}
pub(crate) trait SnekRouted {
    fn destination_key(&self) -> PeerId;
}
impl SnekRouted for SnekPacket {
    fn destination_key(&self) -> PeerId {
        self.destination_key.to_peer_id()
    }
}
impl SnekRouted for SnekSetup {
    fn destination_key(&self) -> PeerId {
        self.destination_key.to_peer_id()
    }
}
impl SnekRouted for SnekBootstrap {
    fn destination_key(&self) -> PeerId {
        self.destination_key.to_peer_id()
    }
}
