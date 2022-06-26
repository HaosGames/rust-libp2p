mod coordinates;
mod frames;
mod handler;
mod protocol;
mod snek;
mod tests;
mod tree;
mod wait_timer;
mod wire_frame;

use crate::coordinates::Coordinates;
use handler::Connection;
use libp2p_core::identity::ed25519::{Keypair, PublicKey as Ed25519Pub};
use libp2p_core::{connection::ConnectionId, PeerId, PublicKey};
use libp2p_swarm::{
    ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
    NotifyHandler, PollParameters,
};
use log::{debug, info, trace};
use rand::{thread_rng, Rng};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};
use std::{
    collections::VecDeque,
    task::{Context, Poll},
};

use crate::frames::{
    Frame, SnekBootstrap, SnekBootstrapAck, SnekSetup, SnekSetupAck, SnekTeardown, TreeAnnouncement,
};
use crate::snek::{SnekPath, SnekPathIndex, SnekRouted};
use crate::tree::{Root, TreeRouted};
use crate::wait_timer::WaitTimer;

pub type Port = u64;
pub type SequenceNumber = u64;
pub type SnekPathId = u64;
pub type VerificationKey = [u8; 32];

pub(crate) const SNEK_EXPIRY_PERIOD: Duration = Duration::from_secs(60 * 60);

pub(crate) const ANNOUNCEMENT_TIMEOUT: Duration = Duration::from_secs(45 * 60); //45 min
pub(crate) const ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 min
pub(crate) const REPARENT_WAIT_TIME: Duration = Duration::from_secs(1); //   1 sec

pub struct Router {
    in_events: VecDeque<Event>,
    out_events: VecDeque<Event>,

    keypair: Keypair,
    public_key: VerificationKey,

    parent: VerificationKey,
    tree_announcements: BTreeMap<VerificationKey, TreeAnnouncement>,
    ports: BTreeMap<Port, Option<VerificationKey>>,

    sequence: SequenceNumber,
    ordering: SequenceNumber,

    announcement_timer: WaitTimer,
    reparent_timer: Option<WaitTimer>,

    ascending_path: Option<SnekPathIndex>,
    descending_path: Option<SnekPathIndex>,
    paths: BTreeMap<SnekPathIndex, SnekPath>,
    candidate: Option<SnekPath>,
}

#[derive(Debug)]
pub enum Event {
    ReceivedFrame { from: VerificationKey, frame: Frame },
    SendFrame { to: VerificationKey, frame: Frame },
    AddPeer(VerificationKey),
    RegisterPort { port: Port, of: VerificationKey },
    RemovePeer(VerificationKey),
}

impl NetworkBehaviour for Router {
    type ConnectionHandler = Connection;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        let port = self.get_new_port();
        let announcement = self.current_announcement();
        self.ports.insert(port, None);
        info!(
            "Registering new peer on port {} and sending announcement",
            port
        );
        Connection::new(self.keypair.clone(), port, announcement)
    }

    fn inject_event(
        &mut self,
        _: PeerId,
        _: ConnectionId,
        result: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        match result {
            _ => self.in_events.push_front(result),
        }
    }

    fn poll(
        &mut self,
        _: &mut Context<'_>,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        trace!("Getting polled {:?}", self.public_key);
        // Handle in_events
        if let Some(event) = self.in_events.pop_back() {
            match event {
                Event::ReceivedFrame { from, frame } => {
                    self.handle_frame(frame, from);
                }
                Event::SendFrame { to, frame } => {
                    self.out_events.push_front(Event::SendFrame { to, frame });
                }
                Event::AddPeer(from) => {
                    if let Some(port) = self.port(from) {
                        let port = port.clone();
                        trace!("Registering already existing port {} for {:?}", port, from);
                        self.out_events.push_front(Event::RegisterPort {
                            port: port.clone(),
                            of: from,
                        });
                    } else {
                        self.add(from);
                    }
                }
                Event::RegisterPort { port, of } => {
                    if let Some(registered_port) = self.port(of) {
                        if &port != registered_port {
                            self.in_events.push_front(Event::AddPeer(of));
                        }
                    } else {
                        self.in_events.push_front(Event::AddPeer(of));
                    }
                }
                Event::RemovePeer(peer) => {
                    if let Some(port) = self.port(peer).cloned() {
                        debug!("Removing peer {:?}", peer);
                        self.disconnect_port(port);
                        self.ports.insert(port, None);
                    }
                }
            }
        }

        self.maintain_tree(true);
        self.maintain_snek();

        // Handle out_events
        return if let Some(event) = self.out_events.pop_back() {
            match event {
                Event::ReceivedFrame { from, frame } => {
                    self.in_events
                        .push_front(Event::ReceivedFrame { from, frame });
                    Poll::Pending
                }
                Event::SendFrame { to, frame } => {
                    trace!("Sending {:?} to {:?}", frame, to);
                    let public_key = PublicKey::Ed25519(Ed25519Pub::decode(&to).unwrap());
                    Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id: public_key.to_peer_id(),
                        handler: NotifyHandler::Any,
                        event: Event::SendFrame { to, frame },
                    })
                }
                Event::AddPeer(from) => {
                    self.in_events.push_front(Event::AddPeer(from));
                    Poll::Pending
                }
                Event::RegisterPort { port, of } => {
                    let public_key = PublicKey::Ed25519(Ed25519Pub::decode(&of).unwrap());
                    Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id: public_key.to_peer_id(),
                        handler: NotifyHandler::Any,
                        event: Event::RegisterPort { port, of },
                    })
                }
                Event::RemovePeer(from) => {
                    self.in_events.push_front(Event::RemovePeer(from));
                    Poll::Pending
                }
            }
        } else {
            Poll::Pending
        };
    }
}

