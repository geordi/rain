
use std::process::Command;
use std::path::{Path, PathBuf};
use std::net::SocketAddr;
use start::common::Readiness;
use start::process::Process;
use start::ssh::RemoteProcess;
use librain::errors::{Error, Result};

use nix::unistd::{gethostname, getpid};
use std::io::BufReader;
use std::io::BufRead;
use std::fs::File;


pub struct StarterConfig {
    /// Number of local worker that will be spawned
    pub local_workers: Vec<Option<u32>>,

    /// Listening address of server
    pub server_listen_address: SocketAddr,

    /// Directory where logs are stored (absolute path)
    pub log_dir: PathBuf,

    pub worker_host_file: Option<PathBuf>,
}

impl StarterConfig {
    pub fn new(local_workers: Vec<Option<u32>>, server_listen_address: SocketAddr, log_dir: &Path) -> Self {
        Self {
            local_workers: local_workers,
            server_listen_address,
            log_dir: ::std::env::current_dir().unwrap().join(log_dir), // Make it absolute
            worker_host_file: None,
        }
    }

    pub fn autoconf_pbs(&mut self) -> Result<()> {
        info!("Configuring PBS environment");
        if self.worker_host_file.is_some() {
            bail!("Options --autoconf=pbs and --worker_host_file are not compatible");
        }
        let nodefile = ::std::env::var("PBS_NODEFILE");
        match nodefile {
            Err(e) => bail!("Variable PBS_NODEFILE not defined, are you running inside PBS?"),
            Ok(path) => self.worker_host_file = Some(PathBuf::from(path)),
        }
        Ok(())
    }
}

/// Starts server & workers
pub struct Starter {
    /// Configuration of starter
    config: StarterConfig,

    /// Spawned and running processes
    processes: Vec<Process>,

    /// Spawned and running processes
    remote_processes: Vec<RemoteProcess>,
}

fn read_host_file(path: &Path) -> Result<Vec<String>> {
    let file = BufReader::new(File::open(path).map_err(|e| {
        format!(
            "Cannot open worker host file {:?}: {}",
            path,
            ::std::error::Error::description(&e)
        )
    })?);
    let mut result = Vec::new();
    for line in file.lines() {
        let line = line?;
        let trimmed_line = line.trim();
        if !trimmed_line.is_empty() && !trimmed_line.starts_with("#") {
            result.push(trimmed_line.to_string());
        }
    }
    Ok(result)
}

impl Starter {
    pub fn new(config: StarterConfig) -> Self {
        Self {
            config: config,
            processes: Vec::new(),
            remote_processes: Vec::new(),
        }
    }

    pub fn has_processes(&self) -> bool {
        !self.processes.is_empty()
    }

    /// Main method of starter that launch everything
    pub fn start(&mut self) -> Result<()> {

        if !self.config.local_workers.is_empty() && self.config.worker_host_file.is_some() {
            bail!("Cannot combine remote & local workers");
        }

        let worker_hosts = if let Some(ref path) = self.config.worker_host_file {
            read_host_file(path)?
        } else {
            Vec::new()
        };

        if self.config.local_workers.is_empty() && worker_hosts.is_empty() {
            bail!("No workers are specified.");
        }

        self.start_server()?;
        self.busy_wait_for_ready()?;

        if !self.config.local_workers.is_empty() {
            self.start_local_workers()?;
        }
        if !worker_hosts.is_empty() {
            self.start_remote_workers(&worker_hosts)?;
        }
        self.busy_wait_for_ready()?;
        Ok(())
    }

    /// Path to "rain" binary
    pub fn local_rain_program(&self) -> String {
        ::std::env::args().nth(0).unwrap()
    }

    fn spawn_process(
        &mut self,
        name: &str,
        ready_file: &Path,
        command: &mut Command,
    ) -> Result<&Process> {
        self.processes.push(Process::spawn(
            &self.config.log_dir,
            name,
            Readiness::WaitingForReadyFile(ready_file.to_path_buf()),
            command,
        )?);
        Ok(&self.processes.last().unwrap())
    }

    /// Create a temporory filename
    fn create_tmp_filename(&self, name: &str) -> PathBuf {
        ::std::env::temp_dir().join(format!("rain-{}-{}", getpid(), name))
    }

