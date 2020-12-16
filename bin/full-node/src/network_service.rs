// Substrate-lite
// Copyright (C) 2019-2020  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Background network service.
//!
//! The [`NetworkService`] manages background tasks dedicated to connecting to other nodes.
//! Importantly, its design is oriented towards the particular use case of the full node.
//!
//! The [`NetworkService`] spawns one background task (using the [`Config::tasks_executor`]) for
//! each active TCP socket, plus one for each TCP listening socket. Messages are exchanged between
//! the service and these background tasks.

// TODO: doc
// TODO: re-review this once finished

use core::{cmp, pin::Pin, time::Duration};
use futures::prelude::*;
use std::{collections::HashMap, io, net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Instant};
use substrate_lite::network::{
    connection,
    multiaddr::{Multiaddr, Protocol},
    peer_id::PeerId,
    protocol, service,
};
use tracing::Instrument as _;

mod with_buffers;

/// Configuration for a [`NetworkService`].
pub struct Config {
    /// Closure that spawns background tasks.
    pub tasks_executor: Box<dyn FnMut(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Addresses to listen for incoming connections.
    pub listen_addresses: Vec<Multiaddr>,

    /// List of block chains to be connected to.
    pub chains: Vec<ChainConfig>,

    /// Key used for the encryption layer.
    /// This is a Noise static key, according to the Noise specifications.
    /// Signed using the actual libp2p key.
    pub noise_key: connection::NoiseKey,
}

/// Configuration for one chain.
pub struct ChainConfig {
    /// List of node identities and addresses that are known to belong to the chain's peer-to-pee
    /// network.
    pub bootstrap_nodes: Vec<(PeerId, Multiaddr)>,

    /// Hash of the genesis block of the chain. Sent to other nodes in order to determine whether
    /// the chains match.
    pub genesis_block_hash: [u8; 32],

    /// Number and hash of the current best block. Can later be updated with // TODO: which function?
    pub best_block: (u64, [u8; 32]),

    /// Identifier of the chain to connect to.
    ///
    /// Each blockchain has (or should have) a different "protocol id". This value identifies the
    /// chain, so as to not introduce conflicts in the networking messages.
    pub protocol_id: String,
}

/// Event generated by [`NetworkService::next_event`].
#[derive(Debug)]
pub enum Event {
    Connected {
        chain_index: usize,
        peer_id: PeerId,
        best_block_number: u64,
    },
    Disconnected {
        chain_index: usize,
        peer_id: PeerId,
    },
    BlockAnnounce {
        chain_index: usize,
        peer_id: PeerId,
        announce: service::EncodedBlockAnnounce,
    },
}

pub struct NetworkService {
    /// Fields behind a mutex.
    ///
    /// A regular `Mutex` is used in order to avoid futures cancellation issues.
    guarded: parking_lot::Mutex<Guarded>,

    /// Data structure holding the entire state of the networking.
    network: service::ChainNetwork<Instant, (), ()>,
}

/// Fields of [`NetworkService`] behind a mutex.
struct Guarded {
    /// See [`Config::tasks_executor`].
    tasks_executor: Box<dyn FnMut(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,
}

impl NetworkService {
    /// Initializes the network service with the given configuration.
    pub async fn new(mut config: Config) -> Result<Arc<Self>, InitError> {
        // For each listening address in the configuration, create a background task dedicated to
        // listening on that address.
        for listen_address in config.listen_addresses {
            // Try to parse the requested address and create the corresponding listening socket.
            let tcp_listener: async_std::net::TcpListener = {
                let mut iter = listen_address.iter();
                let proto1 = match iter.next() {
                    Some(p) => p,
                    None => return Err(InitError::BadListenMultiaddr(listen_address)),
                };
                let proto2 = match iter.next() {
                    Some(p) => p,
                    None => return Err(InitError::BadListenMultiaddr(listen_address)),
                };

                if iter.next().is_some() {
                    return Err(InitError::BadListenMultiaddr(listen_address));
                }

                let addr = match (proto1, proto2) {
                    (Protocol::Ip4(ip), Protocol::Tcp(port)) => SocketAddr::from((ip, port)),
                    (Protocol::Ip6(ip), Protocol::Tcp(port)) => SocketAddr::from((ip, port)),
                    _ => return Err(InitError::BadListenMultiaddr(listen_address)),
                };

                match async_std::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(err) => {
                        return Err(InitError::ListenerIo(listen_address, err));
                    }
                }
            };

            // Spawn a background task dedicated to this listener.
            (config.tasks_executor)(Box::pin(
                async move {
                    loop {
                        // TODO: add a way to immediately interrupt the listener if the network service is destroyed (or fails to create altogether), in order to immediately liberate the port

                        let (socket, _addr) = match tcp_listener.accept().await {
                            Ok(v) => v,
                            Err(_) => {
                                // Errors here can happen if the accept failed, for example if no file
                                // descriptor is available.
                                // A wait is added in order to avoid having a busy-loop failing to
                                // accept connections.
                                futures_timer::Delay::new(Duration::from_secs(2)).await;
                                continue;
                            }
                        };

                        todo!() // TODO: report new connection
                    }
                }
                .instrument(
                    tracing::debug_span!(parent: None, "listener", address = %listen_address),
                ),
            ))
        }

