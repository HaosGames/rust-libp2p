mod coordinates;
mod frames;
mod handler;
mod protocol;
mod snek;
mod tree;
mod wait_timer;
mod wire_frame;

use crate::coordinates::Coordinates;
use handler::Connection;
use libp2p_core::identity::Keypair;
use libp2p_core::{connection::ConnectionId, PeerId};
use libp2p_swarm::{
    ConnectionHandler, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction,
    NotifyHandler, PollParameters,
};
use log::{debug, info, trace};
use std::collections::btree_map::Keys;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};
use std::{
    collections::VecDeque,
    task::{Context, Poll},
};

use crate::frames::{Frame, TreeAnnouncement};
use crate::snek::SnekRouted;
use crate::tree::{Root, TreeRouted};
use crate::wait_timer::WaitTimer;

pub type Port = u64;
pub type SequenceNumber = u64;
pub type SnekPathId = u64;

pub(crate) const SNEK_EXPIRY_PERIOD: Duration = Duration::from_secs(60 * 60);

pub(crate) const ANNOUNCEMENT_TIMEOUT: Duration = Duration::from_secs(45 * 60); //45 min
pub(crate) const ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 min
pub(crate) const REPARENT_WAIT_TIME: Duration = Duration::from_secs(1); //   1 sec

pub struct Router {
    in_events: VecDeque<Event>,
    out_events: VecDeque<Event>,

    keypair: Keypair,
    peer_id: PeerId,

    parent: PeerId,
    tree_announcements: BTreeMap<PeerId, TreeAnnouncement>,
    ports: BTreeMap<PeerId, Port>,

    sequence: SequenceNumber,
    ordering: SequenceNumber,

    announcement_timer: WaitTimer,
    reparent_timer: Option<WaitTimer>,
}

#[derive(Debug)]
pub enum Event {
    ReceivedFrame { from: PeerId, frame: Frame },
    SendFrame { to: PeerId, frame: Frame },
    NewPeer,
    AddPeer(PeerId),
    RegisterPort { port: Port, of: PeerId },
    RemovePeer,
}

impl NetworkBehaviour for Router {
    type ConnectionHandler = Connection;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        Connection::new(self.keypair.clone())
    }

    fn inject_event(
        &mut self,
        peer: PeerId,
        _: ConnectionId,
        result: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as ConnectionHandler>::OutEvent,
    ) {
        match result {
            Event::NewPeer => {
                self.in_events.push_front(Event::AddPeer(peer));
            }
            _ => self.in_events.push_front(result),
        }
    }

    fn poll(
        &mut self,
        _: &mut Context<'_>,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
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
                Event::RemovePeer => {
                    todo!()
                }
                Event::NewPeer => {}
            }
        }

        self.maintain_tree(true);
        //TODO
        // self.maintain_snek();

        // Handle out_events
        if let Some(event) = self.out_events.pop_back() {
            match event {
                Event::ReceivedFrame { from, frame } => {
                    self.in_events
                        .push_front(Event::ReceivedFrame { from, frame });
                    return Poll::Pending;
                }
                Event::SendFrame { to, frame } => {
                    return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id: to,
                        handler: NotifyHandler::Any,
                        event: Event::SendFrame { to, frame },
                    });
                }
                Event::AddPeer(from) => {
                    self.in_events.push_front(Event::AddPeer(from));
                    return Poll::Pending;
                }
                Event::RegisterPort { port, of } => {
                    return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id: of,
                        handler: NotifyHandler::Any,
                        event: Event::RegisterPort { port, of },
                    })
                }
                Event::RemovePeer => {
                    todo!()
                }
                Event::NewPeer => {
                    return Poll::Pending;
                }
            }
        } else {
            return Poll::Pending;
        }
    }
}

