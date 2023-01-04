#[cfg(feature = "server")]
pub use naia_server_socket::ServerAddrs;
pub use naia_socket_shared::SocketConfig;
pub use transport::{Connection, ConnectionChannelsBuilder, Packet};
pub use turbulence::{
    message_channels::{MessageChannelMode, MessageChannelSettings},
    reliable_channel::Settings as ReliableChannelSettings,
    unreliable_channel::Settings as UnreliableChannelSettings,
};

mod channels;
mod transport;

use crate::{
    channels::{SimpleBufferPool, TaskPoolRuntime},
    transport::MultiplexedPacket,
};
use bevy::{
    app::{App, CoreStage, Plugin},
    ecs::{
        event::Events,
        schedule::{StageLabel, SystemStage},
        system::ResMut,
    },
    log,
    prelude::NonSendMut,
    time::FixedTimestep,
    utils::HashMap,
};
use naia_socket_shared::ChannelClosedError;
use std::{error::Error, fmt::Debug};
use turbulence::{
    buffer::BufferPacketPool,
    message_channels::ChannelMessage,
    packet::{Packet as PoolPacket, PacketPool, MAX_PACKET_LEN},
    packet_multiplexer::{IncomingTrySendError, MuxPacketPool},
};

pub type ConnectionHandle = u32;

#[derive(Debug, Hash, PartialEq, Eq, Clone, StageLabel)]
struct SendHeartbeatsStage;

#[derive(Default)]
pub struct NetworkingPlugin {
    pub socket_config: SocketConfig,
    pub message_flushing_strategy: MessageFlushingStrategy,
    /// Disconnect if no packets received in this number of milliseconds
    pub idle_timeout_millis: Option<usize>,
    /// Should we automatically send heartbeat packets if no other packets have been sent?
    /// these are sent silently, and discarded, so you won't see them in your bevy systems.
    /// if auto_heartbeat_ms elapses, and we haven't sent anything else in that time, we send one.
    pub auto_heartbeat_millis: Option<usize>,
    /// FixedTimestep for the `heartbeats_and_timeouts` system which checks for idle connections
    /// and sends heartbeats. Does not need to be every frame.
    ///
    /// The `heartbeats_and_timeouts` system is only added if `idle_timeout_ms` or `auto_heartbeat_ms` are specified.
    ///
    /// Default if None: 0.5 secs
    pub heartbeats_and_timeouts_timestep_in_seconds: Option<f64>,
}

impl Plugin for NetworkingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_non_send_resource(NetworkResource::new(
            self.socket_config.clone(),
            self.message_flushing_strategy,
            self.idle_timeout_millis,
            self.auto_heartbeat_millis,
        ))
        .add_event::<NetworkEvent>()
        .add_system(receive_packets_system);
        if self.idle_timeout_millis.is_some() || self.auto_heartbeat_millis.is_some() {
            // heartbeats and timeouts checking/sending only runs infrequently:
            app.add_stage_after(
                CoreStage::Update,
                SendHeartbeatsStage,
                SystemStage::parallel()
                    .with_run_criteria(
                        FixedTimestep::step(
                            self.heartbeats_and_timeouts_timestep_in_seconds
                                .unwrap_or(0.5),
                        )
                        .with_label("HEARTBEAT"),
                    )
                    .with_system(heartbeats_and_timeouts_system),
            );
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum SendError<T> {
    NaiaSendError(naia_socket_shared::ChannelClosedError<T>),
    InvalidConnectionId(T),
}

impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

impl<T> std::fmt::Display for SendError<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "{}",
            match self {
                SendError::NaiaSendError(err) => format!("naia send error: {err}"),
                SendError::InvalidConnectionId(_) => "connection id doesn't exist".to_string(),
            }
        )
    }
}

#[cfg(feature = "server")]
type ServerChannels =
    HashMap<std::net::SocketAddr, crossbeam_channel::Sender<Result<Packet, NetworkError>>>;

#[cfg(feature = "server")]
struct ServerListener {
    socket: naia_server_socket::Socket,
    channels: ServerChannels,
}

type ChannelsBuilderFunction = Box<dyn Fn(&mut ConnectionChannelsBuilder) + Send + Sync>;

pub struct NetworkResource {
    socket_config: SocketConfig,

    connection_sequence: u32,
    pub connections: HashMap<ConnectionHandle, Box<dyn Connection>>,

    #[cfg(feature = "server")]
    server_listeners: HashMap<std::net::SocketAddr, ServerListener>,
    #[cfg(feature = "client")]
    pending_connections: Vec<ConnectionHandle>,

