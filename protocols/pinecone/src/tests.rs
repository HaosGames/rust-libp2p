use env_logger::WriteStyle;
use futures::StreamExt;
use libp2p_core::identity::Keypair;
use libp2p_core::muxing::StreamMuxerBox;
use libp2p_core::upgrade;
use libp2p_core::upgrade::Version;
use libp2p_core::{identity, Multiaddr, PeerId, Transport};
use libp2p_swarm::{Swarm, SwarmEvent};
use libp2p_tcp::tokio::Tcp;
use libp2p_tcp::{GenTcpConfig, TokioTcpConfig};
use log::{info, warn, LevelFilter};

#[ignore]
#[tokio::test]
async fn listen() {
    let _ = env_logger::builder()
        .write_style(WriteStyle::Always)
        .format_timestamp(None)
        .filter_level(LevelFilter::Debug)
        .filter_module("libp2p_pinecone", LevelFilter::Trace)
        .filter_module("armaged", LevelFilter::Trace)
        .init();

    let ed_keypair = identity::ed25519::Keypair::generate();
    let keypair = Keypair::Ed25519(ed_keypair.clone());
    let local_peer_id = PeerId::from(keypair.public());
    info!("Local PeerID: {:?}", local_peer_id);
    let noise_keys = libp2p_noise::Keypair::<libp2p_noise::X25519Spec>::new()
        .into_authentic(&keypair)
        .unwrap();

    let transport = GenTcpConfig::<Tcp>::new()
        .nodelay(true)
        .upgrade(Version::V1)
        .authenticate(libp2p_noise::NoiseConfig::xx(noise_keys).into_authenticated())
        .multiplex(libp2p_mplex::MplexConfig::default())
        .boxed();

    let pinecone = super::Router::new(ed_keypair);

    let mut swarm = Swarm::new(transport, pinecone, local_peer_id);

    if let Some(addr) = std::env::args().nth(1) {
        swarm
            .listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())
            .unwrap();
        let remote: Multiaddr = addr.parse().unwrap();
        swarm.dial(remote).unwrap();
        info!("Dialed {}", addr);
    } else {
        swarm
            .listen_on("/ip4/127.0.0.1/tcp/42737".parse().unwrap())
            .unwrap();
    }

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => info!("Listening on {:?}", address),
            SwarmEvent::Behaviour(event) => info!("{:?}", event),
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                info!("Connection established with {:?}", peer_id)
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint,
                num_established,
                cause,
            } => {
                warn!("ConnectionClosed with {}", peer_id);
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error } => {
                warn!("OutgoingConnectionError with {:?}, {:?}", peer_id, error);
            }
            SwarmEvent::Dialing(peer_id) => {
                info!("Dialing {}", peer_id);
            }
            _ => {}
        }
    }
}
