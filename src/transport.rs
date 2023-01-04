use crate::{
    channels::{SimpleBufferPool, TaskPoolRuntime},
    NetworkError,
};
use bevy::{log, tasks::IoTaskPool};
use bytes::Bytes;
use futures_lite::StreamExt;
use instant::{Duration, Instant};
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
};
use turbulence::{
    buffer::BufferPacketPool,
    message_channels::{MessageChannels, MessageChannelsBuilder},
    packet::PacketPool,
    packet_multiplexer::{IncomingMultiplexedPackets, MuxPacket, MuxPacketPool, PacketMultiplexer},
};

pub type Packet = Bytes;
pub type MultiplexedPacket = MuxPacket<<BufferPacketPool<SimpleBufferPool> as PacketPool>::Packet>;
pub type ConnectionChannelsBuilder =
    MessageChannelsBuilder<TaskPoolRuntime, MuxPacketPool<BufferPacketPool<SimpleBufferPool>>>;

#[cfg(all(feature = "client", feature = "server"))]
compile_error!("The bevy_disturbulence crate can't be compiled with both `client` and `server` features enabled");
#[cfg(all(not(feature = "client"), not(feature = "server")))]
compile_error!("The bevy_disturbulence crate must be compiled with either `client` or `server` feature enabled");

#[derive(Debug, Clone)]
pub struct PacketStats {
    pub packets_tx: usize,
    pub packets_rx: usize,
    pub bytes_tx: usize,
    pub bytes_rx: usize,
    pub last_tx: Instant,
    pub last_rx: Instant,
}

impl Default for PacketStats {
    fn default() -> Self {
        // default the last rx/tx to now.
        // not strictly true in use-udp mode, since we can "connect" without
        // exchanging packets. but always true for use-webrtc.
        let now = Instant::now();
        Self {
            packets_tx: 0,
            packets_rx: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            last_tx: now,
            last_rx: now,
        }
    }
}

impl PacketStats {
    fn add_tx(&mut self, num_bytes: usize) {
        self.packets_tx += 1;
        self.bytes_tx += num_bytes;
        self.last_tx = Instant::now();
    }
    fn add_rx(&mut self, num_bytes: usize) {
        self.packets_rx += 1;
        self.bytes_rx += num_bytes;
        self.last_rx = Instant::now();
    }
    // returns Duration since last (rx, tx)
    fn idle_durations(&self) -> (Duration, Duration) {
        let now = Instant::now();
        let rx = now.duration_since(self.last_rx);
        let tx = now.duration_since(self.last_tx);
        (rx, tx)
    }
}

pub trait Connection {
    fn remote_address(&self) -> Option<SocketAddr>;

    fn send(
        &mut self,
        payload: Packet,
    ) -> Result<(), naia_socket_shared::ChannelClosedError<Packet>>;

    fn receive(&mut self) -> Option<Result<Packet, NetworkError>>;

    fn build_channels(
        &mut self,
        builder_fn: &(dyn Fn(&mut ConnectionChannelsBuilder)),
        runtime: TaskPoolRuntime,
        pool: MuxPacketPool<BufferPacketPool<SimpleBufferPool>>,
    );

    fn channels(&mut self) -> Option<&mut MessageChannels>;

    fn channels_rx(&mut self) -> Option<&mut IncomingMultiplexedPackets<MultiplexedPacket>>;

    fn stats(&self) -> PacketStats;

    /// returns milliseconds since last (rx, tx)
    fn last_packet_timings(&self) -> (u128, u128);
}

#[cfg(feature = "server")]
pub struct ServerConnection {
    packet_rx: crossbeam_channel::Receiver<Result<Packet, NetworkError>>,
    sender: naia_server_socket::PacketSender,
    client_address: SocketAddr,
    stats: Arc<RwLock<PacketStats>>,

    channels: Option<MessageChannels>,
    channels_rx: Option<IncomingMultiplexedPackets<MultiplexedPacket>>,
}