impl Router {
    pub fn new(keypair: Keypair) -> Self {
        let peer_id = keypair.public().encode();
        Self {
            in_events: Default::default(),
            out_events: Default::default(),
            keypair,
            public_key: peer_id,
            tree_announcements: Default::default(),
            ports: Default::default(),
            parent: peer_id,
            sequence: 0,
            ordering: 0,
            announcement_timer: WaitTimer::new_expired(),
            reparent_timer: None,
            ascending_path: None,
            descending_path: None,
            paths: Default::default(),
            candidate: None,
        }
    }
    pub fn add(&mut self, peer: VerificationKey) {
        let port = self.get_new_port();
        self.ports.insert(port, Some(peer));
        info!("Added peer {:?}", peer);
        self.out_events
            .push_front(Event::RegisterPort { port, of: peer });
        self.send_tree_announcement(peer, self.current_announcement());
    }
    fn get_new_port(&self) -> Port {
        for i in 1.. {
            if self.ports.contains_key(&i) {
                continue;
            } else {
                return i;
            }
        }
        unreachable!("Reached port limit of {}", Port::MAX);
    }
    fn disconnect_port(&mut self, port: Port) {
        let peer = self.get_peer_on_port(port).unwrap();
        let mut bootstrap = false;
        // Scan the local DHT table for any routes that transited this now-dead
        // peering. If we find any then we need to send teardowns in the opposite
        // direction, so that nodes further along the path will learn that the
        // path was broken.
        for (key, value) in self.paths.clone() {
            if value.destination == port || value.source == port {
                self.send_teardown_for_existing_path(port, key.public_key, key.path_id);
            }
        }

        // If the ascending path was also lost because it went via the now-dead
        // peering then clear that path (although we can't send a teardown) and
        // then bootstrap again.
        if let Some(asc) = &self.ascending_path.clone() {
            let ascending = self.paths.get(&asc).unwrap();
            if ascending.destination == port {
                self.teardown_path(0, asc.public_key, asc.path_id);
                bootstrap = true;
            }
        }

        // If the descending path was lost because it went via the now-dead
        // peering then clear that path (although we can't send a teardown) and
        // wait for another incoming setup.
        if let Some(desc) = &self.descending_path.clone() {
            let descending = self.paths.get(&desc).unwrap();
            if descending.destination == port {
                self.teardown_path(0, desc.public_key, desc.path_id);
            }
        }

        // If the peer that died was our chosen tree parent, then we will need to
        // select a new parent. If we successfully choose a new parent (as in, we
        // don't end up promoting ourselves to a root) then we will also need to
        // send a new bootstrap into the network.
        if self.parent == peer {
            bootstrap = bootstrap || self.parent_selection();
        }

        if bootstrap {
            self.bootstrap_now();
        }
    }

    fn tree_announcement(&self, of: VerificationKey) -> Option<&TreeAnnouncement> {
        self.tree_announcements.get(&of)
    }
    fn set_tree_announcement(&mut self, of: VerificationKey, announcement: TreeAnnouncement) {
        self.tree_announcements.insert(of.clone(), announcement);
    }
    fn port(&self, of: VerificationKey) -> Option<&Port> {
        for (port, peer) in &self.ports {
            if let Some(peer) = peer {
                if peer == &of {
                    return Some(&port);
                }
            }
        }
        None
    }
    fn peers(&self) -> Vec<VerificationKey> {
        let mut peers = Vec::new();
        for (_port, peer) in &self.ports {
            if let Some(peer) = peer {
                peers.push(peer.clone());
            }
        }
        peers
    }
    fn parent(&self) -> VerificationKey {
        self.parent
    }
    fn set_parent(&mut self, peer: VerificationKey) {
        info!("Setting parent to {:?}", peer);
        self.parent = peer;
    }
    fn keypair(&self) -> Keypair {
        self.keypair.clone()
    }
    fn public_key(&self) -> VerificationKey {
        self.public_key
    }
    fn get_peer_on_port(&self, port: Port) -> Option<VerificationKey> {
        if let Some(peer) = self.ports.get(&port).cloned() {
            return peer;
        }
        None
    }
    fn current_sequence(&self) -> SequenceNumber {
        self.sequence
    }
    fn next_sequence(&mut self) -> SequenceNumber {
        self.sequence += 1;
        self.sequence
    }
    fn current_ordering(&self) -> SequenceNumber {
        self.ordering
    }
    fn next_ordering(&mut self) -> SequenceNumber {
        self.ordering += 1;
        self.ordering
    }
    fn reparent_timer_expired(&self) -> bool {
        if let Some(timer) = &self.reparent_timer {
            timer.is_expired()
        } else {
            true
        }
    }
    fn set_reparent_timer(&mut self) {
        trace!("Reparent in {:?}", REPARENT_WAIT_TIME);
        self.reparent_timer = Some(WaitTimer::new(REPARENT_WAIT_TIME));
    }
    fn announcement_timer_expired(&self) -> bool {
        self.announcement_timer.is_expired()
    }
    fn reset_announcement_timer(&mut self) {
        self.announcement_timer = WaitTimer::new(ANNOUNCEMENT_INTERVAL);
    }
    fn send_to_local(&mut self, frame: Frame) {
        let peer_id = self.public_key().clone();
        self.send(frame, peer_id);
    }
    fn send(&mut self, frame: Frame, to: VerificationKey) {
        self.out_events.push_front(Event::SendFrame { to, frame });
    }
    fn public_key_for_peer_id(&self, id: &PeerId) -> Option<VerificationKey> {
        for key in self.peers() {
            if id == &PublicKey::Ed25519(Ed25519Pub::decode(&key).unwrap()).to_peer_id() {
                return Some(key);
            }
        }
        None
    }

