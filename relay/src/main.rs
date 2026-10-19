//! `desklink-relay --config /etc/desklink-relay/config.toml`
//!
//! Logs go to stderr and are limited to startup/config errors and periodic
//! aggregate counts: no payload, IP address or fingerprint is ever printed.

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use desklink_relay::config::Config;
use desklink_relay::limits::TurnQuota;
use desklink_relay::{Server, Shared};

const SAVE_EVERY: Duration = Duration::from_secs(60);
const LOG_EVERY: Duration = Duration::from_secs(300);

/// SIGTERM/SIGINT only set a flag; the main loop saves the quota and exits.
/// This matters in a container, where the relay is PID 1 and an unhandled
/// SIGTERM is ignored until Docker escalates to SIGKILL ten seconds later.
#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: i32) {
        STOP.store(true, Ordering::SeqCst);
    }

    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    pub fn install() {
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe; `signal` is the libc function std links anyway.
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }
    }

    pub fn stop_requested() -> bool {
        STOP.load(Ordering::SeqCst)
    }
}

#[cfg(not(unix))]
mod signals {
    pub fn install() {}
    pub fn stop_requested() -> bool {
        false
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let path = match args.as_slice() {
        [_, flag, path] if flag == "--config" => path,
        _ => {
            eprintln!("usage: desklink-relay --config <config.toml>");
            return ExitCode::from(2);
        }
    };
    let config = match std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {path}: {error}"))
        .and_then(|text| Config::parse(&text, |name| std::env::var(name).ok()))
    {
        Ok(config) => config,
        Err(error) => {
            eprintln!("desklink-relay: {error}");
            return ExitCode::from(2);
        }
    };
    let quota = match &config.state_file {
        Some(state_file) => TurnQuota::load(state_file).unwrap_or_else(|error| {
            eprintln!("desklink-relay: {error}");
            TurnQuota::default()
        }),
        None => TurnQuota::default(),
    };
    let backend = config.turn_backend;
    let shared = Shared::new(config, quota);
    let server = match Server::bind(shared.clone()) {
        Ok(server) => server,
        Err(error) => {
            eprintln!(
                "desklink-relay: cannot listen on {}: {error}",
                shared.config.listen
            );
            return ExitCode::FAILURE;
        }
    };
    match server.local_addr() {
        Ok(addr) => eprintln!("desklink-relay listening on {addr}, TURN backend {backend:?}"),
        Err(_) => eprintln!("desklink-relay listening, TURN backend {backend:?}"),
    }
    signals::install();
    if let Err(error) = thread::Builder::new()
        .name("relay-accept".into())
        .spawn(move || server.run())
    {
        eprintln!("desklink-relay: cannot start: {error}");
        return ExitCode::FAILURE;
    }

    let (mut last_save, mut last_log) = (Instant::now(), Instant::now());
    while !signals::stop_requested() {
        thread::sleep(Duration::from_secs(1));
        if last_save.elapsed() >= SAVE_EVERY {
            last_save = Instant::now();
            shared.prune();
            if let Err(error) = shared.save_quota() {
                eprintln!("desklink-relay: {error}");
            }
        }
        if last_log.elapsed() >= LOG_EVERY {
            last_log = Instant::now();
            eprintln!("desklink-relay: {}", shared.summary());
        }
    }
    if let Err(error) = shared.save_quota() {
        eprintln!("desklink-relay: {error}");
    }
    eprintln!("desklink-relay: stopped; {}", shared.summary());
    ExitCode::SUCCESS
}