impl Router {
    pub fn new(keypair: Keypair) -> Self {
        let peer_id = keypair.public().to_peer_id();
        Self {
            in_events: Default::default(),
            out_events: Default::default(),
            keypair,
            peer_id,
            tree_announcements: Default::default(),
            ports: Default::default(),
            parent: peer_id,
            sequence: 0,
            ordering: 0,
            announcement_timer: WaitTimer::new_expired(),
            reparent_timer: None,
        }
    }
    pub fn add(&mut self, peer: PeerId) {
        let port = self.get_new_port();
        self.ports.insert(peer.clone(), port);
        info!("Added peer {}", peer);
        self.out_events
            .push_front(Event::RegisterPort { port, of: peer });
        self.send_tree_announcement(peer, self.current_tree_announcement());
    }
    fn get_new_port(&self) -> Port {
        for i in 1.. {
            let mut port_exists = false;
            for port in self.ports.values() {
                if &i == port {
                    port_exists = true;
                    break;
                }
            }
            if port_exists {
                continue;
            } else {
                return i;
            }
        }
        unreachable!("Reached port limit of {}", Port::MAX);
    }

    fn tree_announcement(&self, of: PeerId) -> Option<&TreeAnnouncement> {
        self.tree_announcements.get(&of)
    }
    fn set_tree_announcement(&mut self, of: PeerId, announcement: TreeAnnouncement) {
        self.tree_announcements.insert(of.clone(), announcement);
    }
    fn port(&self, of: PeerId) -> Option<&Port> {
        self.ports.get(&of)
    }
    fn peers(&self) -> Vec<PeerId> {
        let mut peers = Vec::new();
        for (peer, _) in &self.ports {
            peers.push(peer.clone());
        }
        peers
    }
    fn parent(&self) -> PeerId {
        self.parent
    }
    fn set_parent(&mut self, peer: PeerId) {
        info!(
            "Setting new parent to {} on port {}",
            peer,
            self.port(peer).unwrap_or(&0)
        );
        self.parent = peer;
    }
    fn keypair(&self) -> Keypair {
        self.keypair.clone()
    }
    fn peer_id(&self) -> PeerId {
        self.peer_id
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
        let peer_id = self.peer_id().clone();
        self.send(frame, peer_id);
    }
    fn send(&mut self, frame: Frame, to: PeerId) {
        self.out_events.push_front(Event::SendFrame { to, frame });
    }