    fn next_tree_hop(
        &self,
        frame: &impl TreeRouted,
        from: VerificationKey,
    ) -> Option<VerificationKey> {
        if frame.destination_coordinates() == self.coordinates() {
            return Some(self.public_key());
        }
        let our_distance = frame
            .destination_coordinates()
            .distance_to(&self.coordinates());
        if our_distance == 0 {
            return Some(self.public_key());
        }

        let mut best_peer = None;
        let mut best_distance = our_distance;
        let mut best_ordering = SequenceNumber::MAX;
        for peer in self.peers() {
            if peer == from {
                continue; // don't route back where the packet came from
            }
            if let None = self.tree_announcement(peer) {
                continue; // ignore peers that haven't sent us announcements
            }
            if let Some(announcement) = self.tree_announcement(peer) {
                if !(self.current_root() == announcement.root) {
                    continue; // ignore peers that are following a different root or seq
                }

                let peer_coordinates: Coordinates = announcement.into();
                let distance_to_peer =
                    peer_coordinates.distance_to(&frame.destination_coordinates());
                if self.is_better_next_tree_hop_candidate(
                    distance_to_peer,
                    best_distance,
                    announcement.receive_order,
                    best_ordering,
                    best_peer.is_some(),
                ) {
                    best_peer = Some(peer);
                    best_distance = distance_to_peer;
                    best_ordering = announcement.receive_order;
                }
            }
        }
        best_peer
    }
    fn is_better_next_tree_hop_candidate(
        &self,
        peer_distance: usize,
        best_distance: usize,
        peer_order: SequenceNumber,
        best_order: SequenceNumber,
        candidate_exists: bool,
    ) -> bool {
        let mut better_candidate = false;
        if peer_distance < best_distance {
            // The peer is closer to the destination.
            better_candidate = true;
        } else if peer_distance > best_distance {
            // The peer is further away from the destination.
        } else if candidate_exists && peer_order < best_order {
            // The peer has a lower latency path to the root as a
            // last-resort tiebreak.
            better_candidate = true;
        }
        better_candidate
    }
    fn handle_frame(&mut self, frame: Frame, from: VerificationKey) {
        trace!("Handling Frame...");
        match frame {
            Frame::TreeRouted(packet) => {
                trace!("Frame is TreeRouted");
                if let Some(peer) = self.next_tree_hop(&packet, from) {
                    let peer = peer;
                    if peer == self.public_key() {
                        self.send_to_local(Frame::TreeRouted(packet));
                        return;
                    }
                    self.send(Frame::TreeRouted(packet), peer);
                    return;
                }
                return;
            }
            Frame::SnekRouted(packet) => {
                trace!("Frame is SnekRouted");
                if let Some(peer) = self.next_snek_hop(&packet, false, true) {
                    let peer = peer;
                    if peer == self.public_key() {
                        self.send_to_local(Frame::SnekRouted(packet));
                        return;
                    }
                    self.send(Frame::SnekRouted(packet), peer);
                    return;
                }
                return;
            }
            Frame::TreeAnnouncement(announcement) => {
                trace!("Frame is TreeAnnouncement");
                self.handle_tree_announcement(announcement, from);
            }

            Frame::SnekBootstrap(bootstrap) => {
                self.handle_snek_bootstrap(bootstrap);
            }
            Frame::SnekBootstrapACK(ack) => {
                self.handle_snek_bootstrap_ack(ack);
            }
            Frame::SnekSetup(setup) => {
                let from_port = self.port(from).unwrap().clone();
                let next_hop = self.next_tree_hop(&setup, from).unwrap();
                let next_hop_port = self.port(next_hop).unwrap().clone();
                self.handle_setup(from_port, setup, next_hop_port);
            }
            Frame::SnekSetupACK(ack) => {
                let port = self.port(from).unwrap().clone();
                self.handle_setup_ack(port, ack);
            }
            Frame::SnekTeardown(teardown) => {
                let port = self.port(from).unwrap().clone();
                self.handle_teardown(port, teardown);
            }
        }
    }
    fn handle_tree_announcement(&mut self, mut frame: TreeAnnouncement, from: VerificationKey) {
        frame.receive_time = SystemTime::now();
        frame.receive_order = self.next_ordering();

        if let Some(announcement) = self.tree_announcement(from) {
            if frame.has_same_root_key(announcement) {
                if frame.replayed_old_sequence(announcement) {
                    debug!("Announcement replayed old sequence. Dropping");
                    return;
                }
            }
        }
        trace!("Storing announcement {}", frame);
        self.set_tree_announcement(from, frame.clone());
        if !self.reparent_timer_expired() {
            debug!("Waiting to reparent");
            return;
        }
        if from == self.parent() {
            trace!("Announcement came from parent");
            if frame.is_loop_of_child(&self.public_key()) {
                // SelectNewParentWithWait
                debug!("Announcement contains loop");
                self.set_reparent_timer();
                self.become_root();
                return;
            }
            if frame.root.public_key < self.current_announcement().root.public_key {
                // SelectNewParentWithWait
                debug!("Announcement has weaker root");
                self.set_reparent_timer();
                self.become_root();
                return;
            }
            if frame.root.public_key > self.current_announcement().root.public_key {
                // AcceptUpdate
                debug!("Announcement has stronger root. Forwarding to peers");
                self.send_tree_announcements_to_all(self.current_announcement());
                return;
            }
            if frame.root.public_key == self.current_announcement().root.public_key {
                if frame.root.sequence_number > self.current_announcement().root.sequence_number {
                    // AcceptUpdate
                    trace!("Announcement has higher sequence. Forwarding to peers");
                    self.send_tree_announcements_to_all(self.current_announcement());
                    return;
                }
                // SelectNewParentWithWait
                debug!("Announcement replayed current sequence");
                self.set_reparent_timer();
                self.become_root();
                return;
            }
        } else {
            trace!("Announcement didn't come from parent");
            if frame.is_loop_of_child(&self.public_key()) {
                // DropFrame
                trace!("Announcement contains loop. Dropping");
                return;
            }
            if frame.root.public_key > self.current_announcement().root.public_key {
                // AcceptNewParent
                trace!("Announcement has stronger root. Forwarding to peers");
                self.set_parent(from.clone());
                self.send_tree_announcements_to_all(self.current_announcement());
                return;
            }
            if frame.root.public_key < self.current_announcement().root.public_key {
                // InformPeerOfStrongerRoot
                trace!("Announcement has weaker root. Sending my announcement");
                self.send_tree_announcement(from, self.current_announcement());
                return;
            }
            if frame.root.public_key == self.current_announcement().root.public_key {
                // SelectNewParent
                trace!("Announcement has same root");
                self.reparent(false);
                return;
            }
        }
    }
    fn current_announcement(&self) -> TreeAnnouncement {
        if let Some(announcement) = self.tree_announcement(self.parent()) {
            announcement.clone()
        } else {
            TreeAnnouncement {
                root: Root {
                    public_key: self.keypair().public().encode(),
                    sequence_number: self.current_sequence(),
                },
                signatures: vec![],
                receive_time: SystemTime::now(),
                receive_order: self.current_ordering(),
            }
        }
    }
    fn coordinates(&self) -> Coordinates {
        self.current_announcement().into()
    }
    fn send_tree_announcements_to_all(&mut self, announcement: TreeAnnouncement) {
        trace!("Sending tree announcements to all peers");
        for peer in self.peers() {
            self.send_tree_announcement(peer, announcement.clone());
        }
    }
    fn send_tree_announcement(&mut self, to: VerificationKey, announcement: TreeAnnouncement) {
        let port = self.port(to).unwrap();
        let signed_announcement = announcement; // .append_signature(self.keypair(), port); <- is done by the ConnectionHandler
        debug!(
            "Sending tree announcement to port {}: {}",
            port, signed_announcement
        );
        self.send(Frame::TreeAnnouncement(signed_announcement), to);
    }
    fn new_tree_announcement(&mut self) -> TreeAnnouncement {
        TreeAnnouncement {
            root: Root {
                public_key: self.keypair().public().encode(),
                sequence_number: self.next_sequence(),
            },
            signatures: vec![],
            receive_time: SystemTime::now(),
            receive_order: 0,
        }
    }
    fn parent_selection(&mut self) -> bool {
        trace!("Running parent selection...");
        if self.public_key() > self.current_root().public_key {
            debug!("My key is stronger than current root");
            self.become_root()
        }
        let mut best_root = self.current_root();
        let mut best_peer = None;
        let mut best_order = SequenceNumber::MAX;
        for peer in self.peers() {
            if let Some(announcement) = self.tree_announcement(peer) {
                if announcement.receive_time.elapsed().unwrap() > ANNOUNCEMENT_TIMEOUT {
                    continue;
                }
                if announcement.is_loop_of_child(&self.public_key()) {
                    continue;
                }
                if announcement.root > best_root {
                    best_root = announcement.root.clone();
                    best_peer = Some(peer);
                    best_order = announcement.receive_order;
                }
                if announcement.root < best_root {
                    continue;
                }
                if announcement.receive_order < best_order {
                    best_root = announcement.root.clone();
                    best_peer = Some(peer);
                    best_order = announcement.receive_order;
                }
            }
        }
        return match best_peer {
            Some(best_peer) => {
                if best_peer == self.parent() {
                    debug!("Current parent is the best available parent");
                    return false;
                }
                let best_peer = best_peer.clone();
                self.set_parent(best_peer);
                self.send_tree_announcements_to_all(self.current_announcement());
                true
            }
            None => {
                debug!("I am root");
                self.become_root();
                false
            }
        };
    }
    fn become_root(&mut self) {
        trace!("Becoming root");
        self.set_parent(self.public_key().clone());
    }
    fn reparent(&mut self, wait: bool) {
        if self.reparent_timer_expired() || !wait {
            trace!("Re-parenting");
            if self.parent_selection() {
                self.bootstrap_now();
            }
        }
    }
    fn current_root(&self) -> Root {
        self.current_announcement().root
    }
    fn maintain_tree(&mut self, wait: bool) {
        if self.i_am_root() {
            if self.announcement_timer_expired() && wait {
                self.reset_announcement_timer();
                let announcement = self.new_tree_announcement();
                self.send_tree_announcements_to_all(announcement)
            }
        }
        self.reparent(true);
    }
    fn i_am_root(&self) -> bool {
        self.public_key() == self.parent()
    }

