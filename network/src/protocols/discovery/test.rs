// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{peer_manager::PeerManagerRequest, proto::DiscoveryMsg};
use core::str::FromStr;
use crypto::{test_utils::TEST_SEED, *};
use futures::future::{FutureExt, TryFutureExt};
use memsocket::MemorySocket;
use rand::{rngs::StdRng, SeedableRng};
use tokio::runtime::Runtime;

fn gen_peer_info() -> PeerInfo {
    let mut peer_info = PeerInfo::default();
    peer_info.epoch = 1;
    peer_info.addrs.push(
        Multiaddr::from_str("/ip4/127.0.0.1/tcp/9090")
            .unwrap()
            .as_ref()
            .into(),
    );
    peer_info
}

fn gen_full_node_payload() -> FullNodePayload {
    let mut payload = FullNodePayload::default();
    payload.epoch = 1;
    payload.dns_seed_addr = b"example.com"[..].into();
    payload
}

fn get_addrs_from_note(note: &Note) -> Vec<Multiaddr> {
    let signed_peer_info = note.signed_peer_info.as_ref().unwrap();
    let peer_info = PeerInfo::decode(&signed_peer_info.peer_info).unwrap();
    let mut addrs = vec![];
    for addr in peer_info.addrs {
        addrs.push(Multiaddr::try_from(addr.clone()).unwrap());
    }
    addrs
}

fn get_addrs_from_info(peer_info: &PeerInfo) -> Vec<Multiaddr> {
    peer_info
        .addrs
        .iter()
        .map(|addr| Multiaddr::try_from(addr.clone()).unwrap())
        .collect()
}

fn setup_discovery(
    rt: &mut Runtime,
    peer_id: PeerId,
    addrs: Vec<Multiaddr>,
    seed_peer_id: PeerId,
    seed_peer_info: PeerInfo,
    signer: Signer,
    trusted_peers: Arc<RwLock<HashMap<PeerId, NetworkPublicKeys>>>,
) -> (
    channel::Receiver<PeerManagerRequest<MemorySocket>>,
    channel::Receiver<ConnectivityRequest>,
    channel::Sender<PeerManagerNotification<MemorySocket>>,
    channel::Sender<()>,
) {
    let (peer_mgr_reqs_tx, peer_mgr_reqs_rx) = channel::new_test(0);
    let (conn_mgr_reqs_tx, conn_mgr_reqs_rx) = channel::new_test(1);
    let (peer_mgr_notifs_tx, peer_mgr_notifs_rx) = channel::new_test(0);
    let (ticker_tx, ticker_rx) = channel::new_test(0);
    let discovery = {
        Discovery::new(
            peer_id,
            addrs,
            signer,
            vec![(seed_peer_id, seed_peer_info)].into_iter().collect(),
            trusted_peers,
            ticker_rx,
            PeerManagerRequestSender::new(peer_mgr_reqs_tx),
            peer_mgr_notifs_rx,
            conn_mgr_reqs_tx,
            Duration::from_secs(180),
        )
    };
    rt.spawn(discovery.start().boxed().unit_error().compat());
    (
        peer_mgr_reqs_rx,
        conn_mgr_reqs_rx,
        peer_mgr_notifs_tx,
        ticker_tx,
    )
}

async fn expect_address_update(
    conn_mgr_reqs_rx: &mut channel::Receiver<ConnectivityRequest>,
    expected_peer_id: PeerId,
    expected_addrs: &[Multiaddr],
) {
    match conn_mgr_reqs_rx.next().await.unwrap() {
        ConnectivityRequest::UpdateAddresses(peer_id, addrs) => {
            assert_eq!(expected_peer_id, peer_id);
            assert_eq!(expected_addrs, &addrs[..]);
        }
        req => {
            panic!("Unexpected request to connectivity manager: {:?}", req);
        }
    }
}

fn generate_network_pub_keys_and_signer(peer_id: PeerId) -> (NetworkPublicKeys, Signer) {
    let mut rng = StdRng::from_seed(TEST_SEED);
    let (signing_priv_key, _) = compat::generate_keypair(&mut rng);
    let (_, identity_pub_key) = x25519::compat::generate_keypair(&mut rng);
    (
        NetworkPublicKeys {
            signing_public_key: signing_priv_key.public_key().clone(),
            identity_public_key: identity_pub_key,
        },
        Signer::new(peer_id, signing_priv_key),
    )
}

