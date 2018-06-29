#[macro_use]
extern crate clap;
extern crate env_logger;
extern crate failure;
extern crate futures;
extern crate hex;
extern crate itertools;
#[macro_use]
extern crate log;
extern crate tokio;
extern crate tox;

mod cli_config;

use std::io::{Error, ErrorKind};
use std::net::{IpAddr, SocketAddr};

use futures::sync::mpsc;
use futures::{future, Future, Sink, Stream};
use log::LevelFilter;
use tokio::executor::thread_pool;
use tokio::net::{UdpSocket, UdpFramed};
use tokio::runtime;
use tox::toxcore::crypto_core::*;
use tox::toxcore::dht::codec::{DecodeError, DhtCodec};
use tox::toxcore::dht::server::Server;
use tox::toxcore::dht::lan_discovery::LanDiscoverySender;

use cli_config::*;

/// Bind a UDP listener to the socket address.
fn bind_socket(addr: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind(&addr).expect("Failed to bind UDP socket");
    socket.set_broadcast(true).expect("set_broadcast call failed");
    if addr.is_ipv6() {
        socket.set_multicast_loop_v6(true).expect("set_multicast_loop_v6 call failed");
    }
    socket
}

/// Run a future with the runtime specified by config.
fn run<F>(future: F, threads_config: ThreadsConfig)
    where F: Future<Item = (), Error = Error> + Send + 'static
{
    if threads_config == ThreadsConfig::N(1) {
        let mut runtime = runtime::current_thread::Runtime::new().expect("Failed to create runtime");
        runtime.block_on(future).expect("Execution was terminated with error");
    } else {
        let mut threadpool_builder = thread_pool::Builder::new();
        threadpool_builder.name_prefix("tox-node-");
        match threads_config {
            ThreadsConfig::N(n) => { threadpool_builder.pool_size(n as usize); },
            ThreadsConfig::Auto => { }, // builder will detect number of cores automatically
        }
        let mut runtime = runtime::Builder::new()
            .threadpool_builder(threadpool_builder)
            .build()
            .expect("Failed to create runtime");
        runtime.block_on(future).expect("Execution was terminated with error");
    };
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(LevelFilter::Info)
        .init();

    if !crypto_init() {
        panic!("Crypto initialization failed.");
    }

    let cli_config = cli_parse();

    let local_addr: SocketAddr = "0.0.0.0:33445".parse().unwrap(); // 0.0.0.0 for ipv4
    // let local_addr: SocketAddr = "[::]:33445".parse().unwrap(); // [::] for ipv6

    let socket = bind_socket(local_addr);
    let (sink, stream) = UdpFramed::new(socket, DhtCodec).split();

    let (dht_pk, dht_sk) = gen_keypair();

    // Create a channel for server to communicate with network
    let (tx, rx) = mpsc::unbounded();

    let lan_discovery_sender = LanDiscoverySender::new(tx.clone(), dht_pk, local_addr.is_ipv6());

    let mut server = Server::new(tx, dht_pk, dht_sk);
    server.set_bootstrap_info(07032018, b"This is tox-rs".to_vec());

    if cli_config.bootstrap_nodes.is_empty() {
        warn!("No bootstrap nodes!");
    }

    for node in cli_config.bootstrap_nodes {
        assert!(server.try_add_to_close_nodes(&node));
    }

    // The server task asynchronously iterates over and processes each
    // incoming packet.
    let server_c = server.clone();
    let network_reader = stream.then(future::ok).filter(|event| // TODO: use filter_map from futures 0.2 to avoid next `expect`
        match event {
            &Ok(_) => true,
            &Err(ref e) => {
                error!("packet receive error = {:?}", e);
                // ignore packet decode errors
                e.cause().downcast_ref::<DecodeError>().is_none()
            }
        }
    ).then(|event: Result<_, ()>|
        event.expect("always ok")
    ).for_each(move |(packet, addr)| {
        trace!("Received packet {:?}", packet);
        server_c.handle_packet(packet, addr).or_else(|err| {
            error!("Failed to handle packet: {:?}", err);
            future::ok(())
        })
    }).map_err(|e| Error::new(ErrorKind::Other, e.compat()));

    let network_writer = rx
        .map_err(|()| Error::new(ErrorKind::Other, "rx error"))
        // filter out IPv6 packets if node is running in IPv4 mode
        .filter(move |&(ref _packet, addr)| !(local_addr.is_ipv4() && addr.is_ipv6()))
        .fold(sink, move |sink, (packet, mut addr)| {
            if local_addr.is_ipv6() {
                if let IpAddr::V4(ip) = addr.ip() {
                    addr = SocketAddr::new(IpAddr::V6(ip.to_ipv6_mapped()), addr.port());
                }
            }
            trace!("Sending packet {:?} to {:?}", packet, addr);
            sink.send((packet, addr)).map_err(|e| Error::new(ErrorKind::Other, e.compat()))
        })
        // drop sink when rx stream is exhausted
        .map(|_sink| ());

    let future = network_writer.select(network_reader).map(|_| ()).map_err(|(e, _)| e);
    let future = future.select(server.run()).map(|_| ()).map_err(|(e, _)| e);
    let future = future.select(lan_discovery_sender.run()).map(|_| ()).map_err(|(e, _)| e);

    info!("Running server on {}", local_addr);

    run(future, cli_config.threads_config);
}
