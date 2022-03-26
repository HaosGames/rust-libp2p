use crate::frames::Frame;
use crate::protocol::Pinecone;
use crate::{protocol, Event, Port};
use libp2p_core::identity::Keypair;
use libp2p_core::{upgrade::NegotiationError, PeerId, UpgradeError};
use libp2p_swarm::{
    ConnectionHandler, ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive,
    NegotiatedSubstream, SubstreamProtocol,
};
use std::collections::VecDeque;
use std::{
    error::Error,
    fmt, io,
    num::NonZeroU32,
    task::{Context, Poll},
    time::Duration,
};
use void::Void;

#[derive(Debug)]
pub enum Failure {
    Timeout,
    Unsupported,
    Other {
        error: Box<dyn std::error::Error + Send + 'static>,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Timeout => f.write_str("Ping timeout"),
            Failure::Other { error } => write!(f, "Ping error: {}", error),
            Failure::Unsupported => write!(f, "Ping protocol not supported"),
        }
    }
}

impl Error for Failure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Failure::Timeout => None,
            Failure::Other { error } => Some(&**error),
            Failure::Unsupported => None,
        }
    }
}

pub struct Connection {
    keypair: Keypair,

    in_events: VecDeque<Event>,
    out_events: VecDeque<Event>,

    inbound: Option<NegotiatedSubstream>,
    outbound: Option<NegotiatedSubstream>,
    port: Option<Port>,
    peer: Option<PeerId>,
    // config: Config,
    // timer: Delay,
    // pending_errors: VecDeque<Failure>,
    // failures: u32,
    // outbound: Option<PingState>,
    // inbound: Option<PongFuture>,
    // state: State,
}

impl ConnectionHandler for Connection {
    type InEvent = Event;
    type OutEvent = Event;
    type Error = Failure;
    type InboundProtocol = protocol::Pinecone;
    type OutboundProtocol = protocol::Pinecone;
    type InboundOpenInfo = ();
    type OutboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<protocol::Pinecone, ()> {
        SubstreamProtocol::new(protocol::Pinecone, ())
    }

    fn inject_fully_negotiated_inbound(&mut self, stream: NegotiatedSubstream, (): ()) {
        self.inbound = Some(stream);
        self.out_events.push_front(Event::NewPeer);
    }

    fn inject_fully_negotiated_outbound(&mut self, stream: NegotiatedSubstream, (): ()) {
        self.outbound = Some(stream);
        self.out_events.push_front(Event::NewPeer);
    }

    fn inject_event(&mut self, event: Event) {
        self.in_events.push_front(event);
    }

    fn inject_dial_upgrade_error(&mut self, _info: (), error: ConnectionHandlerUpgrErr<Void>) {
        self.outbound = None;
        self.out_events.push_front(Event::RemovePeer);
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ConnectionHandlerEvent<protocol::Pinecone, (), Self::OutEvent, Self::Error>> {
        if self.outbound.is_none() {
            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest {
                protocol: SubstreamProtocol::new(Pinecone, ()),
            });
        }
        // Handle incoming events
        if let Some(event) = self.in_events.pop_back() {
            match event {
                Event::RegisterPort { port, of } => {
                    self.port = Some(port);
                    self.peer = Some(of);
                }
                Event::RemovePeer => {
                    self.port = None;
                    self.peer = None;
                }
                Event::SendFrame { to, frame } => {
                    match frame {
                        Frame::TreeAnnouncement(ann) => {
                            if let Some(port) = self.port {
                                let signed = ann.append_signature(self.keypair.clone(), port);
                                //TODO
                                // Send signed announcement to peer
                            } else {
                                self.out_events.push_front(Event::NewPeer);
                            }
                        }
                        _ => {
                            //TODO
                            // Send frame to peer
                        }
                    }
                }
                _ => self.out_events.push_front(event),
            }
        }

        //TODO
        // Receive frames from peer

        // Return out_events
        if let Some(event) = self.out_events.pop_back() {
            Poll::Ready(ConnectionHandlerEvent::Custom(event))
        } else {
            Poll::Pending
        }
    }
}

impl Connection {
    pub fn new(keypair: Keypair) -> Self {
        Connection {
            keypair,
            in_events: Default::default(),
            out_events: Default::default(),
            inbound: None,
            outbound: None,
            port: None,
            peer: None,
        }
    }
}