    fn next_tree_hop(&self, frame: &impl TreeRouted, from: PeerId) -> Option<PeerId> {
        if frame.destination_coordinates() == self.coordinates() {
            return Some(self.peer_id());
        }
        let our_distance = frame
            .destination_coordinates()
            .distance_to(&self.coordinates());
        if our_distance == 0 {
            return Some(self.peer_id());
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
    fn next_snek_hop(&self, frame: &impl SnekRouted) -> Option<PeerId> {
        todo!()
    }
    fn handle_frame(&mut self, frame: Frame, from: PeerId) {
        trace!("{} Handling Frame...", self.peer_id());
        match frame {
            Frame::TreeRouted(packet) => {
                trace!("Frame is TreeRouted");
                if let Some(peer) = self.next_tree_hop(&packet, from) {
                    let peer = peer;
                    if peer == self.peer_id() {
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
                if let Some(peer) = self.next_snek_hop(&packet) {
                    let peer = peer;
                    if peer == self.peer_id() {
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
                todo!()
            }
            Frame::SnekBootstrapACK(ack) => {
                todo!()
            }
            Frame::SnekSetup(setup) => {
                todo!()
            }
            Frame::SnekSetupACK(ack) => {
                todo!()
            }
            Frame::SnekTeardown(teardown) => {
                todo!()
            }
        }
    }
    fn handle_tree_announcement(&mut self, mut frame: TreeAnnouncement, from: PeerId) {
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
            if frame.is_loop_of_child(&self.peer_id()) {
                // SelectNewParentWithWait
                debug!("Announcement contains loop");
                self.set_reparent_timer();
                self.become_root();
                return;
            }
            if frame.root.public_key.to_peer_id()
                < self
                    .current_tree_announcement()
                    .root
                    .public_key
                    .to_peer_id()
            {
                // SelectNewParentWithWait
                debug!("Announcement has weaker root");
                self.set_reparent_timer();
                self.become_root();
                return;
            }
            if frame.root.public_key.to_peer_id()
                > self
                    .current_tree_announcement()
                    .root
                    .public_key
                    .to_peer_id()
            {
                // AcceptUpdate
                debug!("Announcement has stronger root. Forwarding to peers");
                self.send_tree_announcements_to_all(self.current_tree_announcement());
                return;
            }
            if frame.root.public_key == self.current_tree_announcement().root.public_key {
                if frame.root.sequence_number
                    > self.current_tree_announcement().root.sequence_number
                {
                    // AcceptUpdate
                    trace!("Announcement has higher sequence. Forwarding to peers");
                    self.send_tree_announcements_to_all(self.current_tree_announcement());
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
            if frame.is_loop_of_child(&self.peer_id()) {
                // DropFrame
                trace!("Announcement contains loop. Dropping");
                return;
            }
            if frame.root.public_key.to_peer_id()
                > self
                    .current_tree_announcement()
                    .root
                    .public_key
                    .to_peer_id()
            {
                // AcceptNewParent
                trace!("Announcement has stronger root. Forwarding to peers");
                self.set_parent(from.clone());
                self.send_tree_announcements_to_all(self.current_tree_announcement());
                return;
            }
            if frame.root.public_key.to_peer_id()
                < self
                    .current_tree_announcement()
                    .root
                    .public_key
                    .to_peer_id()
            {
                // InformPeerOfStrongerRoot
                trace!("Announcement has weaker root. Sending my announcement");
                self.send_tree_announcement(from, self.current_tree_announcement());
                return;
            }
            if frame.root.public_key == self.current_tree_announcement().root.public_key {
                // SelectNewParent
                trace!("Announcement has same root");
                self.reparent(false);
                return;
            }
        }
    }
    fn current_tree_announcement(&self) -> TreeAnnouncement {
        if let Some(announcement) = self.tree_announcement(self.parent()) {
            announcement.clone()
        } else {
            TreeAnnouncement {
                root: Root {
                    public_key: self.keypair().public(),
                    sequence_number: self.current_sequence(),
                },
                signatures: vec![],
                receive_time: SystemTime::now(),
                receive_order: self.current_ordering(),
            }
        }
    }
    fn coordinates(&self) -> Coordinates {
        self.current_tree_announcement().into()
    }
    fn send_tree_announcements_to_all(&mut self, announcement: TreeAnnouncement) {
        trace!("Sending tree announcements to all peers");
        for peer in self.peers() {
            self.send_tree_announcement(peer, announcement.clone());
        }
    }
    fn send_tree_announcement(&mut self, to: PeerId, announcement: TreeAnnouncement) {
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
                public_key: self.keypair().public(),
                sequence_number: self.next_sequence(),
            },
            signatures: vec![],
            receive_time: SystemTime::now(),
            receive_order: 0,
        }
    }
    fn parent_selection(&mut self) -> bool {
        trace!("Running parent selection...");
        if self.peer_id() > self.current_root().public_key.to_peer_id() {
            trace!("My key is stronger than current root");
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
                if announcement.is_loop_of_child(&self.peer_id()) {
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
        match best_peer {
            Some(best_peer) => {
                if best_peer == self.parent() {
                    return false;
                }
                let best_peer = best_peer.clone();
                self.set_parent(best_peer);
                self.send_tree_announcements_to_all(self.current_tree_announcement());
                return true;
            }
            None => {
                self.become_root();
                return false;
            }
        }
    }
    fn bootstrap_now(&self) {
        info!("Bootstrapping ...");
        return;
        // TODO
    }
    fn become_root(&mut self) {
        info!("{} becoming root", self.peer_id());
        self.set_parent(self.peer_id().clone());
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
        self.current_tree_announcement().root
    }
    fn maintain_tree(&mut self, wait: bool) {
        if self.i_am_root() {
            if self.announcement_timer_expired() {
                self.reset_announcement_timer();
                let announcement = self.new_tree_announcement();
                self.send_tree_announcements_to_all(announcement)
            }
        }
        self.reparent(true);
    }
    fn i_am_root(&self) -> bool {
        self.peer_id() == self.parent()
    }
}