    fn start_server(&mut self) -> Result<()> {
        let ready_file = self.create_tmp_filename("server-ready");
        let rain = self.local_rain_program();
        let server_address = format!("{}", self.config.server_listen_address);
        info!("Starting local server ({})", server_address);
        let process = self.spawn_process(
            "server",
            &ready_file,
            Command::new(rain)
                .arg("server")
                .arg("--listen")
                .arg(&server_address)
                .arg("--ready-file")
                .arg(&ready_file),
        )?;
        info!("Server pid = {}", process.id());
        Ok(())
    }

    fn start_remote_workers(&mut self, worker_hosts: &Vec<String>) -> Result<()> {
        info!("Starting {} remote worker(s)", worker_hosts.len());
        let rain = self.local_rain_program(); // TODO: configurable path for remotes
        let dir = ::std::env::current_dir().unwrap(); // TODO: Do it configurable
        let server_address = self.server_address();

        for (i, host) in worker_hosts.iter().enumerate() {
            info!(
                "Connecting to {} (remote log dir: {:?})",
                host,
                self.config.log_dir
            );
            let ready_file = self.create_tmp_filename(&format!("worker-{}-ready", i));
            let name = format!("worker-{}", i);
            let mut process = RemoteProcess::new(
                name,
                host,
                Readiness::WaitingForReadyFile(ready_file.to_path_buf()),
            );
            let command = format!(
                "{rain} worker {server_address} --ready-file {ready_file:?}",
                rain = rain,
                server_address = server_address,
                ready_file = ready_file
            );
            process.start(&command, &dir, &self.config.log_dir)?;
            self.remote_processes.push(process);
        }
        Ok(())
    }

    fn server_address(&self) -> String {
        let mut buf = [0u8; 256];
        let result: &str = gethostname(&mut buf).unwrap().to_str().unwrap();
        format!("{}:{}", result, self.config.server_listen_address.port())
    }

    fn start_local_workers(&mut self) -> Result<()> {
        info!("Starting {} local worker(s)", self.config.local_workers.len());
        let server_address = self.server_address();
        let rain = self.local_rain_program();
        let workers: Vec<_> = self.config.local_workers.iter().map(|x| x.clone()).enumerate().collect();
        for (i, resource) in workers {
            let ready_file = self.create_tmp_filename(&format!("worker-{}-ready", i));
            let mut cmd = Command::new(&rain);
                cmd.arg("worker")
                   .arg(&server_address)
                   .arg("--ready-file")
                   .arg(&ready_file);
                if let Some(cpus) = resource {
                    cmd.arg("--cpus");
                    cmd.arg(cpus.to_string());
                }
            let process = self.spawn_process(
                &format!("worker-{}", i),
                &ready_file,
                &mut cmd
            )?;
        }
        Ok(())
    }

    /// Waits until all processes are ready
    pub fn busy_wait_for_ready(&mut self) -> Result<()> {
        let mut timeout_ms = 50; // Timeout, it it increased every cycle upto 1.5 seconds
        while 0 != self.check_all_ready()? {
            ::std::thread::sleep(::std::time::Duration::from_millis(timeout_ms));
            if timeout_ms < 1500 {
                timeout_ms += 50;
            }
        }
        Ok(())
    }

    /// Checks that all registered processes are still running
    /// and check if their ready_files are not createn
    pub fn check_all_ready(&mut self) -> Result<u32> {
        let mut not_ready = 0u32;
        // Here we intentionally goes through all processes
        // even we found first non-ready one, since we also
        // want to check that other processes are not terminated
        for process in &mut self.processes {
            if !process.check_ready()? {
                not_ready += 1;
            }
        }

        for process in &mut self.remote_processes {
            if !process.check_ready()? {
                not_ready += 1;
            }
        }
        Ok(not_ready)
    }

    /// This is cleanup method, so we want to silent errors
    pub fn kill_all(&mut self) {
        for mut process in ::std::mem::replace(&mut self.processes, Vec::new()) {
            match process.kill() {
                Ok(()) => {}
                Err(e) => debug!("Kill failed: {}", e.description()),
            };
        }

        for mut process in ::std::mem::replace(&mut self.remote_processes, Vec::new()) {
            match process.kill() {
                Ok(()) => {}
                Err(e) => debug!("Kill failed: {}", e.description()),
            };
        }
    }
}