        // TODO: code is messy
        let mut known_nodes =
            Vec::with_capacity(config.chains.iter().map(|c| c.bootstrap_nodes.len()).sum());
        let mut chains = Vec::with_capacity(config.chains.len());
        for chain in config.chains {
            let mut bootstrap_nodes = Vec::with_capacity(chain.bootstrap_nodes.len());
            for (peer_id, addr) in chain.bootstrap_nodes {
                bootstrap_nodes.push(known_nodes.len());
                known_nodes.push(((), peer_id, addr));
            }

            chains.push(service::ChainConfig {
                bootstrap_nodes,
                in_slots: 25,
                out_slots: 25,
                protocol_id: chain.protocol_id,
                best_hash: chain.best_block.1,
                best_number: chain.best_block.0,
                genesis_hash: chain.genesis_block_hash,
                role: protocol::Role::Full,
            });
        }

        // Initialize the network service.
        let network_service = Arc::new(NetworkService {
            guarded: parking_lot::Mutex::new(Guarded {
                tasks_executor: config.tasks_executor,
            }),
            network: service::ChainNetwork::new(service::Config {
                chains,
                known_nodes,
                listen_addresses: Vec::new(), // TODO:
                noise_key: config.noise_key,
                pending_api_events_buffer_size: NonZeroUsize::new(64).unwrap(),
                randomness_seed: rand::random(),
            }),
        });

        /*
        // Spawn tasks dedicated to the Kademlia discovery.
        (network_service.guarded.try_lock().unwrap().tasks_executor)(Box::pin({
            let network_service = Arc::downgrade(&network_service);
            async move {
                let mut next_discovery = Duration::from_secs(5);

                loop {
                    futures_timer::Delay::new(next_discovery).await;
                    next_discovery = cmp::min(next_discovery * 2, Duration::from_secs(120));

                    let network_service = match network_service.upgrade() {
                        Some(ns) => ns,
                        None => {
                            tracing::debug!("discovery-finish");
                            return;
                        }
                    };

                    match network_service
                        .network
                        .kademlia_discovery_round(Instant::now(), 0)
                        .await
                    {
                        Ok(insert) => {
                            insert
                                .insert(|_| ())
                                .instrument(tracing::debug_span!("insert"))
                                .await
                        }
                        Err(error) => tracing::debug!(%error, "discovery-error"),
                    }
                }
            }
            .instrument(tracing::debug_span!(parent: None, "kademlia-discovery"))
        }));
        */

        (network_service.guarded.try_lock().unwrap().tasks_executor)(Box::pin({
            let network_service = network_service.clone();
            async move {
                // TODO: stop the task if the network service is destroyed
                loop {
                    network_service
                        .network
                        .next_substream()
                        .await
                        .open(Instant::now())
                        .await;
                }
            }
            .instrument(tracing::debug_span!(parent: None, "substreams-open"))
        }));

        Ok(network_service)
    }