#[cfg(feature = "server")]
impl ServerConnection {
    pub fn new(
        packet_rx: crossbeam_channel::Receiver<Result<Packet, NetworkError>>,
        sender: naia_server_socket::PacketSender,
        client_address: SocketAddr,
    ) -> Self {
        ServerConnection {
            packet_rx,
            sender,
            client_address,
            stats: Arc::new(RwLock::new(PacketStats::default())),
            channels: None,
            channels_rx: None,
        }
    }
}

#[cfg(feature = "server")]
impl Connection for ServerConnection {
    fn remote_address(&self) -> Option<SocketAddr> {
        Some(self.client_address)
    }

    fn stats(&self) -> PacketStats {
        self.stats.read().expect("stats lock poisoned").clone()
    }

    fn send(
        &mut self,
        payload: Packet,
    ) -> Result<(), naia_socket_shared::ChannelClosedError<Packet>> {
        self.stats
            .write()
            .expect("stats lock poisoned")
            .add_tx(payload.len());
        self.sender
            .send(&self.client_address, payload.as_ref())
            .map_err(|_err: naia_socket_shared::ChannelClosedError<()>| {
                naia_socket_shared::ChannelClosedError(payload)
            })
    }

    fn last_packet_timings(&self) -> (u128, u128) {
        let (rx_dur, tx_dur) = self
            .stats
            .read()
            .expect("stats lock poisoned")
            .idle_durations();
        (rx_dur.as_millis(), tx_dur.as_millis())
    }

    fn receive(&mut self) -> Option<Result<Packet, NetworkError>> {
        match self.packet_rx.try_recv() {
            Ok(payload) => match payload {
                Ok(packet) => {
                    self.stats
                        .write()
                        .expect("stats lock poisoned")
                        .add_rx(packet.len());
                    Some(Ok(packet))
                }
                Err(err) => Some(Err(err)),
            },
            Err(error) => match error {
                crossbeam_channel::TryRecvError::Empty => None,
                crossbeam_channel::TryRecvError::Disconnected => {
                    Some(Err(NetworkError::Disconnected))
                }
            },
        }
    }

    fn build_channels(
        &mut self,
        builder_fn: &(dyn Fn(&mut ConnectionChannelsBuilder)),
        runtime: TaskPoolRuntime,
        pool: MuxPacketPool<BufferPacketPool<SimpleBufferPool>>,
    ) {
        let mut builder = MessageChannelsBuilder::new(runtime, pool);
        builder_fn(&mut builder);

        let mut multiplexer = PacketMultiplexer::new();
        self.channels = Some(builder.build(&mut multiplexer));
        let (channels_rx, mut channels_tx) = multiplexer.start();
        self.channels_rx = Some(channels_rx);

        let sender = self.sender.clone();
        let client_address = self.client_address;
        let stats = self.stats.clone();

        let task = async move {
            loop {
                let Some(packet) = channels_tx.next().await else {
                    log::debug!("A server connection is closed, closing the channel builder task");
                    return;
                };
                stats
                    .write()
                    .expect("stats lock poisoned")
                    .add_tx(packet.len());
                // TODO: to confirm that this error handling makes sense.
                //
                // When a channel is closed, there's indeed no sense to do anything
                // but finish the task.
                //
                // I'm not so sure about the case when the channel is full though...
                // Possible reasons:
                // - something is blocking the async executor that naia is using
                // - the connection hasn't even been properly established (I had a weird
                //   case when I tried to send packets to the session socket, the queue
                //   just wasn't being consumed).
                // If we just skip the error without finishing the task, will the packet
                // multiplexer try to re-send packets? Do we want any sort of backpressure?
                if let Err(err) = sender.send(&client_address, packet.as_ref()) {
                    log::error!("Failed to send a packet: {err}");
                    return;
                }
            }
        };

        // The task will finish as soon as `channels_rx` is dropped, so we are safe to detach here.
        IoTaskPool::get().spawn(task).detach();
    }

    fn channels(&mut self) -> Option<&mut MessageChannels> {
        self.channels.as_mut()
    }

    fn channels_rx(&mut self) -> Option<&mut IncomingMultiplexedPackets<MultiplexedPacket>> {
        self.channels_rx.as_mut()
    }
}