    /// `maintain_snake` is responsible for working out if we need to send bootstraps
    /// or to clean up any old paths.
    fn maintain_snek(&mut self) {
        // Work out if we are able to bootstrap. If we are the root node then
        // we don't send bootstraps, since there's nowhere for them to go —
        // bootstraps are sent up to the next ascending node, but as the root,
        // we already have the highest key on the network.
        let root_announcement = self.current_announcement();
        let can_bootstrap = self.parent != self.public_key()
            && root_announcement.root.public_key != self.public_key();
        let mut will_bootstrap = false;

        // The ascending node is the node with the next highest key.
        if let Some(asc) = &self.ascending_path.clone() {
            let ascending = self.paths.get(&asc).unwrap().clone();
            if !ascending.valid() {
                // The ascending path entry has expired, so tear it down and then
                // see if we can bootstrap again.
                self.send_teardown_for_existing_path(0, asc.public_key, asc.path_id);
            }
            if ascending.root == root_announcement.root {
                // The ascending node was set up with a different root key or sequence
                // number. In this case, we will send another bootstrap to the remote
                // side in order to hopefully replace the path with a new one.
                will_bootstrap = can_bootstrap;
            }
        } else {
            // We don't have an ascending node at all, so if we can, we'll try
            // bootstrapping to locate it.
            will_bootstrap = can_bootstrap;
        }

        // The descending node is the node with the next lowest key.
        if let Some(desc) = &self.descending_path.clone() {
            let descending_path = self.paths.get(&desc).unwrap().clone();
            if !descending_path.valid() {
                // The descending path has expired, so tear it down and then that should
                // prompt the remote side into sending a new bootstrap to set up a new
                // path, if they are still alive.
                self.send_teardown_for_existing_path(0, desc.public_key, desc.path_id);
            }
        }

        // Clean up any paths that were installed more than 5 seconds ago but haven't
        // been activated by a setup ACK.
        for (index, path) in self.paths.clone() {
            if !path.active && path.last_seen.elapsed().unwrap() > Duration::from_secs(5) {
                self.send_teardown_for_existing_path(0, index.public_key, index.path_id);
            }
        }

        // If one of the previous conditions means that we need to bootstrap, then
        // send the actual bootstrap message into the network.
        if will_bootstrap {
            self.bootstrap_now();
        }
    }