    /// Returns the number of established TCP connections, both incoming and outgoing.
    pub async fn num_established_connections(&self) -> usize {
        self.network.num_established_connections().await
    }

    /// Sends a blocks request to the given peer.
    // TODO: more docs
    // TODO: proper error type
    #[tracing::instrument(skip(self))]
    pub async fn blocks_request(
        self: Arc<Self>,
        target: PeerId,
        chain_index: usize,
        config: protocol::BlocksRequestConfig,
    ) -> Result<Vec<protocol::BlockData>, service::BlocksRequestError> {
        self.network
            .blocks_request(Instant::now(), target, chain_index, config)
            .await
    }

    async fn warp_sync(
        &self,
        peer_id: PeerId,
        hash: [u8; 32],
        genesis_chain_infomation: &substrate_lite::chain::chain_information::ChainInformation,
    ) -> Result<(), substrate_lite::finality::justification::verify::Error> {
        let warp_sync_response = self
            .network
            .grandpa_warp_sync_request(Instant::now(), peer_id, 0, hash)
            .await
            .unwrap();

        let mut authorities_list: Vec<[u8; 32]> = match &genesis_chain_infomation.finality {
            substrate_lite::chain::chain_information::ChainInformationFinality::Grandpa {
                finalized_triggered_authorities, ..
            } => {
                finalized_triggered_authorities.iter().map(|auth| auth.public_key).collect()
            },
            _ => unimplemented!()
        };

        for (i, fragment) in warp_sync_response.iter().enumerate() {
            let authorities_set_id = i as u64;

            let config = substrate_lite::finality::justification::verify::Config {
                justification: (&fragment.justification).into(),
                authorities_list: authorities_list.iter(),
                authorities_set_id,
            };

            substrate_lite::finality::justification::verify::verify(config)?;

            authorities_list = fragment.header.digest.logs()
                .filter_map(|log_item| {
                    match log_item {
                        substrate_lite::header::DigestItemRef::GrandpaConsensus(grandpa_log_item) => {
                            match grandpa_log_item {
                                substrate_lite::header::GrandpaConsensusLogRef::ScheduledChange(change)
                                | substrate_lite::header::GrandpaConsensusLogRef::ForcedChange { change, .. } => {
                                    Some(change.next_authorities)
                                },
                                _ => None
                            }
                        },
                        _ => None
                    }
                })
                .flat_map(|next_authorities| next_authorities)
                .map(|authority| *authority.public_key)
                .collect();
        }

        println!("Verified {} warp sync fragments", warp_sync_response.len());

        Ok(())
    }

