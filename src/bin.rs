extern crate librain;
#[macro_use]
extern crate clap;
#[macro_use]
extern crate log;
extern crate tokio_core;
extern crate env_logger;
extern crate num_cpus;
extern crate nix;
extern crate serde_json;
#[macro_use]
extern crate error_chain;

pub mod start;

use std::process::exit;
use std::path::{Path, PathBuf};
use std::error::Error;
use std::io::Write;
use std::collections::HashMap;

use librain::{server, worker, VERSION};
use clap::{Arg, ArgMatches, App, SubCommand};
use librain::errors::Result;

use std::net::{SocketAddr, IpAddr, Ipv4Addr, ToSocketAddrs};

const DEFAULT_SERVER_PORT: u16 = 7210;
const DEFAULT_WORKER_PORT: u16 = 0;
const CLIENT_PROTOCOL_VERSION: i32 = 0;
const WORKER_PROTOCOL_VERSION: i32 = 0;

const DEFAULT_HTTP_PORT: u16 = 8080;


fn parse_listen_arg(args: &ArgMatches, default_port: u16) -> SocketAddr {
    if !args.is_present("LISTEN_ADDRESS") {
        return SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), default_port);
    }

    value_t!(args, "LISTEN_ADDRESS", SocketAddr)
        .unwrap_or_else(|_| match value_t!(args, "LISTEN_ADDRESS", IpAddr) {
            Ok(ip) => SocketAddr::new(ip, default_port),
            _ => {
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                    value_t_or_exit!(args, "LISTEN_ADDRESS", u16),
                )
            }
        })
}


fn run_server(_global_args: &ArgMatches, cmd_args: &ArgMatches) {
    let listen_address = parse_listen_arg(cmd_args, DEFAULT_SERVER_PORT);
    let ready_file = cmd_args.value_of("READY_FILE");
    info!(
        "Starting Rain {} server at port {}",
        VERSION,
        listen_address
    );

    let log_dir_prefix = Path::new(cmd_args.value_of("LOG_DIR").unwrap_or("/tmp"));

    let log_dir = make_logging_directory(log_dir_prefix, "server-").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    let mut tokio_core = tokio_core::reactor::Core::new().unwrap();
    let state = server::state::StateRef::new(tokio_core.handle(), listen_address, log_dir);
    state.start();

    // Create ready file - a file that is created when server is ready
    if let Some(name) = ready_file {
        ::librain::common::fs::create_ready_file(Path::new(name));
    }

    loop {
        tokio_core.turn(None);
        if !state.turn() {
            break;
        }
    }
}


// Creates a working directory of the following scheme prefix + "/rain/" + base_name + process_pid
// It checks that 'prefix' exists, but not the full path
fn make_working_directory(prefix: &Path, base_name: &str) -> Result<PathBuf> {
    if !prefix.exists() {
        bail!(format!(
            "Working directory prefix {:?} does not exists",
            prefix
        ));
    }

    if !prefix.is_dir() {
        bail!(format!(
            "Working directory prefix {:?} is not directory",
            prefix
        ));
    }

    let pid = nix::unistd::getpid();
    let work_dir = prefix.join("rain").join(format!("{}{}", base_name, pid));

    if work_dir.exists() {
        bail!(format!("Working directory {:?} already exists", work_dir));
    }

    debug!("Creating working directory {:?}", work_dir);
    if let Err(e) = std::fs::create_dir_all(work_dir.clone()) {
        bail!(format!(
            "Working directory {:?} cannot by created: {}",
            work_dir,
            e.description()
        ));
    }
    Ok(work_dir)
}


// Creates a logging directory of the following scheme:
// prefix + "/rain/" + base_name + process_pid + "/logs/"
// It checks that 'prefix' exists, but not the full path
fn make_logging_directory(prefix: &Path, base_name: &str) -> Result<PathBuf> {
    if !prefix.exists() {
        bail!(format!(
            "Logging directory prefix {:?} does not exists",
            prefix
        ));
    }

    if !prefix.is_dir() {
        bail!(format!(
            "Logging directory prefix {:?} is not directory",
            prefix
        ));
    }

    let pid = nix::unistd::getpid();
    let log_dir = prefix
        .join("rain")
        .join(format!("{}{}", base_name, pid))
        .join("logs");

    if log_dir.exists() {
        bail!(format!("Logging directory {:?} already exists", log_dir));
    }

    debug!("Creating logging directory {:?}", log_dir);
    if let Err(e) = std::fs::create_dir_all(log_dir.clone()) {
        bail!(format!(
            "Logging directory {:?} cannot by created: {}",
            log_dir,
            e.description()
        ));
    }
    Ok(log_dir)
}


