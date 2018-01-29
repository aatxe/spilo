extern crate failure;
extern crate futures;
extern crate irc;
extern crate tokio_core;
extern crate tokio_io;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::vec::IntoIter;
use failure::Fail;
use futures::{Async, AsyncSink, Poll, Sink, StartSend};
use futures::sync::mpsc;
use futures::sync::mpsc::{UnboundedReceiver, UnboundedSender};
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
    let splitter = Splitter::new();
    let client_splitter_future = client.stream().forward(splitter.clone());
    let saddr = client.config().socket_addr()?;
    let hostname = LatestHostname::new();
    let split_to_nowhere = splitter.split().map(|message| {
        let local_client = client.clone();
        print!("[R {}] {}", saddr, message);
        if message.source_nickname().unwrap_or("") == local_client.current_nickname() {
            hostname.clone().update_hostname(message.prefix.clone());
        }
        message
    }).map_err::<IrcError, _>(|()| unreachable!()).collect();

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
    let bouncer = bouncer_connections.map_err(IrcError::Io).for_each(|(writer, reader, baddr)| {
        let client_stream = splitter.split();
        let local_client = client.clone();
        let saddr = local_client.config().socket_addr()?;

        let mut chans = local_client.list_channels().unwrap_or_else(|| Vec::new()).iter().fold(
            String::new(), |mut acc, chan| {
                acc.push_str(chan);
                acc.push_str(",");
                acc
            }
        );
        if chans.len() > 0 {
            let new_size = chans.len() - 1;
            chans.truncate(new_size);
        }
        local_client.send(Command::NAMES(Some(chans), None))?;

        let (tx_bouncer_client, rx_bouncer_client) = mpsc::unbounded();

        let to_bouncer_client = writer.send_all(client_stream.select(rx_bouncer_client).map(
            move |message| {
                print!("[S {}] {}", &baddr, message);
                message
            }
        ).map_err::<IrcError, _>(|()| unreachable!()));

        let filter_client = local_client.clone();
        let filter_hostname = hostname.clone();
        let to_real_client = reader.filter(move |message| {
            print!("[R {}] {}", &baddr, message);
            match message.command {
                // Complex (action-taking) filters
                Command::JOIN(ref chan, _, _) if filter_client.list_channels().map(|chans| {
                    chans.contains(chan)
                }).unwrap_or(false) => {
                    tx_bouncer_client.unbounded_send(Message {
                        tags: None,
                        prefix: filter_hostname.hostname(),
                        command: Command::JOIN(chan.to_owned(), None, None)
                    }).unwrap();
                    false
                }

                // Ordinary filters
                Command::Raw(ref cmd, _, _) if cmd == "USER" => false,
                Command::NICK(_) |
                Command::USER(_, _, _) |
                Command::CAP(_, _, _, _) |
                Command::QUIT(_) => false,
                _ => true,
            }
        }).for_each(move |message| {
            print!("[S {}] {}", &saddr, message);
            local_client.send(message)
        });
        let baddr = baddr.clone();
        server_handle.spawn(to_bouncer_client.join(to_real_client).map(|_| ()).map_err(move |e| {
            eprint!("[E {}] {}", baddr, e);
            for err in e.causes().skip(1) {
                eprint!(": {}", err);
            }
            eprintln!();
        }));
        Ok(())
    });

    reactor.run(bouncer.join(client_send_future)
                .join(client_splitter_future)
                .join(split_to_nowhere)
                .map(|_| ()))
}

#[derive(Clone)]
struct LatestHostname(Arc<Mutex<Option<String>>>);

impl LatestHostname {
    pub fn new() -> LatestHostname {
        LatestHostname(Arc::new(Mutex::new(None)))
    }

    pub fn update_hostname(&self, hostname: Option<String>) {
        *self.0.lock().expect("unreachable") = hostname
    }

    pub fn hostname(&self) -> Option<String> {
        self.0.lock().expect("unreachable").clone()
    }
}


#[derive(Clone)]
struct ReplayBuffer(Arc<Mutex<Vec<Message>>>, Arc<AtomicBool>);

impl ReplayBuffer {
    pub fn new() -> ReplayBuffer {
        ReplayBuffer(Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)))
    }

    pub fn ready(&self) {
        self.1.store(true, Ordering::SeqCst)
    }

    pub fn is_ready(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }

    pub fn push(&self, message: Message) {
        self.0.lock().expect("unreachable").push(message);
    }
}

impl IntoIterator for ReplayBuffer {
    type Item = Message;
    type IntoIter = IntoIter<Message>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.lock().expect("unreachable").clone().into_iter()
    }
}


#[derive(Clone)]
struct Splitter {
    senders: Arc<Mutex<Vec<UnboundedSender<Message>>>>,
    buffer: ReplayBuffer,
}

impl Splitter {
    pub fn new() -> Splitter {
        Splitter {
            senders: Arc::new(Mutex::new(Vec::new())),
            buffer: ReplayBuffer::new(),
        }
    }

    pub fn split(&self) -> UnboundedReceiver<Message> {
        let (send, recv) = mpsc::unbounded();
        for message in self.buffer.clone().into_iter() {
            send.unbounded_send(message).expect("unreachable");
        }
        self.senders.lock().expect("unreachable").push(send);
        recv
    }
}

impl Sink for Splitter {
    type SinkItem = Message;
    type SinkError = IrcError;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let mut res = Ok(AsyncSink::Ready);
        if self.buffer.is_ready() == false {
            match item.command {
                Command::Response(Response::RPL_ENDOFMOTD, _, _) |
                Command::Response(Response::ERR_NOMOTD, _, _) => self.buffer.ready(),
                _ => ()
            }
            self.buffer.push(item.clone());
        }
        for mut sender in self.senders.lock().expect("unreachable").iter() {
            res = res.and(sender.start_send(item.clone()));
        }
        res.map_err(IrcError::AsyncChannelClosed)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        let mut res = Ok(Async::Ready(()));
        for mut sender in self.senders.lock().expect("unreachable").iter() {
            res = res.and(sender.poll_complete());
        }
        res.map_err(IrcError::AsyncChannelClosed)
    }
}