    runtime: TaskPoolRuntime,
    packet_pool: MuxPacketPool<BufferPacketPool<SimpleBufferPool>>,
    channels_builder_fn: Option<ChannelsBuilderFunction>,
    message_flushing_strategy: MessageFlushingStrategy,
    idle_timeout_ms: Option<usize>,
    auto_heartbeat_ms: Option<usize>,
}

#[derive(Debug)]
pub enum NetworkEvent {
    Connected(ConnectionHandle),
    Disconnected(ConnectionHandle),
    Packet(ConnectionHandle, Packet),
    Error(ConnectionHandle, NetworkError),
}

#[derive(Debug)]
pub enum NetworkError {
    TurbulenceChannelError(IncomingTrySendError<MultiplexedPacket>),
    IoError(Box<dyn Error + Sync + Send>),
    /// if we haven't seen a packet for the specified timeout
    MissedHeartbeat,
    Disconnected,
}

/// Turbulence will coalesce multiple small messages into a single packet when flush is called.
/// the default is `OnEverySend` - flushing after each message, which bypasses the coalescing.
/// You probably want to call flush once per tick instead, in your own system.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MessageFlushingStrategy {
    /// OnEverySend - flush immediately after calling send_message or send_broadcast.
    /// turbulence will never have a chance to coalesce multiple messages into a packet.
    OnEverySend,

    /// Never - you will want a system in (eg) PostUpdate which calls channels.flush for every channel type
    /// eg:
    ///
    /// pub fn flush_channels(mut net: ResMut<NetworkResource>) {
    ///     for (_handle, connection) in net.connections.iter_mut() {
    ///         let channels = connection.channels().unwrap();
    ///         channels.flush::<ClientNetMessage>();
    ///         channels.flush::<MyOtherMessageType>();
    ///         channels.flush::<AllMyTypesHere>();
    ///         ...
    ///     }
    /// }
    /// ...
    /// builder.add_system_to_stage(CoreStage::PostUpdate, flush_channels.system());
    ///
    Never,
}

impl Default for MessageFlushingStrategy {
    fn default() -> MessageFlushingStrategy {
        MessageFlushingStrategy::OnEverySend
    }
}

impl NetworkResource {
    pub fn new(
        socket_config: SocketConfig,
        message_flushing_strategy: MessageFlushingStrategy,
        idle_timeout_ms: Option<usize>,
        auto_heartbeat_ms: Option<usize>,
    ) -> Self {
        let packet_pool = MuxPacketPool::new(BufferPacketPool::new(SimpleBufferPool(
            MAX_PACKET_LEN as usize,
        )));

        NetworkResource {
            connection_sequence: 0,
            connections: HashMap::new(),
            socket_config,
            #[cfg(feature = "server")]
            server_listeners: HashMap::new(),
            #[cfg(feature = "client")]
            pending_connections: Vec::new(),
            runtime: TaskPoolRuntime,
            packet_pool,
            channels_builder_fn: None,
            message_flushing_strategy,
            idle_timeout_ms,
            auto_heartbeat_ms,
        }
    }

    #[cfg(feature = "server")]
    pub fn listen(&mut self, server_addrs: &naia_server_socket::ServerAddrs) {
        let mut socket = naia_server_socket::Socket::new(&self.socket_config);
        socket.listen(server_addrs);
        self.server_listeners.insert(
            server_addrs.session_listen_addr,
            ServerListener {
                socket,
                channels: HashMap::new(),
            },
        );
    }

    #[cfg(feature = "server")]
    pub fn close(&mut self, session_listen_addr: std::net::SocketAddr) {
        self.server_listeners.remove(&session_listen_addr);
    }

    #[cfg(feature = "client")]
    fn process_client_connections(&mut self, network_events: &mut Events<NetworkEvent>) {
        for connection in std::mem::take(&mut self.pending_connections) {
            network_events.send(NetworkEvent::Connected(connection));
        }
    }

