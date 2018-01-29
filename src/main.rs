extern crate futures;
extern crate irc;
extern crate tokio_core;
extern crate tokio_io;

use futures::{Sink, future};
use irc::client::prelude::*;
use irc::client::PackedIrcClient;
use irc::error::IrcError;
use irc::proto::IrcCodec;
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;

fn main() {
    main_impl().unwrap();
}

fn main_impl() -> irc::error::Result<()> {
    let address = "0.0.0.0:6667".parse().unwrap();
    let config = Config {
        nickname: Some("spilo".to_owned()),
        server: Some("turtur.pdgn.co".to_owned()),
        use_ssl: Some(true),
        ..Config::default()
    };

    let mut reactor = Core::new()?;
    let (client_handle, server_handle) = (reactor.handle(), reactor.handle());

    // Setting up the client portion.
    let PackedIrcClient(client, client_send_future) = reactor.run(IrcClient::new_future(
        client_handle, &config
    )?)?;
    client.identify()?;

    // Setting up the bouncer server portion.
    let listener = TcpListener::bind(&address, &server_handle)?;
    let _codec = IrcCodec::new(config.encoding())?;
    let bouncer_connections = listener.incoming().map(move |(socket, addr)| {
        let (writer, reader) = socket.framed(
            IrcCodec::new(config.encoding()).expect("unreachable")
        ).split();
        (writer, reader, addr)
    });

    // Joining the two together.
    let bouncer = bouncer_connections.map_err(|e| {
        IrcError::Io(e)
    }).for_each(|(writer, reader, baddr)| {
        let local_client = client.clone();
        let saddr = local_client.config().server()?.to_owned();
        let (saddr1, saddr2) = (saddr.clone(), saddr);
        let (baddr1, baddr2) = (baddr.clone(), baddr);
        // local_client.stream() will panic right now if there's more than one client.
        let to_bouncer_client = writer.send_all(local_client.stream().map(move |message| {
            print!("[R {}] {}", saddr1, message);
            print!("[S {}] {}", baddr1, message);
            message
        }));
        let to_real_client = reader.filter(move |message| {
            print!("[R {}] {}", baddr2, message);
            match message.command {
                Command::NICK(_) |
                Command::USER(_, _, _) |
                Command::QUIT(_) => false,
                _ => true,
            }
        }).for_each(move |message| {
            print!("[S {}] {}", saddr2, message);
            local_client.send(message)
        });
        server_handle.spawn(to_bouncer_client.join(to_real_client).map(|_| ()).map_err(|e| {
            Err(e).unwrap()
        }));
        Ok(())
    });

    reactor.run(bouncer.join(client_send_future).map(|_| ()))
}
