// Arg parser for the examples.
// 
// each example gets a different Args struct, which it adds as a bevy resource
use bevy::ecs::system::Resource;
use clap::{Arg, App as ClapApp, value_t_or_exit};

#[derive(Resource, Debug)]
pub struct MessageCoalescingArgs {
    pub pings: usize,
    pub pongs: usize,
    pub manual_flush: bool,
    pub auto_flush: bool,
}

#[derive(Resource, Debug)]
pub struct IdleTimeoutArgs {
    pub pings: usize,
    pub pongs: usize,
    pub idle_timeout_ms: usize,
    pub auto_heartbeat_ms: usize,
}

fn exe_name() -> String {
    match std::env::current_exe() {
        Ok(pathbuf) => match pathbuf.file_name() {
            Some(name) => name.to_string_lossy().into(),
            None => String::new()
        },
        Err(_) => String::new()
    }
}

#[allow(dead_code)]
pub fn parse_message_coalescing_args() -> MessageCoalescingArgs {
    let matches = ClapApp::new(exe_name())
        .about("Message coalescing example")
        .args(flushing_strategy_args().as_slice())
        .args(pings_pongs_args().as_slice())
        .get_matches();
    MessageCoalescingArgs {
        pings: value_t_or_exit!(matches, "pings", usize),
        pongs: value_t_or_exit!(matches, "pongs", usize),
        manual_flush: matches.is_present("manual-flush"),
        auto_flush: matches.is_present("auto-flush"),
    }
}

#[allow(dead_code)]
pub fn parse_idle_timeout_args() -> IdleTimeoutArgs {
    let matches = ClapApp::new(exe_name())
        .about("Idle timeout example")
        .args(pings_pongs_args().as_slice())
        .args(timeout_args().as_slice())
        .get_matches();
    IdleTimeoutArgs {
        pings: value_t_or_exit!(matches, "pings", usize),
        pongs: value_t_or_exit!(matches, "pongs", usize),
        idle_timeout_ms: value_t_or_exit!(matches, "idle-drop-timeout", usize),
        auto_heartbeat_ms: value_t_or_exit!(matches, "heartbeat-interval", usize),
    }
}

#[allow(dead_code)]
fn timeout_args<'a>() -> Vec<Arg<'a, 'a>> {
    vec![
        Arg::with_name("idle-drop-timeout")
        .help("Idle timeout (ms) after which to drop inactive connections")
        .long("idle-drop-timeout")
        .default_value("3000")
        .takes_value(true)
        .required(false)
        ,
        Arg::with_name("heartbeat-interval")
        .help("Interval (ms) after which to send a heartbeat packet, if we've been silent this long")
        .long("heartbeat-interval")
        .default_value("1000")
        .takes_value(true)
        .required(false)
    ]   
}

#[allow(dead_code)]
fn flushing_strategy_args<'a>() -> Vec<Arg<'a, 'a>> {
    vec![
        Arg::with_name("auto-flush")
        .help("Flush after every send")
        .long("auto-flush")
        .required_unless("manual-flush")
        .conflicts_with("manual-flush")
        .takes_value(false)
        ,
        Arg::with_name("manual-flush")
        .help("No automatic flushing, you must add a flushing system")
        .long("manual-flush")
        .required_unless("auto-flush")
        .conflicts_with("auto-flush")
        .takes_value(false)
    ]
}

fn pings_pongs_args<'a>() -> Vec<Arg<'a, 'a>> {
    vec![
        Arg::with_name("pings")
        .long("pings")
        .default_value("20")
        .help("Number of pings to send, once connected")
        .takes_value(true)
        ,
        Arg::with_name("pongs")
        .long("pongs")
        .default_value("10")
        .help("Number of pongs available to send as replies to pings")
        .takes_value(true)
    ]
}