    /// Returns the next event that happens in the network service.
    ///
    /// If this method is called multiple times simultaneously, the events will be distributed
    /// amongst the different calls in an unpredictable way.
    #[tracing::instrument(skip(self))]
    pub async fn next_event(
        self: &Arc<Self>,
        use_me: [u8; 32],
        genesis_chain_infomation: &substrate_lite::chain::chain_information::ChainInformation,
    ) -> Event {
        loop {
            match self.network.next_event().await {
                service::Event::Connected(peer_id) => {
                    tracing::debug!(%peer_id, "connected");
                    self.warp_sync(peer_id, use_me, genesis_chain_infomation).await.unwrap();
                }
                service::Event::Disconnected {
                    peer_id,
                    chain_indices,
                } => {
                    tracing::debug!(%peer_id, "disconnected");
                    if !chain_indices.is_empty() {
                        debug_assert_eq!(chain_indices.len(), 1); // TODO: not implemented
                        return Event::Disconnected {
                            chain_index: chain_indices[0],
                            peer_id,
                        };
                    }
                }
                service::Event::StartConnect { id, multiaddr } => {
                    let span = tracing::debug_span!("start-connect", ?id, %multiaddr);
                    let _enter = span.enter();

                    // Convert the `multiaddr` (typically of the form `/ip4/a.b.c.d/tcp/d`) into
                    // a `Future<dyn Output = Result<TcpStream, ...>>`.
                    let socket = match multiaddr_to_socket(&multiaddr) {
                        Ok(socket) => socket,
                        Err(_) => {
                            tracing::debug!(%multiaddr, "not-tcp");
                            self.network.pending_outcome_err(id).await;
                            continue;
                        }
                    };

                    let mut guarded = self.guarded.lock();
                    (guarded.tasks_executor)(Box::pin(
                        connection_task(socket, self.clone(), id).instrument(
                            tracing::trace_span!(parent: None, "connection", address = %multiaddr),
                        ),
                    ));
                }
                service::Event::BlockAnnounce {
                    chain_index,
                    peer_id,
                    announce,
                } => {
                    tracing::debug!(%chain_index, %peer_id, ?announce, "block-announce");
                    return Event::BlockAnnounce {
                        chain_index,
                        peer_id,
                        announce,
                    };
                }
                service::Event::ChainConnected {
                    peer_id,
                    chain_index,
                    best_number,
                    ..
                } => {
                    return Event::Connected {
                        peer_id,
                        chain_index,
                        best_block_number: best_number,
                    };
                }
                service::Event::ChainDisconnected {
                    peer_id,
                    chain_index,
                } => {
                    return Event::Disconnected {
                        chain_index,
                        peer_id,
                    };
                }
            }
        }
    }
}

/// Error when initializing the network service.
#[derive(Debug, derive_more::Display)]
pub enum InitError {
    /// I/O error when initializing a listener.
    #[display(fmt = "I/O error when creating listener for {}: {}", _0, _1)]
    ListenerIo(Multiaddr, io::Error),
    /// A listening address passed through the configuration isn't valid.
    BadListenMultiaddr(Multiaddr),
}

/// Asynchronous task managing a specific TCP connection.
#[tracing::instrument(skip(tcp_socket, network_service))]
async fn connection_task(
    tcp_socket: impl Future<Output = Result<async_std::net::TcpStream, io::Error>>,
    network_service: Arc<NetworkService>,
    id: service::PendingId,
) {
    // Finishing ongoing connection process.
    let tcp_socket = match tcp_socket.await {
        Ok(s) => s,
        Err(_) => {
            network_service.network.pending_outcome_err(id).await;
            return;
        }
    };

    let id = network_service.network.pending_outcome_ok(id, ()).await;

    // The Nagle algorithm, implemented in the kernel, consists in buffering the data to be sent
    // out and waiting a bit before actually sending it out, in order to potentially merge
    // multiple writes in a row into one packet. In the implementation below, it is guaranteed
    // that the buffer in `WithBuffers` is filled with as much data as possible before the
    // operating system gets involved. As such, we disable the Nagle algorithm, in order to avoid
    // adding an artificial delay to all sends.
    let _ = tcp_socket.set_nodelay(true);

    // The socket is wrapped around a `WithBuffers` object containing a read buffer and a write
    // buffer. These are the buffers whose pointer is passed to `read(2)` and `write(2)` when
    // reading/writing the socket.
    let tcp_socket = with_buffers::WithBuffers::new(tcp_socket);
    futures::pin_mut!(tcp_socket);

    // Set to a timer after which the state machine of the connection needs an update.
    let mut poll_after: futures_timer::Delay;

    loop {
        let (read_buffer, write_buffer) = match tcp_socket.buffers() {
            Ok(b) => b,
            Err(error) => {
                tracing::info!(%error, "task-finished");
                // TODO: report disconnect to service
                return;
            }
        };

        let now = Instant::now();

        // TODO: hacky code
        struct Waker(std::sync::Mutex<(bool, Option<std::task::Waker>)>);
        impl futures::task::ArcWake for Waker {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                let mut lock = arc_self.0.lock().unwrap();
                lock.0 = true;
                if let Some(w) = lock.1.take() {
                    w.wake();
                }
            }
        }

        let waker = Arc::new(Waker(std::sync::Mutex::new((false, None))));

        let read_write = match network_service
            .network
            .read_write(
                id,
                now,
                read_buffer.map(|b| b.0),
                write_buffer.unwrap(),
                &mut std::task::Context::from_waker(&*futures::task::waker_ref(&waker)),
            )
            .await
        {
            Ok(rw) => rw,
            Err(error) => {
                tracing::info!(%error, "task-finished");
                return;
            }
        };

        if read_write.read_bytes != 0 || read_write.written_bytes != 0 || read_write.write_close {
            tracing::event!(
                tracing::Level::TRACE,
                read = read_write.read_bytes,
                written = read_write.written_bytes,
                "wake-up" = ?read_write.wake_up_after,  // TODO: ugly display
                "write-close" = read_write.write_close,
            );
        }

        if read_write.write_close && read_buffer.is_none() {
            // Make sure to finish closing the TCP socket.
            tcp_socket
                .flush_close()
                .instrument(tracing::debug_span!("flush-close"))
                .await;
            tracing::info!("task-finished");
            return;
        }

        if read_write.write_close && !tcp_socket.is_closed() {
            tcp_socket.close();
            tracing::info!("write-closed");
        }

        if let Some(wake_up) = read_write.wake_up_after {
            if wake_up > now {
                let dur = wake_up - now;
                poll_after = futures_timer::Delay::new(dur);
            } else {
                poll_after = futures_timer::Delay::new(Duration::from_secs(0));
            }
        } else {
            poll_after = futures_timer::Delay::new(Duration::from_secs(3600));
        }

        tcp_socket.advance(read_write.read_bytes, read_write.written_bytes);

        // TODO: maybe optimize the code below so that multiple messages are pulled from `to_connection` at once

        futures::select! {
            _ = tcp_socket.as_mut().process().fuse() => {
                tracing::event!(
                    tracing::Level::TRACE,
                    "socket-ready"
                );
            },
            _ = future::poll_fn(move |cx| {
                let mut lock = waker.0.lock().unwrap();
                if lock.0 {
                    return std::task::Poll::Ready(());
                }
                match lock.1 {
                    Some(ref w) if w.will_wake(cx.waker()) => {}
                    _ => lock.1 = Some(cx.waker().clone()),
                }
                std::task::Poll::Pending
            }).fuse() => {}
            _ = (&mut poll_after).fuse() => { // TODO: no, ref mut + fuse() = probably panic
                // Nothing to do, but guarantees that we loop again.
                tracing::event!(
                    tracing::Level::TRACE,
                    "timer-ready"
                );
            }
        }
    }
}

