use crate::frames::Frame;
use crate::protocol::PineconeProtocol;
use crate::wire_frame::PineconeCodec;
use crate::{protocol, Event, Port, TreeAnnouncement, VerificationKey};
use asynchronous_codec::{Framed, FramedRead, FramedWrite};
use futures::{SinkExt, StreamExt};
use libp2p_core::identity::ed25519::Keypair;
use libp2p_core::{upgrade::NegotiationError, PeerId, UpgradeError};
use libp2p_swarm::{
    ConnectionHandler, ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive,
    NegotiatedSubstream, SubstreamProtocol,
};
use log::{debug, error, trace, warn};
use std::collections::VecDeque;
use std::future::Future;
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

    inbound: Option<Framed<NegotiatedSubstream, PineconeCodec>>,
    outbound: Option<Framed<NegotiatedSubstream, PineconeCodec>>,
    creating_outbound: bool,
    port: Port,
    peer: Option<VerificationKey>,

    keepalive: KeepAlive,
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
    type InboundProtocol = protocol::PineconeProtocol;
    type OutboundProtocol = protocol::PineconeProtocol;
    type InboundOpenInfo = VerificationKey;
    type OutboundOpenInfo = VerificationKey;

    fn listen_protocol(&self) -> SubstreamProtocol<protocol::PineconeProtocol, VerificationKey> {
        SubstreamProtocol::new(protocol::PineconeProtocol, self.keypair.public().encode())
    }

    fn inject_fully_negotiated_inbound(
        &mut self,
        stream: NegotiatedSubstream,
        public_key: VerificationKey,
    ) {
        self.inbound = Some(Framed::new(stream, PineconeCodec));
        debug!("Set inbound stream");
    }

    fn inject_fully_negotiated_outbound(
        &mut self,
        stream: NegotiatedSubstream,
        public_key: VerificationKey,
    ) {
        self.outbound = Some(Framed::new(stream, PineconeCodec));
        self.creating_outbound = false;
        debug!("Set outbound stream");
    }

    fn inject_event(&mut self, event: Event) {
        trace!("Injecting {:?}", event);
        self.in_events.push_front(event);
    }

    fn inject_dial_upgrade_error(
        &mut self,
        _info: VerificationKey,
        error: ConnectionHandlerUpgrErr<Void>,
    ) {
        self.outbound = None;
        self.creating_outbound = false;
        warn!("Couldn't upgrade connection {:?}", error)
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keepalive
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<
            protocol::PineconeProtocol,
            VerificationKey,
            Self::OutEvent,
            Self::Error,
        >,
    > {
        if let Some(peer) = self.peer {
            if peer == self.keypair.public().encode() {
                self.peer = None;
                error!("Added myself as Peer");
                panic!();
            }
        }
        if self.outbound.is_none() && !self.creating_outbound {
            self.creating_outbound = true;
            trace!("Requesting outbound stream");
            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest {
                protocol: SubstreamProtocol::new(PineconeProtocol, self.keypair.public().encode()),
            });
        }
        // Handle incoming events
        if let Some(event) = self.in_events.pop_back() {
            match event {
                Event::RegisterPort { port, of } => {
                    self.port = port;
                    self.peer = Some(of);
                }
                Event::SendFrame { to, frame } => {
                    match frame {
                        Frame::TreeAnnouncement(ann) => {
                            let signed = ann.append_signature(self.keypair.clone(), self.port);
                            // Send signed announcement to peer
                            if let Some(stream) = &mut self.outbound {
                                debug!("Sending to port {}: {:?}", self.port, signed);
                                stream.start_send_unpin(Frame::TreeAnnouncement(signed));
                            } else {
                                trace!("Sending TreeAnnouncement next round");
                                self.in_events.push_front(Event::SendFrame {
                                    to,
                                    frame: Frame::TreeAnnouncement(ann),
                                });
                            }
                        }
                        _ => {
                            // Send frame to peer
                            if let Some(stream) = &mut self.outbound {
                                debug!("Sending {:?}", frame);
                                stream.start_send_unpin(frame);
                            }
                        }
                    }
                }
                _ => self.out_events.push_front(event),
            }
        }
        if let Some(stream) = &mut self.outbound {
            stream.flush();
        }

        // Receive frames from peer
        if let Some(stream) = &mut self.inbound {
            match stream.poll_next_unpin(cx) {
                Poll::Ready(option) => match option {
                    None => {
                        self.inbound = None;
                        self.keepalive = KeepAlive::No;
                        if let Some(peer) = self.peer {
                            self.out_events.push_front(Event::RemovePeer(peer));
                            self.peer = None;
                        }
                        warn!("Inbound Stream was terminated");
                    }
                    Some(result) => match result {
                        Ok(frame) => {
                            if self.peer.is_none() {
                                match &frame {
                                    Frame::TreeAnnouncement(announcement) => {
                                        if let Some(sig) = announcement.signatures.last() {
                                            self.peer = Some(sig.signing_public_key);
                                            self.out_events
                                                .push_front(Event::AddPeer(sig.signing_public_key));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            debug!("Decoded frame {:?}", frame);
                            self.out_events.push_front(Event::ReceivedFrame {
                                from: self.peer.unwrap(),
                                frame,
                            });
                        }
                        Err(e) => {
                            warn!("Could not decode frame {:?}", e);
                        }
                    },
                },
                Poll::Pending => {}
            }
        }

        // Return out_event
        if let Some(event) = self.out_events.pop_back() {
            Poll::Ready(ConnectionHandlerEvent::Custom(event))
        } else {
            Poll::Pending
        }
    }
}

impl Connection {
    pub fn new(keypair: Keypair, port: Port, announcement: TreeAnnouncement) -> Self {
        let mut in_events: VecDeque<Event> = Default::default();
        in_events.push_front(Event::SendFrame {
            to: [0; 32],
            frame: Frame::TreeAnnouncement(announcement),
        });
        Connection {
            keypair,
            in_events,
            out_events: Default::default(),
            inbound: None,
            outbound: None,
            creating_outbound: false,
            port,
            peer: None,
            keepalive: KeepAlive::Yes,
        }
    }
}
