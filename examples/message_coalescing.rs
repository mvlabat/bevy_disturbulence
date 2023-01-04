use bevy::{
    app::{AppExit, ScheduleRunnerSettings},
    log::{self, LogPlugin},
    prelude::*,
};
use bevy_disturbulence::{
    ConnectionChannelsBuilder, MessageChannelMode, MessageChannelSettings, MessageFlushingStrategy,
    NetworkResource, NetworkingPlugin,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use turbulence::unreliable_channel::Settings;

mod utils;

const NUM_PINGS: usize = 100;

#[derive(Resource, Debug, Default)]
struct PingPongCounter {
    pings_sent: usize,
    pings_seen: usize,
    pongs_sent: usize,
    pongs_seen: usize,
}

#[derive(Resource, Deref, DerefMut)]
struct Ttl(Option<f64>);

#[derive(Resource, Deref, DerefMut)]
struct Ticks(usize);

fn main() {
    let args = utils::parse_message_coalescing_args();
    let mut net_plugin = NetworkingPlugin::default();
    if args.manual_flush {
        net_plugin.message_flushing_strategy = MessageFlushingStrategy::Never;
    }

    let mut app = App::new();
    app
        // minimal plugins necessary for timers + headless loop
        .insert_resource(ScheduleRunnerSettings::run_loop(Duration::from_secs_f64(
            1.0 / 60.0,
        )))
        .insert_resource::<Ticks>(Ticks(0))
        .insert_resource::<Ttl>(Ttl(None))
        .insert_resource(PingPongCounter::default())
        .add_plugins(MinimalPlugins)
        .add_plugin(LogPlugin::default())
        // The NetworkingPlugin
        .add_plugin(net_plugin)
        // Our networking
        .insert_resource(args)
        .add_startup_system(setup_channels_system)
        .add_system(tick_system)
        .add_system(send_messages_system)
        .add_system(handle_messages_system)
        .add_system(ttl_system);

    #[cfg(feature = "server")]
    app.add_startup_system(startup_server_system);
    #[cfg(feature = "client")]
    app.add_startup_system(startup_client_system);

    if utils::parse_message_coalescing_args().manual_flush {
        app.add_system_to_stage(CoreStage::PostUpdate, flush_channels_system);
    }
    app.run();
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
enum NetMsg {
    Ping(usize),
    Pong(usize),
}

const NETMSG_SETTINGS: MessageChannelSettings = MessageChannelSettings {
    channel: 0,
    channel_mode: MessageChannelMode::Unreliable {
        settings: Settings {
            bandwidth: 4096,
            burst_bandwidth: 1024,
        },
        max_message_len: 1024,
    },
    // The buffer size for the mpsc channel of messages that transports messages of this type to /
    // from the network task.
    message_buffer_size: NUM_PINGS,
    // The buffer size for the mpsc channel of packets for this message type that transports
    // packets to / from the packet multiplexer.
    packet_buffer_size: 10,
};

fn setup_channels_system(mut net: ResMut<NetworkResource>) {
    net.set_channels_builder(|builder: &mut ConnectionChannelsBuilder| {
        builder.register::<NetMsg>(NETMSG_SETTINGS).unwrap();
    });
}

fn tick_system(mut ticks: ResMut<Ticks>) {
    **ticks += 1;
}

fn flush_channels_system(mut net: ResMut<NetworkResource>) {
    for (_handle, connection) in net.connections.iter_mut() {
        let channels = connection.channels().unwrap();
        channels.flush::<NetMsg>();
    }
}

#[cfg(feature = "server")]
fn startup_server_system(mut net: ResMut<NetworkResource>) {
    log::info!("Starting server");
    net.listen(&bevy_disturbulence::ServerAddrs {
        session_listen_addr: "127.0.0.1:8089".parse().unwrap(),
        webrtc_listen_addr: "127.0.0.1:0".parse().unwrap(),
        public_webrtc_url: "http://127.0.0.1:8089".to_string(),
    });
}

#[cfg(feature = "client")]
fn startup_client_system(mut net: ResMut<NetworkResource>) {
    log::info!("Starting client");
    net.connect("http://127.0.0.1:8089");
}

fn ttl_system(
    mut ttl: ResMut<Ttl>,
    mut exit: EventWriter<AppExit>,
    time: Res<Time>,
    net: Res<NetworkResource>,
    ppc: Res<PingPongCounter>,
    args: Res<utils::MessageCoalescingArgs>,
) {
    match **ttl {
        None => {}
        Some(secs) => {
            let new_secs = secs - time.delta_seconds_f64();
            if new_secs <= 0.0 {
                // dump some stats and exit
                log::info!(
                    "Final stats, is_server: {:?}, flushing mode: {}",
                    cfg!(feature = "server"),
                    if args.manual_flush {
                        "--manual-flush"
                    } else {
                        "--auto-flush"
                    }
                );
                log::info!("{:?}", *ppc);
                for (handle, connection) in net.connections.iter() {
                    log::info!("{:?} [h:{}]", connection.stats(), handle);
                }
                log::info!("Exiting.");
                exit.send(AppExit);
            } else {
                **ttl = Some(new_secs);
            }
        }
    }
}

fn send_messages_system(
    mut net: ResMut<NetworkResource>,
    mut ppc: ResMut<PingPongCounter>,
    mut ttl: ResMut<Ttl>,
    ticks: Res<Ticks>,
) {
    if cfg!(feature = "server") || net.connections.len() == 0 {
        // client sends pings, server replies with pongs in handle_messages.
        return;
    }
    // send 10 pings per tick
    for _ in 0..10 {
        if ppc.pings_sent < NUM_PINGS {
            ppc.pings_sent += 1;
            let msg = NetMsg::Ping(ppc.pings_sent);
            log::info!("[t:{}] Sending ping {}", **ticks, ppc.pings_sent);
            net.broadcast_message(msg);
        } else if ppc.pings_sent == NUM_PINGS && ttl.is_none() {
            // shutdown after short delay, to finish receiving in-flight pongs
            **ttl = Some(1.0);
            return;
        }
    }
}

fn handle_messages_system(
    mut net: ResMut<NetworkResource>,
    mut ppc: ResMut<PingPongCounter>,
    mut ttl: ResMut<Ttl>,
    ticks: Res<Ticks>,
) {
    let mut to_send = Vec::new();
    for (handle, connection) in net.connections.iter_mut() {
        let channels = connection.channels().unwrap();
        while let Some(netmsg) = channels.recv::<NetMsg>() {
            match netmsg {
                NetMsg::Ping(i) => {
                    ppc.pings_seen += 1;
                    log::info!("[t:{}] Sending pong {}", **ticks, i);
                    to_send.push((*handle, NetMsg::Pong(i)));
                    // seen our first ping, so schedule a shutdown
                    if ttl.is_none() {
                        **ttl = Some(3.0);
                    }
                }
                NetMsg::Pong(i) => {
                    ppc.pongs_seen += 1;
                    log::info!("[t:{}] Got pong {}", **ticks, i);
                }
            }
        }
    }
    for (handle, msg) in to_send {
        ppc.pongs_sent += 1;
        net.send_message(handle, msg).unwrap();
    }
}