    #[cfg(feature = "server")]
    fn process_server_connections(&mut self, network_events: &mut Events<NetworkEvent>) {
        for listener in self.server_listeners.values_mut() {
            let mut packet_receiver = listener.socket.packet_receiver();
            let packet_sender = listener.socket.packet_sender();
            loop {
                match packet_receiver.receive() {
                    Ok(Some((address, packet))) => {
                        let message = String::from_utf8_lossy(packet);
                        log::debug!("Server recv <- {}:{}: {}", address, packet.len(), message);

                        // Try to forward to a channel. If unsuccessful, we'll create a new one.
                        let needs_new_channel = match listener
                            .channels
                            .get(&address)
                            .map(|channel| channel.send(Ok(Packet::copy_from_slice(packet))))
                        {
                            Some(Ok(())) => false,
                            Some(Err(crossbeam_channel::SendError(_packet))) => {
                                log::warn!("Server can't send to channel, recreating");
                                // If we can't send to a channel, it's disconnected.
                                // We need to re-create the channel and re-try sending the message.
                                true
                            }
                            // This is a new connection, so we need to create a channel.
                            None => true,
                        };

                        if !needs_new_channel {
                            return;
                        }

                        let (packet_tx, packet_rx) =
                            crossbeam_channel::unbounded::<Result<Packet, NetworkError>>();
                        match packet_tx.send(Ok(Packet::copy_from_slice(packet))) {
                            Ok(()) => {
                                // It makes sense to store the connection only if the channel healthy.
                                let mut connection = transport::ServerConnection::new(
                                    packet_rx,
                                    packet_sender.clone(),
                                    address,
                                );
                                if let Some(channels_builder_fn) = self.channels_builder_fn.as_ref()
                                {
                                    connection.build_channels(
                                        channels_builder_fn,
                                        self.runtime.clone(),
                                        self.packet_pool.clone(),
                                    );
                                }
                                self.connections
                                    .insert(self.connection_sequence, Box::new(connection));
                                let handle = self.connection_sequence;
                                self.connection_sequence = self
                                    .connection_sequence
                                    .checked_add(1)
                                    .expect("reached the max connections number");
                                listener.channels.insert(address, packet_tx);
                                network_events.send(NetworkEvent::Connected(handle));
                            }
                            Err(error) => {
                                // This is unlikely to happen for a newly created channel,
                                // but if for some strange reason it does, we'll have to drop the message.
                                log::error!("Server Send Error (retry): {}", error);
                            }
                        }
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(error) => {
                        log::error!("Server Receive Error: {}", error);
                    }
                }
            }
        }
    }

    #[cfg(feature = "client")]
    pub fn connect(&mut self, server_session_url: &str) {
        let mut socket = naia_client_socket::Socket::new(&self.socket_config);
        let server_session_url = server_session_url.to_string();
        socket.connect(&server_session_url);

        let mut connection = transport::ClientConnection::new(socket);
        let handle = self.connection_sequence;
        if let Some(channels_builder_fn) = self.channels_builder_fn.as_ref() {
            log::debug!("Building channels (connection {handle})");
            connection.build_channels(
                channels_builder_fn,
                self.runtime.clone(),
                self.packet_pool.clone(),
            );
        }
        self.connections.insert(handle, Box::new(connection));
        self.connection_sequence = self
            .connection_sequence
            .checked_add(1)
            .expect("reached the max connections number");
        self.pending_connections.push(handle);
    }

    pub fn disconnect(&mut self, handle: ConnectionHandle) {
        if let Some(connection) = self.connections.remove(&handle) {
            log::debug!(
                "Disconnected connection {} (address: {:?})",
                handle,
                connection.remote_address()
            );
            #[cfg(feature = "server")]
            if let Some(addr) = connection.remote_address() {
                for listener in self.server_listeners.values_mut() {
                    if listener.channels.remove(&addr).is_some() {
                        break;
                    }
                }
            } else {
                log::error!("Failed to remove a server channel (connection {})", handle);
            }
        }
    }

    pub fn send(
        &mut self,
        handle: ConnectionHandle,
        payload: Packet,
    ) -> Result<(), SendError<Packet>> {
        match self.connections.get_mut(&handle) {
            Some(connection) => connection.send(payload).map_err(SendError::NaiaSendError),
            None => Err(SendError::InvalidConnectionId(payload)),
        }
    }

    pub fn broadcast(
        &mut self,
        payload: &Packet,
    ) -> Result<(), Vec<(ConnectionHandle, ChannelClosedError<()>)>> {
        let mut errors = Vec::new();
        for (handle, connection) in self.connections.iter_mut() {
            if let Err(ChannelClosedError(_)) = connection.send(payload.clone()) {
                errors.push((*handle, ChannelClosedError(())));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn set_channels_builder<F>(&mut self, builder: F)
    where
        F: Fn(&mut ConnectionChannelsBuilder) + Send + Sync + 'static,
    {
        self.channels_builder_fn = Some(Box::new(builder));
    }

    pub fn send_message<M: ChannelMessage + Debug + Clone>(
        &mut self,
        handle: ConnectionHandle,
        message: M,
    ) -> Result<Option<M>, Box<dyn Error + Send>> {
        match self.connections.get_mut(&handle) {
            Some(connection) => {
                let channels = connection.channels().unwrap();
                let unsent = channels.send(message);
                if self.message_flushing_strategy == MessageFlushingStrategy::OnEverySend {
                    channels.flush::<M>();
                }
                Ok(unsent)
            }
            None => Err(Box::new(std::io::Error::new(
                // FIXME: move to enum Error
                std::io::ErrorKind::NotFound,
                "No such connection",
            ))),
        }
    }

    pub fn broadcast_message<M: ChannelMessage + Debug + Clone>(&mut self, message: M) {
        // info!("Broadcast:\n{:?}", message);
        for (handle, connection) in self.connections.iter_mut() {
            let channels = connection.channels().unwrap();
            let result = channels.send(message.clone());
            if self.message_flushing_strategy == MessageFlushingStrategy::OnEverySend {
                channels.flush::<M>();
            }
            if let Some(msg) = result {
                log::warn!("Failed broadcast to [{}]: {:?}", handle, msg);
            }
        }
    }

    pub fn recv_message<M: ChannelMessage + Debug + Clone>(
        &mut self,
        handle: ConnectionHandle,
    ) -> Option<M> {
        match self.connections.get_mut(&handle) {
            Some(connection) => {
                let channels = connection.channels().unwrap();
                channels.recv()
            }
            None => None,
        }
    }
}

// check every connection for timeouts.
// ie. check how long since we last saw a packet.
pub fn heartbeats_and_timeouts_system(
    mut net: NonSendMut<NetworkResource>,
    mut network_events: ResMut<Events<NetworkEvent>>,
) {
    let mut silent_handles = Vec::new();
    let mut needs_hb_handles = Vec::new();
    let idle_limit = net.idle_timeout_ms;
    let heartbeat_limit = net.auto_heartbeat_ms;
    for (handle, connection) in net.connections.iter_mut() {
        let (rx_ms, tx_ms) = connection.last_packet_timings();
        log::debug!("millis since last rx: {} tx: {}", rx_ms, tx_ms);
        if idle_limit.is_some() && rx_ms > idle_limit.unwrap() as u128 {
            // idle-timeout this connection
            silent_handles.push(*handle);
        }
        if heartbeat_limit.is_some() && tx_ms > heartbeat_limit.unwrap() as u128 {
            // send a heartbeat packet.
            needs_hb_handles.push(*handle);
        }
    }
    for handle in needs_hb_handles {
        log::debug!("Sending hearbeat packet on h:{}", handle);
        // heartbeat packets are empty
        net.send(handle, Packet::new()).unwrap();
    }
    for handle in silent_handles {
        log::warn!("Idle disconnect for h:{}", handle);
        // Error doesn't imply Disconnected, so we send both
        network_events.send(NetworkEvent::Error(handle, NetworkError::MissedHeartbeat));
        network_events.send(NetworkEvent::Disconnected(handle));
        net.disconnect(handle);
    }
}

pub fn receive_packets_system(
    mut net: NonSendMut<NetworkResource>,
    mut network_events: ResMut<Events<NetworkEvent>>,
) {
    let packet_pool = net.packet_pool.clone();
    #[cfg(feature = "server")]
    net.process_server_connections(&mut network_events);
    #[cfg(feature = "client")]
    net.process_client_connections(&mut network_events);
    for (handle, connection) in net.connections.iter_mut() {
        while let Some(result) = connection.receive() {
            match result {
                Ok(packet) => {
                    // heartbeat packets are empty
                    if packet.is_empty() {
                        log::debug!("Received heartbeat packet");
                        // discard without sending a NetworkEvent
                        continue;
                    }
                    let message = String::from_utf8_lossy(&packet);
                    log::debug!("Received on [{}] {} RAW: {}", handle, packet.len(), message);
                    if let Some(channels_rx) = connection.channels_rx() {
                        log::debug!("Processing as message");
                        let mut pool_packet = packet_pool.acquire();
                        pool_packet.resize(packet.len(), 0);
                        pool_packet[..].copy_from_slice(&packet);
                        if let Err(err) = channels_rx.try_send(pool_packet) {
                            log::error!("Channel Incoming Error: {}", err);
                            network_events.send(NetworkEvent::Error(
                                *handle,
                                NetworkError::TurbulenceChannelError(err),
                            ));
                        }
                    } else {
                        log::debug!("Processing as packet");
                        network_events.send(NetworkEvent::Packet(*handle, packet));
                    }
                }
                Err(err) => {
                    log::error!("Receive Error: {:?}", err);
                    network_events.send(NetworkEvent::Error(*handle, err));
                }
            }
        }
    }
}