/// Builds a future that connects to the given multiaddress. Returns an error if the multiaddress
/// protocols aren't supported.
fn multiaddr_to_socket(
    addr: &Multiaddr,
) -> Result<impl Future<Output = Result<async_std::net::TcpStream, io::Error>>, ()> {
    let mut iter = addr.iter();
    let proto1 = iter.next().ok_or(())?;
    let proto2 = iter.next().ok_or(())?;

    if iter.next().is_some() {
        return Err(());
    }

    // Ensure ahead of time that the multiaddress is supported.
    match (&proto1, &proto2) {
        (Protocol::Ip4(_), Protocol::Tcp(_))
        | (Protocol::Ip6(_), Protocol::Tcp(_))
        | (Protocol::Dns(_), Protocol::Tcp(_))
        | (Protocol::Dns4(_), Protocol::Tcp(_))
        | (Protocol::Dns6(_), Protocol::Tcp(_)) => {}
        _ => return Err(()),
    }

    let proto1 = proto1.acquire();
    let proto2 = proto2.acquire();

    Ok(async move {
        match (proto1, proto2) {
            (Protocol::Ip4(ip), Protocol::Tcp(port)) => {
                async_std::net::TcpStream::connect(SocketAddr::new(ip.into(), port)).await
            }
            (Protocol::Ip6(ip), Protocol::Tcp(port)) => {
                async_std::net::TcpStream::connect(SocketAddr::new(ip.into(), port)).await
            }
            // TODO: for DNS, do things a bit more explicitly? with for example a library that does the resolution?
            // TODO: differences between DNS, DNS4, DNS6 not respected
            (Protocol::Dns(addr), Protocol::Tcp(port))
            | (Protocol::Dns4(addr), Protocol::Tcp(port))
            | (Protocol::Dns6(addr), Protocol::Tcp(port)) => {
                async_std::net::TcpStream::connect((&*addr, port)).await
            }
            _ => unreachable!(),
        }
    })
}
