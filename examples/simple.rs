use bevy::{
    app::ScheduleRunnerSettings,
    log::{self, LogPlugin},
    prelude::*,
};
use bevy_disturbulence::{NetworkEvent, NetworkResource, NetworkingPlugin, Packet};
use std::time::Duration;

fn main() {
    let mut app = App::new();
    app
        // Minimal plugins necessary for timers + headless loop
        .insert_resource(ScheduleRunnerSettings::run_loop(Duration::from_secs_f64(
            1.0 / 60.0,
        )))
        .add_plugins(MinimalPlugins)
        .add_plugin(LogPlugin::default())
        .add_plugin(NetworkingPlugin::default())
        // The example systems
        .add_system(send_packets_system)
        .add_system(handle_packets_system);

    #[cfg(feature = "server")]
    app.add_startup_system(server_startup_system);
    #[cfg(feature = "client")]
    app.add_startup_system(client_startup_system);

    app.run();
}

#[cfg(feature = "server")]
fn server_startup_system(mut net: NonSendMut<NetworkResource>) {
    log::info!("Starting server");
    net.listen(&naia_server_socket::ServerAddrs {
        session_listen_addr: "127.0.0.1:8088".parse().unwrap(),
        webrtc_listen_addr: "127.0.0.1:8088".parse().unwrap(),
        public_webrtc_url: "http://127.0.0.1:8088".to_string(),
    });
}

#[cfg(feature = "client")]
fn client_startup_system(mut net: NonSendMut<NetworkResource>) {
    log::info!("Starting client");
    net.connect("http://127.0.0.1:8088");
}

fn send_packets_system(mut net: NonSendMut<NetworkResource>, time: Res<Time>) {
    if cfg!(feature = "client") && (time.elapsed_seconds() * 60.) as i64 % 60 == 0 {
        // Client context
        if !net.connections.is_empty() {
            log::info!("PING");
            net.broadcast(&Packet::from("PING")).unwrap();
        }
    }
}
fn handle_packets_system(
    mut net: NonSendMut<NetworkResource>,
    time: Res<Time>,
    mut reader: EventReader<NetworkEvent>,
) {
    for event in reader.iter() {
        match event {
            NetworkEvent::Packet(handle, packet) => {
                let message = String::from_utf8_lossy(packet);
                log::info!("Got packet on [{}]: {}", handle, message);
                if message == "PING" {
                    let message = format!("PONG @ {}", time.elapsed_seconds());
                    match net.send(*handle, Packet::from(message)) {
                        Ok(()) => {
                            log::info!("Sent PONG");
                        }
                        Err(error) => {
                            log::info!("PONG send error: {}", error);
                        }
                    }
                }
            }
            event => log::info!("{event:?} received!"),
        }
    }
}
