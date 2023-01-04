use bevy::{
    app::{App, CoreStage, ScheduleRunnerSettings},
    ecs::{
        event::EventReader,
        system::{ResMut, Resource},
    },
    log,
    log::LogPlugin,
    prelude::SystemStage,
    time::FixedTimestep,
    MinimalPlugins,
};
use bevy_disturbulence::{NetworkEvent, NetworkResource, NetworkingPlugin, Packet};
use std::time::Duration;

mod utils;

#[derive(Resource, Debug, Default)]
struct PingPongCounter {
    ping_reservoir: usize,
    pong_reservoir: usize,
}

fn main() {
    let args = utils::parse_idle_timeout_args();
    log::info!("{:?}", args);

    let net_plugin = NetworkingPlugin {
        idle_timeout_millis: Some(args.idle_timeout_ms),
        auto_heartbeat_millis: Some(args.auto_heartbeat_ms),
        ..Default::default()
    };

    let ppc = PingPongCounter {
        ping_reservoir: args.pings,
        pong_reservoir: args.pongs,
    };

    let mut app = App::new();
    app
        // Minimal plugins necessary for timers + headless loop
        .insert_resource(ScheduleRunnerSettings::run_loop(Duration::from_secs_f64(
            1.0 / 60.0,
        )))
        .insert_resource(ppc)
        .add_plugins(MinimalPlugins)
        .add_plugin(LogPlugin::default())
        // The NetworkingPlugin
        .add_plugin(net_plugin)
        // The example resources and systems
        .insert_resource(args)
        .add_system(send_pongs_system)
        .add_stage_after(
            CoreStage::Update,
            "ping_sending_stage",
            SystemStage::single(send_pings_system).with_run_criteria(FixedTimestep::step(1.0)),
        );

    #[cfg(feature = "server")]
    app.add_startup_system(server_startup_system);
    #[cfg(feature = "client")]
    app.add_startup_system(client_startup_system);

    app.run();
}

#[cfg(feature = "server")]
fn server_startup_system(mut net: ResMut<NetworkResource>) {
    log::info!("Starting server");
    net.listen(&naia_server_socket::ServerAddrs {
        session_listen_addr: "127.0.0.1:8089".parse().unwrap(),
        webrtc_listen_addr: "127.0.0.1:8088".parse().unwrap(),
        public_webrtc_url: "http://127.0.0.1:8088".to_string(),
    });
}

#[cfg(feature = "client")]
fn client_startup_system(mut net: ResMut<NetworkResource>) {
    log::info!("Starting client");
    net.connect("http://127.0.0.1:8089");
}

fn send_pings_system(mut net: ResMut<NetworkResource>, mut ppc: ResMut<PingPongCounter>) {
    if ppc.ping_reservoir == 0 || net.connections.len() == 0 {
        return;
    }

    ppc.ping_reservoir -= 1;
    net.broadcast(&Packet::from("PING")).unwrap();

    if ppc.ping_reservoir == 0 {
        log::info!("(No more pings left to send)");
    }
}

fn send_pongs_system(
    mut net: ResMut<NetworkResource>,
    mut ppc: ResMut<PingPongCounter>,
    mut reader: EventReader<NetworkEvent>,
) {
    for event in reader.iter() {
        match event {
            NetworkEvent::Packet(handle, packet) => {
                let message = String::from_utf8_lossy(packet);
                log::info!("Got packet on [{}]: {}", handle, message);
                if message == "PING" {
                    if ppc.pong_reservoir > 0 {
                        ppc.pong_reservoir -= 1;
                        match net.send(*handle, Packet::from("PONG")) {
                            Ok(()) => log::info!("Sent PONG"),
                            Err(error) => log::warn!("PONG send error: {}", error),
                        }
                    } else {
                        log::info!("No pongs left to send.");
                    }
                }
            }
            other => {
                log::info!("Other event: {:?}", other);
            }
        }
    }
}