#[test]
// Test behavior on receipt of an inbound DiscoveryMsg.
fn inbound() {
    ::logger::try_init_for_testing();
    let mut rt = Runtime::new().unwrap();

    // Setup self.
    let peer_id = PeerId::random();
    let addrs = vec![Multiaddr::from_str("/ip4/127.0.0.1/tcp/9090").unwrap()];
    let (self_pub_keys, self_signer) = generate_network_pub_keys_and_signer(peer_id);

    // Setup seed.
    let seed_peer_info = gen_peer_info();
    let seed_peer_addrs = get_addrs_from_info(&seed_peer_info);
    let seed_peer_payload = gen_full_node_payload();
    let seed_peer_id = PeerId::random();
    let (seed_pub_keys, seed_signer) = generate_network_pub_keys_and_signer(seed_peer_id);
    let trusted_peers = Arc::new(RwLock::new(
        vec![(seed_peer_id, seed_pub_keys), (peer_id, self_pub_keys)]
            .into_iter()
            .collect(),
    ));

    // Setup discovery.
    let (_, mut conn_mgr_reqs_rx, mut peer_mgr_notifs_tx, _) = setup_discovery(
        &mut rt,
        peer_id,
        addrs,
        seed_peer_id,
        seed_peer_info.clone(),
        self_signer,
        trusted_peers.clone(),
    );

    // Fake connectivity manager and dialer.
    let f_peer_mgr = async move {
        // Connectivity manager receives addresses of the seed peer during bootstrap.
        expect_address_update(&mut conn_mgr_reqs_rx, seed_peer_id, &seed_peer_addrs[..]).await;

        let (dialer_substream, listener_substream) = MemorySocket::new_pair();
        // Notify discovery actor of inbound substream.

        peer_mgr_notifs_tx
            .send(PeerManagerNotification::NewInboundSubstream(
                seed_peer_id,
                NegotiatedSubstream {
                    protocol: ProtocolId::from_static(DISCOVERY_PROTOCOL_NAME),
                    substream: listener_substream,
                },
            ))
            .await
            .unwrap();
        // Wrap dialer substream in a framed substream.
        let mut dialer_substream =
            Framed::new(dialer_substream.compat(), UviBytes::<Bytes>::default()).sink_compat();

        // Send DiscoveryMsg consisting of 2 notes to the discovery actor - one note for the
        // seed peer and one for another peer. The discovery actor should send addresses of the new
        // peer to the connectivity manager.
        let peer_id_other = PeerId::random();
        let addrs_other = vec![Multiaddr::from_str("/ip4/172.29.52.192/tcp/8080").unwrap()];
        let seed_note = create_note(
            &seed_signer,
            seed_peer_id,
            seed_peer_info.clone(),
            seed_peer_payload.clone(),
        );
        let (pub_keys_other, signer_other) = generate_network_pub_keys_and_signer(peer_id_other);
        trusted_peers
            .write()
            .unwrap()
            .insert(peer_id_other, pub_keys_other);
        let note_other = {
            let mut peer_info = PeerInfo::default();
            peer_info.addrs = addrs_other
                .iter()
                .map(|addr| addr.as_ref().into())
                .collect();
            let full_node_payload = gen_full_node_payload();
            create_note(&signer_other, peer_id_other, peer_info, full_node_payload)
        };
        let mut msg = DiscoveryMsg::default();
        msg.notes.push(note_other.clone());
        msg.notes.push(seed_note.clone());
        dialer_substream
            .send(msg.to_bytes().unwrap())
            .await
            .unwrap();

        // Connectivity manager receives address of new peer.
        expect_address_update(&mut conn_mgr_reqs_rx, peer_id_other, &addrs_other[..]).await;

        // The addrs sent to connectivity manager should also include the
        // configured seed peer addrs for seed-peer-received notes.
        let mut expected_seed_addrs = seed_peer_addrs.clone();
        expected_seed_addrs.extend_from_slice(&seed_peer_addrs[..]);

        // Connectivity manager receives a connect to the seed peer at the same address.
        expect_address_update(
            &mut conn_mgr_reqs_rx,
            seed_peer_id,
            &expected_seed_addrs[..],
        )
        .await;

        let (dialer_substream, listener_substream) = MemorySocket::new_pair();
        // Notify discovery actor of inbound substream.

        peer_mgr_notifs_tx
            .send(PeerManagerNotification::NewInboundSubstream(
                peer_id_other,
                NegotiatedSubstream {
                    protocol: ProtocolId::from_static(DISCOVERY_PROTOCOL_NAME),
                    substream: listener_substream,
                },
            ))
            .await
            .unwrap();
        // Wrap dialer substream in a framed substream.
        let mut dialer_substream =
            Framed::new(dialer_substream.compat(), UviBytes::<Bytes>::default()).sink_compat();
        // Compose new msg.
        let mut msg = DiscoveryMsg::default();
        msg.notes.push(note_other);
        let new_seed_addrs = vec![Multiaddr::from_str("/ip4/127.0.0.1/tcp/8098").unwrap()];
        {
            let seed_peer_info = create_peer_info(new_seed_addrs.clone());
            let seed_note = create_note(
                &seed_signer,
                seed_peer_id,
                seed_peer_info,
                seed_peer_payload,
            );
            msg.notes.push(seed_note);
            dialer_substream
                .send(msg.to_bytes().unwrap())
                .await
                .unwrap();
        }

        // The addrs sent to connectivity manager should also include the
        // configured seed peer addrs for seed-peer-received notes.
        let mut expected_seed_addrs = new_seed_addrs.clone();
        expected_seed_addrs.extend_from_slice(&seed_peer_addrs[..]);

        // Connectivity manager receives new address of seed peer.
        expect_address_update(
            &mut conn_mgr_reqs_rx,
            seed_peer_id,
            &expected_seed_addrs[..],
        )
        .await;
    };
    rt.block_on(f_peer_mgr.boxed().unit_error().compat())
        .unwrap();
}

