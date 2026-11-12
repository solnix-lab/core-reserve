use std::{io, net::SocketAddr, time::Duration};

use clap::Args;
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};

use crate::{Result, Shutdown, cpu_work, write_json};

#[derive(Args, Serialize)]
pub struct Settings {
    #[arg(long, default_value = "127.0.0.1:8400")]
    listen: SocketAddr,
    /// Linux CPU list, such as 0-3,8.
    #[arg(long)]
    pub(crate) cpus: Option<String>,
    /// CPU work performed per request.
    #[arg(long, default_value_t = 50_000, value_parser = clap::value_parser!(u32).range(1..=10_000_000))]
    iterations: u32,
    /// Maximum simultaneous client connections.
    #[arg(long, default_value_t = 1_024, value_parser = clap::value_parser!(u32).range(1..=16_384))]
    max_connections: u32,
}

#[derive(Serialize)]
struct Ready<'a> {
    schema_version: u32,
    event: &'static str,
    address: SocketAddr,
    effective_cpus: Vec<usize>,
    settings: &'a Settings,
}

pub(crate) async fn run(settings: Settings, cpus: Vec<usize>) -> Result<()> {
    let mut shutdown = Shutdown::new()?;
    let listener = TcpListener::bind(settings.listen).await?;
    write_json(
        io::stdout().lock(),
        &Ready {
            schema_version: 1,
            event: "service_ready",
            address: listener.local_addr()?,
            effective_cpus: cpus,
            settings: &settings,
        },
    )?;

    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.wait() => return Ok(()),
            Some(result) = requests.join_next() => result?,
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                while let Some(result) = requests.try_join_next() {
                    result?;
                }
                if requests.len() < settings.max_connections as usize {
                    let iterations = settings.iterations;
                    requests.spawn(async move {
                        // Incomplete and disconnected requests are reflected in client outcomes.
                        let _ = timeout(Duration::from_secs(5), respond(socket, iterations)).await;
                    });
                }
            }
        }
    }
}

async fn respond(mut socket: TcpStream, iterations: u32) -> io::Result<()> {
    let mut nonce = [0; 8];
    socket.read_exact(&mut nonce).await?;
    cpu_work(u64::from_be_bytes(nonce), iterations);
    socket.write_all(&nonce).await
}