    /// `bootstrap_now` is responsible for sending a bootstrap massage to the network
    fn bootstrap_now(&mut self) {
        trace!("Bootstrapping ...");
        // If we are the root node then there's no point in trying to bootstrap. We
        // already have the highest public key on the network so a bootstrap won't be
        // able to go anywhere in ascending order.
        if self.parent == self.public_key() {
            trace!("Not bootstrapping because I am root");
            return;
        }

        // If we already have a relationship with an ascending node and that has the
        // same root key and sequence number (i.e. nothing has changed in the tree since
        // the path was set up) then we don't need to send another bootstrap message just
        // yet. We'll either wait for the path to be torn down, expire or for the tree to
        // change.
        let announcement = self.current_announcement();
        if let Some(asc) = &self.ascending_path {
            let ascending = self.paths.get(&asc).unwrap();
            let _asc_peer = self.get_peer_on_port(ascending.source).unwrap();
            if ascending.root == announcement.root {
                trace!("Not bootstrapping because a valid ascending path is set");
                return;
            }
        }

        // Construct the bootstrap packet. We will include our root key and sequence
        // number in the update so that the remote side can determine if we are both using
        // the same root node when processing the update.
        let frame = SnekBootstrap {
            root: self.current_root(),
            destination_key: self.public_key(),
            source: self.coordinates(),
            path_id: thread_rng().gen(),
        };

        if let Some(peer) = self.next_snek_hop(&frame, true, false) {
            trace!("Bootstrapping path {} ", frame.path_id);
            self.send(Frame::SnekBootstrap(frame), peer);
        }
        trace!("Not bootstrapping because no next hop was found");
    }