fn run_worker(_global_args: &ArgMatches, cmd_args: &ArgMatches) {
    let ready_file = cmd_args.value_of("READY_FILE");
    let listen_address = parse_listen_arg(cmd_args, DEFAULT_WORKER_PORT);
    let mut server_address = cmd_args.value_of("SERVER_ADDRESS").unwrap().to_string();
    if !server_address.contains(":") {
        server_address = format!("{}:{}", server_address, DEFAULT_SERVER_PORT);
    }


    let server_addr = match server_address.to_socket_addrs() {
        Err(_) => {
            error!("Cannot resolve server address");
            exit(1);
        }
        Ok(mut addrs) => {
            match addrs.next() {
                None => {
                    error!("Cannot resolve server address");
                    exit(1);
                }
                Some(ref addr) => *addr,
            }
        }
    };

    fn detect_cpus() -> i32 {
        debug!("Detecting number of cpus");
        let cpus = num_cpus::get();
        if cpus < 1 {
            error!("Autodetection of CPUs failed. Use --cpus with a positive argument.");
            exit(1);
        }
        cpus as i32
    }

    let cpus = if cmd_args.is_present("CPUS") {
        let value = value_t_or_exit!(cmd_args, "CPUS", i32);
        if value < 0 {
            let cpus = detect_cpus();
            if cpus <= -value {
                error!("{} cpus detected and {} is subtracted via --cpus. No cpus left.", cpus, -value);
                exit(1);
            }
            detect_cpus() + value
        } else {
            value
        }
    } else {
        detect_cpus()
    };
    assert!(cpus >= 0);

    let work_dir_prefix = Path::new(cmd_args.value_of("WORK_DIR").unwrap_or("/tmp"));

    let work_dir = make_working_directory(work_dir_prefix, "worker-").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    let log_dir_prefix = Path::new(cmd_args.value_of("LOG_DIR").unwrap_or("/tmp"));

    let log_dir = make_logging_directory(log_dir_prefix, "worker-").unwrap_or_else(|e| {
        error!("{}", e);
        exit(1);
    });

    info!("Starting Rain {} as worker", VERSION);
    info!("Resources: {} cpus", cpus);
    info!("Working directory: {:?}", work_dir);
    info!(
        "Server address {} was resolved as {}",
        server_address,
        server_addr
    );

    let mut tokio_core = tokio_core::reactor::Core::new().unwrap();

    let mut subworkers = HashMap::new();
    subworkers.insert(
        "py".to_string(),
        vec![
            "python3".to_string(),
            "-m".to_string(),
            "rain.subworker".to_string(),
        ],
    );

    let state = worker::state::StateRef::new(
        tokio_core.handle(),
        work_dir,
        log_dir,
        cpus as u32,
        // Python subworker
        subworkers,
    );

    state.start(server_addr, listen_address, ready_file);

    loop {
        tokio_core.turn(None);
        state.turn();
    }
}


fn run_starter(_global_args: &ArgMatches, cmd_args: &ArgMatches) {
    let listen_address = parse_listen_arg(cmd_args, DEFAULT_SERVER_PORT);
    let log_dir = ::std::env::current_dir().unwrap();

    info!("Log directory: {}", log_dir.to_str().unwrap());

    let mut local_workers = Vec::new();

    if cmd_args.is_present("SIMPLE") && cmd_args.is_present("LOCAL_WORKERS") {
        error!("--simple and --local-workers are mutually exclusive");
        exit(1);
    }
    /*    let local_workers = if cmd_args.is_present("LOCAL_WORKERS") {
        value_t_or_exit!(cmd_args, "LOCAL_WORKERS", u32)
    } else {
        0u32
    };*/
    if cmd_args.is_present("SIMPLE") {
        local_workers.push(None);
    }

    if let Some(workers) = cmd_args.value_of("LOCAL_WORKERS") {
        local_workers = match ::serde_json::from_str(workers) {
            Ok(cpus) => {
                let cpus: Vec<u32> = cpus;
                cpus.iter().map(|x| Some(*x)).collect()
            }
            Err(e) => {
                error!("Invalid format for --local-workers");
                exit(1);
            }
        }
    }

    let mut config = start::starter::StarterConfig::new(local_workers, listen_address, &log_dir);

    config.worker_host_file = cmd_args.value_of("WORKER_HOST_FILE").map(
        |s| PathBuf::from(s),
    );

    // Autoconf
    match cmd_args.value_of("AUTOCONF") {
        None => Ok(()),
        Some("pbs") => config.autoconf_pbs(),
        Some(name) => {
            error!("Unknown autoconf environment '{}'", name);
            exit(1)
        }
    }.map_err(|e| {
        error!("Autoconf failed: {}", e.description());
        exit(1);
    })
        .unwrap();

    // Ignite starter
    let mut starter = start::starter::Starter::new(config);

    match starter.start() {
        Ok(()) => info!("Rain is started."),
        Err(e) => {
            error!("{}", e.description());
            if starter.has_processes() {
                info!("Error occurs; clean up started processes ...");
                starter.kill_all();
            }
        }
    }
}