#[test]
// Test that discovery actor sends a DiscoveryMsg to a neighbor on receiving a clock tick.
fn outbound() {
    ::logger::try_init_for_testing();
    let mut rt = Runtime::new().unwrap();

    // Setup self.
    let peer_id = PeerId::random();
    let addrs = vec![Multiaddr::from_str("/ip4/127.0.0.1/tcp/9090").unwrap()];
    let (self_pub_keys, self_signer) = generate_network_pub_keys_and_signer(peer_id);

    // Setup seed.
    let seed_peer_id = PeerId::random();
    let seed_peer_info = gen_peer_info();
    let seed_peer_addrs = get_addrs_from_info(&seed_peer_info);
    let (seed_pub_keys, _) = generate_network_pub_keys_and_signer(seed_peer_id);
    let trusted_peers = Arc::new(RwLock::new(
        vec![(seed_peer_id, seed_pub_keys), (peer_id, self_pub_keys)]
            .into_iter()
            .collect(),
    ));

    // Setup discovery.
    let (mut peer_mgr_reqs_rx, _conn_mgr_req_rx, mut peer_mgr_notifs_tx, mut ticker_tx) =
        setup_discovery(
            &mut rt,
            peer_id,
            addrs.clone(),
            seed_peer_id,
            seed_peer_info.clone(),
            self_signer,
            trusted_peers.clone(),
        );

    // Fake connectivity manager and dialer.
    let f_peer_mgr = async move {
        let (dialer_substream, listener_substream) = MemorySocket::new_pair();
        // Notify discovery actor of connection to seed peer.
        peer_mgr_notifs_tx
            .send(PeerManagerNotification::NewPeer(
                seed_peer_id,
                seed_peer_addrs[0].clone(),
            ))
            .await
            .unwrap();

        // Trigger outbound msg.
        ticker_tx.send(()).await.unwrap();

        // Request outgoing substream from PeerManager.
        match peer_mgr_reqs_rx.next().await.unwrap() {
            PeerManagerRequest::OpenSubstream(peer, protocol, ch) => {
                assert_eq!(peer, seed_peer_id);
                assert_eq!(protocol, DISCOVERY_PROTOCOL_NAME);
                ch.send(Ok(dialer_substream)).unwrap();
            }
            req => {
                panic!("Unexpected request to peer manager: {:?}", req);
            }
        }

        // Receive DiscoveryMsg from actor. The message should contain only a note for the
        // sending peer since it doesn't yet have the note for the seed peer.
        let msg = recv_msg(listener_substream).await.unwrap();
        assert_eq!(1, msg.notes.len());
        assert_eq!(Vec::from(peer_id), msg.notes[0].peer_id);
        assert_eq!(addrs, get_addrs_from_note(&msg.notes[0]));
    };

    rt.block_on(f_peer_mgr.boxed().unit_error().compat())
        .unwrap();
}