#[cfg(feature = "client")]
pub struct ClientConnection {
    socket: naia_client_socket::Socket,
    stats: Arc<RwLock<PacketStats>>,

    channels: Option<MessageChannels>,
    channels_rx: Option<IncomingMultiplexedPackets<MultiplexedPacket>>,
}

#[cfg(feature = "client")]
impl ClientConnection {
    pub fn new(socket: naia_client_socket::Socket) -> Self {
        ClientConnection {
            socket,
            stats: Arc::new(RwLock::new(PacketStats::default())),
            channels: None,
            channels_rx: None,
        }
    }
}

#[cfg(feature = "client")]
impl Connection for ClientConnection {
    fn remote_address(&self) -> Option<SocketAddr> {
        None
    }

    fn stats(&self) -> PacketStats {
        self.stats.read().expect("stats lock poisoned").clone()
    }

    fn last_packet_timings(&self) -> (u128, u128) {
        let (rx_dur, tx_dur) = self
            .stats
            .read()
            .expect("stats lock poisoned")
            .idle_durations();
        (rx_dur.as_millis(), tx_dur.as_millis())
    }

    fn send(
        &mut self,
        payload: Packet,
    ) -> Result<(), naia_socket_shared::ChannelClosedError<Packet>> {
        self.stats
            .write()
            .expect("stats lock poisoned")
            .add_tx(payload.len());
        self.socket.packet_sender().send(payload.as_ref()).map_err(
            |_err: naia_socket_shared::ChannelClosedError<()>| {
                naia_socket_shared::ChannelClosedError(payload)
            },
        )
    }

    fn receive(&mut self) -> Option<Result<Packet, NetworkError>> {
        match self.socket.packet_receiver().receive() {
            Ok(event) => event.map(|packet| {
                self.stats
                    .write()
                    .expect("stats lock poisoned")
                    .add_rx(packet.len());
                Ok(Packet::copy_from_slice(packet))
            }),
            Err(err) => Some(Err(NetworkError::IoError(Box::new(err)))),
        }
    }

    fn build_channels(
        &mut self,
        builder_fn: &(dyn Fn(&mut ConnectionChannelsBuilder)),
        runtime: TaskPoolRuntime,
        pool: MuxPacketPool<BufferPacketPool<SimpleBufferPool>>,
    ) {
        let mut builder = MessageChannelsBuilder::new(runtime, pool);
        builder_fn(&mut builder);

        let mut multiplexer = PacketMultiplexer::new();
        self.channels = Some(builder.build(&mut multiplexer));
        let (channels_rx, mut channels_tx) = multiplexer.start();
        self.channels_rx = Some(channels_rx);

        let sender = self.socket.packet_sender();
        let stats = self.stats.clone();

        let task = async move {
            loop {
                let Some(packet) = channels_tx.next().await else {
                    log::debug!("A client connection is closed, closing the channel builder task");
                    return;
                };

                stats
                    .write()
                    .expect("stats lock poisoned")
                    .add_tx(packet.len());
                // TODO: to confirm that this error handling makes sense.
                //
                // When a channel is closed, there's indeed no sense to do anything
                // but finish the task.
                //
                // I'm not so sure about the case when the channel is full though...
                // Possible reasons:
                // - something is blocking the async executor that naia is using
                // - the connection hasn't even been properly established (I had a weird
                //   case when I tried to send packets to the session socket, the queue
                //   just wasn't being consumed).
                // If we just skip the error without finishing the task, will the packet
                // multiplexer try to re-send packets? Do we want any sort of backpressure?
                if let Err(err) = sender.send(packet.as_ref()) {
                    log::error!("Failed to send a packet: {err}");
                    return;
                }
            }
        };

        // The task will finish as soon as `channels_rx` is dropped, so we are safe to detach here.
        IoTaskPool::get().spawn_local(task).detach();
    }

    fn channels(&mut self) -> Option<&mut MessageChannels> {
        self.channels.as_mut()
    }

    fn channels_rx(&mut self) -> Option<&mut IncomingMultiplexedPackets<MultiplexedPacket>> {
        self.channels_rx.as_mut()
    }
}