fn main() {
    // Temporary simple logger for better module log control, default level is INFO
    // TODO: replace with Fern or log4rs later
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init().unwrap();

    // We do not use clap macro to built argument,
    // since it cannot handle "-" in name of long arguments

    let args = App::new("Rain")
        .version(VERSION)
        .about("Task-based workflow manager and executor")
        .subcommand( // ---- SERVER ----
            SubCommand::with_name("server")
                .about("Rain server")
                .arg(Arg::with_name("LISTEN_ADDRESS")
                    .short("l")
                    .long("--listen")
                    .help("Listening port/address/address:port (default 0.0.0.0:7210)")
                    .takes_value(true))
                .arg(Arg::with_name("LOG_DIR")
                    .long("--logdir")
                    .help("Logging directory (default = /tmp)")
                    .takes_value(true))
                .arg(Arg::with_name("READY_FILE")
                    .long("--ready-file")
                    .help("Create a file when server is initialized and ready to accept connections")
                    .takes_value(true)))
        .subcommand( // ---- WORKER ----
            SubCommand::with_name("worker")
                .about("Rain worker")
                .arg(Arg::with_name("SERVER_ADDRESS")
                    .help("Listening address: port/address/address:port (default 0.0.0.0:7210)")
                    .required(true))
                .arg(Arg::with_name("LISTEN_ADDRESS")
                    .short("l")
                    .long("--listen")
                    .value_name("ADDRESS")
                    .help("Listening port/address/address:port (default = 0.0.0.0:auto)")
                    .takes_value(true))
                .arg(Arg::with_name("CPUS")
                    .long("--cpus")
                    .help("Number of cpus (default = auto)")
                    .value_name("N")
                    .takes_value(true))
                .arg(Arg::with_name("WORK_DIR")
                    .long("--workdir")
                    .help("Workding directory (default = /tmp)")
                    .value_name("DIR")
                    .takes_value(true))
                .arg(Arg::with_name("LOG_DIR")
                    .long("--logdir")
                    .help("Logging directory (default = /tmp)")
                    .takes_value(true))
                .arg(Arg::with_name("READY_FILE")
                    .long("--ready-file")
                    .value_name("DIR")
                    .help("Create a file when worker is initialized and connected to the server")
                    .takes_value(true)))
        .subcommand( // ---- RUN ----
            SubCommand::with_name("run")
                .about("Start server & workers at once")
                .arg(Arg::with_name("SIMPLE")
                    .long("--simple")
                    .help("Start server and one local worker"))
                .arg(Arg::with_name("LOCAL_WORKERS")
                    .long("--local-workers")
                    .help("Specify local workers (e.g. --local-workers=[4,4])")
                     .value_name("RESOURCES")
                    .takes_value(true))
                .arg(Arg::with_name("WORKER_HOST_FILE")
                     .long("--worker-host-file")
                     .help("File with hosts for workers, one each line")
                     .value_name("FILE")
                     .takes_value(true))
                .arg(Arg::with_name("AUTOCONF")
                    .long("--autoconf")
                    .help("Automatic configuration - possible values: pbs")
                    .possible_value("pbs")
                    .takes_value(true))
                .arg(Arg::with_name("LISTEN_ADDRESS")
                    .short("l")
                    .value_name("ADDRESS")
                    .long("--listen")
                    .help("Server listening port/address/address:port (default = 0.0.0.0:auto)")
                    .takes_value(true))
                .arg(Arg::with_name("WORK_DIR")
                    .long("--workdir")
                    .help("Workding directory for workers (default = /tmp)")
                    .takes_value(true))
            .arg(Arg::with_name("LOG_DIR")
                    .long("--logdir")
                    .help("Logging directory for workers & server (default = /tmp)")
                    .takes_value(true)))
        .get_matches();

    match args.subcommand() {
        ("server", Some(ref cmd_args)) => run_server(&args, cmd_args),
        ("worker", Some(ref cmd_args)) => run_worker(&args, cmd_args),
        ("run", Some(ref cmd_args)) => run_starter(&args, cmd_args),
        _ => {
            error!("No subcommand provided.");
            ::std::process::exit(1);
        }
    }
}