    fn next_snek_hop(
        &self,
        frame: &impl SnekRouted,
        bootstrap: bool,
        traffic: bool,
    ) -> Option<VerificationKey> {
        let destination_key = frame.destination_key();
        // If the message isn't a bootstrap message and the destination is for our
        // own public key, handle the frame locally — it's basically loopback.
        if !bootstrap && self.public_key() == destination_key {
            return Some(self.public_key());
        }

        // We start off with our own key as the best key. Any suitable next-hop
        // candidate has to improve on our own key in order to forward the frame.
        let mut best_peer = None;
        if !traffic {
            best_peer = Some(self.public_key());
        }
        let mut best_key = self.public_key();

        // Check if we can use the path to the root via our parent as a starting
        // point. We can't do this if we are the root node as there would be no
        // parent or ascending paths.
        if self.parent() != self.public_key() {
            if bootstrap && best_key == destination_key {
                // Bootstraps always start working towards their root so that they
                // go somewhere rather than getting stuck.
            }
            if Self::dht_ordered(
                &best_key,
                &destination_key,
                &self.current_announcement().root.public_key,
            ) {
                // The destination key is higher than our own key, so start using
                // the path to the root as the first candidate.
                best_key = self.current_announcement().root.public_key;
                best_peer = Some(self.parent())
            }

            // Check our direct ancestors in the tree, that is, all nodes between
            // ourselves and the root node via the parent port.
            for ancestor in self
                .current_announcement()
                .signatures
                .iter()
                .map(|x| x.signing_public_key)
            {
                if !bootstrap && ancestor == destination_key && best_key != destination_key {
                    best_key = ancestor;
                    best_peer = Some(self.parent());
                }
                if Self::dht_ordered(&destination_key, &ancestor, &best_key) {
                    best_key = ancestor;
                    best_peer = Some(self.parent());
                }
            }
        }

        // Check all of the ancestors of our direct peers too, that is, all nodes
        // between our direct peer and the root node.
        for (peer, announcement) in &self.tree_announcements {
            for hop in &announcement.signatures {
                if !bootstrap
                    && hop.signing_public_key == destination_key
                    && best_key != destination_key
                {
                    best_key = hop.signing_public_key;
                    best_peer = Some(peer.clone());
                }
                if Self::dht_ordered(&destination_key, &hop.signing_public_key, &best_key) {
                    best_key = hop.signing_public_key;
                    best_peer = Some(peer.clone());
                }
            }
        }

        // Check whether our current best candidate is actually a direct peer.
        // This might happen if we spotted the node in our direct ancestors for
        // example, only in this case it would make more sense to route directly
        // to the peer via our peering with them as opposed to routing via our
        // parent port.
        for peer in self.peers() {
            if best_key == peer {
                best_key = peer;
                best_peer = Some(peer);
            }
        }

        // Check our DHT entries. In particular, we are only looking at the source
        // side of the DHT paths. Since setups travel from the lower key to the
        // higher one, this is effectively looking for paths that descend through
        // keyspace toward lower keys rather than ascend toward higher ones.
        for (key, entry) in &self.paths {
            if !entry.valid() || entry.source == 0 {
                continue;
            }
            if !bootstrap && !entry.active {
                continue;
            }
            if !bootstrap && key.public_key == destination_key && best_key != destination_key {
                best_key = key.public_key;
                best_peer = Some(self.get_peer_on_port(entry.source).unwrap());
            }
            if Self::dht_ordered(&destination_key, &key.public_key, &best_key) {
                best_key = key.public_key;
                best_peer = Some(self.get_peer_on_port(entry.source).unwrap());
            }
        }
        best_peer
    }

    /// `handle_bootstrap` is called in response to receiving a bootstrap packet.
    /// This function will send a bootstrap ACK back to the sender.
    fn handle_snek_bootstrap(&mut self, frame: SnekBootstrap) {
        // Check that the root key and sequence number in the update match our
        // current root, otherwise we won't be able to route back to them using
        // tree routing anyway. If they don't match, silently drop the bootstrap.
        if self.current_root() == frame.root {
            // In response to a bootstrap, we'll send back a bootstrap ACK packet to
            // the sender. We'll include our own root details in the ACK.
            let frame = SnekBootstrapAck {
                // Bootstrap ACKs are routed using tree routing, so we need to take the
                // coordinates from the source field of the received packet and set the
                // destination of the ACK packet to that.
                destination_coordinates: frame.source.clone(),
                destination_key: frame.destination_key,
                source_coordinates: self.coordinates(),
                source_key: self.public_key(),
                root: self.current_root(),
                path_id: frame.path_id,
            };
            if let Some(peer) = self.next_tree_hop(&frame, self.public_key()) {
                self.send(Frame::SnekBootstrapACK(frame), peer);
            }
        }
    }

    /// `handle_snek_bootstrap_ack` is called in response to receiving a bootstrap ACK
    /// packet. This function will work out whether the remote node is a suitable
    /// candidate to set up an outbound path to, and if so, will send path setup
    /// packets to the network.
    fn handle_snek_bootstrap_ack(&mut self, rx: SnekBootstrapAck) {
        let mut update = false;
        if rx.source_key == self.public_key() {
            // We received a bootstrap ACK from ourselves. This shouldn't happen,
            // so either another node has forwarded it to us incorrectly, or
            // a routing loop has occurred somewhere. Don't act on the bootstrap
            // in that case.
        } else if rx.root == self.current_root() {
            // The root key in the bootstrap ACK doesn't match our own key, or the
            // sequence doesn't match, so it is quite possible that routing setup packets
            // using tree routing would fail.
        } else if let Some(asc) = &self.ascending_path {
            if let Some(ascending) = self.paths.get(&asc) {
                if ascending.valid() {
                    // We already have an ascending entry and it hasn't expired yet.
                    if ascending.origin == rx.source_key && rx.path_id != asc.path_id {
                        // We've received another bootstrap ACK from our direct ascending node.
                        // Just refresh the record and then send a new path setup message to
                        // that node.
                        update = true
                    } else if Self::dht_ordered(
                        &self.public_key(),
                        &rx.source_key,
                        &ascending.origin,
                    ) {
                        // We know about an ascending node already but it turns out that this
                        // new node that we've received a bootstrap from is actually closer to
                        // us than the previous node. We'll update our record to use the new
                        // node instead and then send a new path setup message to it.
                        update = true;
                    }
                } else {
                    // Ascending Path expired.
                    if self.public_key() < rx.source_key {
                        // We don't know about an ascending node and at the moment we don't know
                        // any better candidates, so we'll accept a bootstrap ACK from a node with a
                        // key higher than ours (so that it matches descending order).
                        update = true;
                    }
                }
            }
        } else if None == self.ascending_path {
            // We don't have an ascending entry
            if self.public_key() < rx.source_key {
                // We don't know about an ascending node and at the moment we don't know
                // any better candidates, so we'll accept a bootstrap ACK from a node with a
                // key higher than ours (so that it matches descending order).
                update = true;
            }
        } else {
            // The bootstrap ACK conditions weren't met. This might just be because
            // there's a node out there that hasn't converged to a closer node
            // yet, so we'll just ignore the acknowledgement.
        }
        if !update {
            return;
        }
        // Setup messages routed using tree routing. The destination key is set in the
        // header so that a node can determine if the setup message arrived at the
        // intended destination instead of forwarding it. The source key is set to our
        // public key, since this is the lower of the two keys that intermediate nodes
        // will populate into their routing tables.
        let setup = SnekSetup {
            root: self.current_root(),
            destination: rx.source_coordinates,
            destination_key: rx.source_key.clone(),
            source_key: self.public_key(),
            path_id: rx.path_id,
        };
        let next_hop = self.next_tree_hop(&setup, self.public_key());

        // Importantly, we will only create a DHT entry if it appears as though our next
        // hop has actually accepted the packet. Otherwise we'll create a path entry and
        // the setup message won't go anywhere.
        match next_hop {
            None => {
                // No peer was identified, which shouldn't happen.
                return;
            }
            Some(next_peer) => {
                if self.public_key() == next_peer {
                    // The peer is local, which shouldn't happen.
                    return;
                }
                self.send(Frame::SnekSetup(setup), next_peer);
                let index = SnekPathIndex {
                    public_key: self.public_key(),
                    path_id: rx.path_id.clone(),
                };
                let entry = SnekPath {
                    origin: rx.source_key,
                    target: rx.source_key,
                    source: 0,
                    destination: self.port(next_peer).unwrap().clone(),
                    last_seen: SystemTime::now(),
                    root: rx.root.clone(),
                    active: false,
                };
                // The remote side is responsible for clearing up the replaced path, but
                // we do want to make sure we don't have any old paths to other nodes
                // that *aren't* the new ascending node lying around. This helps to avoid
                // routing loops.
                for (dht_key, entry) in &self.paths.clone() {
                    if entry.source == 0
                        && dht_key.public_key /*TODO dht_key.public_key OR entry.public_key which doesn't exist*/
                        != rx.source_key
                    {
                        self.send_teardown_for_existing_path(
                            0,
                            dht_key.public_key,
                            dht_key.path_id,
                        );
                    }
                }
                // Install the new route into the DHT.
                self.paths.insert(index, entry.clone());
                self.candidate = Some(entry);
            }
        }
    }

