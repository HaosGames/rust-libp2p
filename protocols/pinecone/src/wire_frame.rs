use crate::{Coordinates, Frame};
use libp2p_core::PublicKey;
use log::debug;

/// MaxFrameSize is the maximum size that a single frame can be, including
/// all headers.
const MAX_FRAME_SIZE: u32 = 65535 * 3 + 16;
/// MaxPayloadSize is the maximum size that a single frame can contain
/// as a payload, not including headers.
const MAX_PAYLOAD_SIZE: u16 = 65535;
const FRAME_MAGIC_BYTES: [u8; 4] = [0x70, 0x69, 0x6e, 0x65];
/// 4 magic bytes, 1 byte version, 1 byte type, 2 bytes extra, 2 bytes frame length
const FRAME_HEADER_LENGTH: u32 = 10;

pub struct WireFrame {
    // Header
    version: u8,
    frame_type: u8,
    extra: [u8; 2],
    frame_length: u16,

    // Payload
    destination: Option<Coordinates>,
    destination_key: Option<PublicKey>,
    source: Option<Coordinates>,
    source_key: Option<PublicKey>,
    payload: Vec<u8>,
}
impl WireFrame {
    pub fn new(event: Frame) -> Option<Self> {
        match event {
            Frame::TreeRouted(packet) => {
                return Some(Self {
                    version: 1,
                    frame_type: 0,
                    extra: [0; 2],
                    frame_length: 0,
                    destination: Some(packet.destination_coordinates),
                    destination_key: Some(packet.destination_key),
                    source: Some(packet.source_coordinates),
                    source_key: Some(packet.source_key),
                    payload: vec![],
                });
            }
            Frame::SnekRouted(packet) => {}
            Frame::TreeAnnouncement(announcement) => {}
            Frame::SnekBootstrap(_) => {}
            Frame::SnekBootstrapACK(_) => {}
            Frame::SnekSetup(_) => {}
            Frame::SnekSetupACK(_) => {}
            Frame::SnekTeardown(_) => {}
        }
        todo!()
    }
}
