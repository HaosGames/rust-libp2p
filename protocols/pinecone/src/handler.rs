use crate::frames::Frame;
use crate::protocol::PineconeProtocol;
use crate::wire_frame::PineconeCodec;
use crate::{protocol, Event, Port, VerificationKey};
use asynchronous_codec::{Framed, FramedRead, FramedWrite};
use futures::{SinkExt, StreamExt};
use libp2p_core::identity::ed25519::Keypair;
use libp2p_core::{upgrade::NegotiationError, PeerId, UpgradeError};
use libp2p_swarm::{
    ConnectionHandler, ConnectionHandlerEvent, ConnectionHandlerUpgrErr, KeepAlive,
    NegotiatedSubstream, SubstreamProtocol,
};
use log::{debug, trace, warn};
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
    port: Option<Port>,
    peer: Option<VerificationKey>,
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
        self.peer = Some(public_key);
        self.out_events.push_front(Event::AddPeer(public_key));
        debug!("Set outbound stream");
    }

    fn inject_event(&mut self, event: Event) {
        self.in_events.push_front(event);
    }

    fn inject_dial_upgrade_error(
        &mut self,
        _info: VerificationKey,
        error: ConnectionHandlerUpgrErr<Void>,
    ) {
        self.outbound = None;
        self.creating_outbound = false;
        if let Some(public_key) = self.peer {
            self.out_events.push_front(Event::RemovePeer(public_key));
        }
        warn!("Couldn't upgrade connection {:?}", error)
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
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
                    self.port = Some(port);
                    self.peer = Some(of);
                }
                Event::SendFrame { to, frame } => {
                    if let Some(public_key) = self.peer {
                        match frame {
                            Frame::TreeAnnouncement(ann) => {
                                if let Some(port) = self.port {
                                    let signed = ann.append_signature(self.keypair.clone(), port);
                                    // Send signed announcement to peer
                                    if let Some(stream) = &mut self.outbound {
                                        stream.start_send_unpin(Frame::TreeAnnouncement(signed));
                                    }
                                }
                            }
                            _ => {
                                // Send frame to peer
                                if let Some(stream) = &mut self.outbound {
                                    stream.start_send_unpin(frame);
                                }
                            }
                        }
                    }
                }
                _ => self.out_events.push_front(event),
            }
        }

        // Receive frames from peer
        loop {
            if let Some(stream) = &mut self.inbound {
                match stream.poll_next_unpin(cx) {
                    Poll::Ready(option) => match option {
                        None => {
                            self.inbound = None;
                            warn!("Inbound Stream was terminated");
                            break;
                        }
                        Some(result) => match result {
                            Ok(frame) => {
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
                    Poll::Pending => break,
                }
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
    pub fn new(keypair: Keypair) -> Self {
        Connection {
            keypair,
            in_events: Default::default(),
            out_events: Default::default(),
            inbound: None,
            outbound: None,
            creating_outbound: false,
            port: None,
            peer: None,
        }
    }
}