    /// `handle_setup` is called in response to receiving setup packets. Note that
    /// these packets are handled even as we forward them, as setup packets should be
    /// processed by each node on the path.
    fn handle_setup(&mut self, from: Port, rx: SnekSetup, next_hop: Port) {
        if self.current_root() != rx.root {
            self.send_teardown_for_rejected_path(rx.source_key, rx.path_id, from);
        }
        let index = SnekPathIndex {
            public_key: rx.source_key,
            path_id: rx.path_id,
        };
        // If we already have a path for this public key and path ID combo, which
        // *shouldn't* happen, then we need to tear down both the existing path and
        // then send back a teardown to the sender notifying them that there was a
        // problem. This will probably trigger a new setup, but that's OK, it should
        // have a new path ID.
        if self.paths.contains_key(&index) {
            self.send_teardown_for_existing_path(0, rx.source_key, rx.path_id);
            self.send_teardown_for_rejected_path(rx.source_key, rx.path_id, from);
            return;
        }
        // If we're at the destination of the setup then update our predecessor
        // with information from the bootstrap.
        if rx.destination_key == self.public_key() {
            let mut update = false;
            if self.current_root() == rx.root {
                // The root key in the bootstrap ACK doesn't match our own key, or the
                // sequence doesn't match, so it is quite possible that routing setup packets
                // using tree routing would fail.
            } else if rx.source_key < self.public_key() {
                // The bootstrapping key should be less than ours but it isn't.
            } else if let Some(desc) = &self.descending_path {
                let descending = self.paths.get(desc).unwrap();
                if descending.valid() {
                    // We already have a descending entry and it hasn't expired.
                    if desc.public_key == rx.source_key && rx.path_id != desc.path_id {
                        // We've received another bootstrap from our direct descending node.
                        // Send back an acknowledgement as this is OK.
                        update = true;
                    } else if Self::dht_ordered(
                        &desc.public_key,
                        &rx.source_key,
                        &self.public_key(),
                    ) {
                        // The bootstrapping node is closer to us than our previous descending
                        // node was.
                        update = true;
                    } else {
                        // Our descending entry has expired
                        if rx.source_key < self.public_key() {
                            // The bootstrapping key is less than ours so we'll acknowledge it.
                            update = true;
                        }
                    }
                }
            } else if let None = self.descending_path {
                // We don't have a descending entry
                if rx.source_key < self.public_key() {
                    // The bootstrapping key is less than ours so we'll acknowledge it.
                    update = true;
                }
            } else {
                // The bootstrap conditions weren't met. This might just be because
                // there's a node out there that hasn't converged to a closer node
                // yet, so we'll just ignore the bootstrap.
            }
            if !update {
                self.send_teardown_for_rejected_path(rx.source_key, rx.path_id, from);
                return;
            }
            if let Some(previous_path) = &self.descending_path.clone() {
                self.send_teardown_for_existing_path(
                    0,
                    previous_path.public_key,
                    previous_path.path_id,
                );
            }
            let entry = SnekPath {
                origin: rx.source_key,
                target: rx.destination_key,
                source: from.clone(),
                destination: 0,
                last_seen: SystemTime::now(),
                root: rx.root.clone(),
                active: false,
            };
            self.paths.insert(index.clone(), entry.clone());
            self.descending_path = Some(index.clone());
            // Send back a setup ACK to the remote side
            let setup_ack = SnekSetupAck {
                root: rx.root.clone(),
                destination_key: rx.source_key,
                path_id: index.path_id,
            };
            self.send(
                Frame::SnekSetupACK(setup_ack),
                self.get_peer_on_port(entry.source).unwrap(),
            );
            return;
        }

        // Try to forward the setup onto the next node first. If we
        // can't do that then there's no point in keeping the path.
        let next_peer = self.get_peer_on_port(next_hop).unwrap();
        if next_peer == self.public_key() {
            debug!("Next hop for {:?} is local, which shouldn't happen.", rx);
            self.send_teardown_for_rejected_path(rx.source_key, rx.path_id, from);
            return;
        }
        // Add a new routing table entry as we are intermediate to
        // the path.
        let entry = SnekPath {
            origin: rx.source_key,
            target: rx.destination_key,
            source: from,          // node with lower of the two keys
            destination: next_hop, // node with higher of the two keys
            last_seen: SystemTime::now(),
            root: rx.root,
            active: false,
        };
        self.paths.insert(index, entry);
    }