#[test]
fn addr_update_includes_seed_addrs() {
    ::logger::try_init_for_testing();
    let mut rt = Runtime::new().unwrap();

    // Setup self.
    let peer_id = PeerId::random();
    let addrs = vec![Multiaddr::from_str("/ip4/127.0.0.1/tcp/9090").unwrap()];
    let (self_pub_keys, self_signer) = generate_network_pub_keys_and_signer(peer_id);

    // Setup seed.
    let seed_peer_info = gen_peer_info();
    let mut seed_peer_addrs = get_addrs_from_info(&seed_peer_info);
    let seed_peer_payload = gen_full_node_payload();
    let seed_peer_id = PeerId::random();
    let (seed_pub_keys, seed_signer) = generate_network_pub_keys_and_signer(seed_peer_id);
    let trusted_peers = Arc::new(RwLock::new(
        vec![(seed_peer_id, seed_pub_keys), (peer_id, self_pub_keys)]
            .into_iter()
            .collect(),
    ));

    // Setup discovery.
    let (_, mut conn_mgr_reqs_rx, mut peer_mgr_notifs_tx, _) = setup_discovery(
        &mut rt,
        peer_id,
        addrs,
        seed_peer_id,
        seed_peer_info,
        self_signer,
        trusted_peers,
    );

    // Fake connectivity manager and dialer.
    let f_peer_mgr = async move {
        // Connectivity manager receives addresses of the seed peer during bootstrap.
        expect_address_update(&mut conn_mgr_reqs_rx, seed_peer_id, &seed_peer_addrs[..]).await;

        // Notify discovery actor of inbound substream.
        let (dialer_substream, listener_substream) = MemorySocket::new_pair();
        peer_mgr_notifs_tx
            .send(PeerManagerNotification::NewInboundSubstream(
                seed_peer_id,
                NegotiatedSubstream {
                    protocol: ProtocolId::from_static(DISCOVERY_PROTOCOL_NAME),
                    substream: listener_substream,
                },
            ))
            .await
            .unwrap();
        // Wrap dialer substream in a framed substream.
        let mut dialer_substream =
            Framed::new(dialer_substream.compat(), UviBytes::<Bytes>::default()).sink_compat();

        // Send DiscoveryMsg consisting of the new seed peer's discovery note.
        // The discovery actor should send the addrs in the new seed peer note
        // _and_ the configured seed addrs to the connectivity manager.
        let new_seed_addrs = vec![Multiaddr::from_str("/ip4/127.0.0.1/tcp/9091").unwrap()];
        let new_seed_info = create_peer_info(new_seed_addrs.clone());
        let seed_note = create_note(&seed_signer, seed_peer_id, new_seed_info, seed_peer_payload);
        let mut msg = DiscoveryMsg::default();
        msg.notes.push(seed_note.clone());
        dialer_substream
            .send(msg.to_bytes().unwrap())
            .await
            .unwrap();

        // The addrs sent to connectivity manager should also include the
        // configured seed peer addrs for seed-peer-received notes.
        let mut expected_seed_addrs = new_seed_addrs.clone();
        expected_seed_addrs.append(&mut seed_peer_addrs);

        // Connectivity manager receives an update including the seed peer's new
        // note addrs and the original configured seed addrs.
        expect_address_update(
            &mut conn_mgr_reqs_rx,
            seed_peer_id,
            &expected_seed_addrs[..],
        )
        .await;
    };
    rt.block_on(f_peer_mgr.boxed().unit_error().compat())
        .unwrap();
}