    /// `handle_setup_ack` is called in response to a setup ACK
    /// packet from the network
    fn handle_setup_ack(&mut self, from: Port, rx: SnekSetupAck) {
        // Look up to see if we have a matching route. The route must be not active
        // (i.e. we haven't received a setup ACK for it yet) and must have arrived
        // from the port that the entry was populated with.
        for (key, entry) in self.paths.clone() {
            if entry.active || key.public_key != rx.destination_key || key.path_id != rx.path_id {
                continue;
            }
            if from == 0 {}
            if from == entry.destination {
                if entry.source == 0 {
                    let entry_source = self.get_peer_on_port(entry.source).unwrap();
                    self.send(Frame::SnekSetupACK(rx.clone()), entry_source);
                }
            }
        }
        for (key, entry) in self.paths.iter_mut() {
            if entry.active || key.public_key != rx.destination_key || key.path_id != rx.path_id {
                continue;
            }
            if from == 0 {}
            if from == entry.destination {
                if entry.source == 0 {
                    entry.active = true;
                    if let Some(candidate) = &self.candidate {
                        if entry == candidate {
                            self.candidate = None;
                        }
                    }
                }
            }
        }
    }

    /// `handle_teardown` is called in response to receiving a teardown
    /// packet from the network
    fn handle_teardown(&mut self, from: Port, rx: SnekTeardown) -> Vec<Port> {
        self.teardown_path(from, rx.destination_key, rx.path_id)
    }

    /// `teardown_path` processes a teardown message by tearing down any
    /// related routes, returning a slice of next-hop candidates that the
    /// teardown must be forwarded to.
    fn teardown_path(
        &mut self,
        from: Port,
        path_key: VerificationKey,
        path_id: SnekPathId,
    ) -> Vec<Port> {
        if let Some(asc) = &self.ascending_path {
            if asc.public_key == path_key && asc.path_id == path_id {
                if from == 0 {
                    // originated locally
                }
                let ascending = self.paths.get(asc).unwrap().clone();
                if from == ascending.destination {
                    // from network
                    self.paths.remove(asc);
                    self.ascending_path = None;
                    return vec![ascending.destination];
                }
            }
        }
        if let Some(desc) = &self.descending_path {
            if desc.public_key == path_key && desc.path_id == path_id {
                if from == 0 {
                    // originated locally
                }
                let descending = self.paths.get(desc).unwrap().clone();
                if from == descending.destination {
                    // from network
                    self.paths.remove(desc);
                    self.descending_path = None;
                    return vec![descending.destination];
                }
            }
        }
        for (key, value) in self.paths.to_owned() {
            if key.public_key == path_key && key.path_id == path_id {
                if from == 0 {
                    // happens when we're tearing down an existing duplicate path
                    self.paths.remove(&key);
                    return vec![value.destination, value.source];
                }
                if from == value.source {
                    // from network, return the opposite direction
                    self.paths.remove(&key);
                    return vec![value.destination];
                }
                if from == value.destination {
                    // from network, return the opposite direction
                    self.paths.remove(&key);
                    return vec![value.source];
                }
            }
        }
        return vec![];
    }

    fn send_teardown_for_existing_path(
        &mut self,
        from: Port,
        path_key: VerificationKey,
        path_id: SnekPathId,
    ) {
        let frame = self.get_teardown(path_key, path_id);
        for next_hop in self.teardown_path(from, path_key, path_id) {
            let peer = self.get_peer_on_port(next_hop).unwrap();
            self.send(Frame::SnekTeardown(frame.clone()), peer);
        }
    }
    fn send_teardown_for_rejected_path(
        &mut self,
        path_key: VerificationKey,
        path_id: SnekPathId,
        via: Port,
    ) {
        let frame = self.get_teardown(path_key, path_id);
        let peer = self.get_peer_on_port(via).unwrap();
        self.send(Frame::SnekTeardown(frame), peer);
    }

    fn get_teardown(&self, path_key: VerificationKey, path_id: SnekPathId) -> SnekTeardown {
        SnekTeardown {
            root: self.current_root(),
            destination_key: path_key,
            path_id,
        }
    }
    /// `dht_ordered` returns true if the order of A, B and C is
    /// correct, where A < B < C without wrapping.
    fn dht_ordered(a: &VerificationKey, b: &VerificationKey, c: &VerificationKey) -> bool {
        a < b && b < c
    }
}
