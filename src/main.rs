extern crate env_logger;
extern crate failure;
extern crate futures;
#[macro_use]
extern crate log;
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
    env_logger::init();
    while let Err(e) = main_impl() {
        let report = e.causes().skip(1).fold(format!("{}", e), |acc, err| {
            format!("{}: {}", acc, err)
        });
        error!("{}", report);
        if let Some(backtrace) = e.backtrace() {
            debug!("{}", backtrace);
        }
    }
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
    info!(
        "HOST: {}:{} resolved to {}", client.config().server()?, client.config().port(),
        client.config().socket_addr()?
    );
    client.identify()?;
    let splitter = Splitter::new();
    let client_splitter_future = client.stream().forward(splitter.clone());
    let saddr = client.config().socket_addr()?;
    let hostname = LatestHostname::new();
    let split_to_nowhere = splitter.split().map(|message| {
        let local_client = client.clone();
        info!("FROM {}: {}", saddr, message.to_string().trimmed());
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
        debug!("client connected: {}", baddr);
        let client_stream = splitter.split();
        trace!("finished replaying to {}", baddr);
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
                info!("TO {}: {}", &baddr, message.to_string().trimmed());
                message
            }
        ).map_err::<IrcError, _>(|()| unreachable!()));

        let filter_client = local_client.clone();
        let filter_hostname = hostname.clone();
        let to_real_client = reader.filter(move |message| {
            info!("FROM {}: {}", &baddr, message.to_string().trimmed());
            match message.command {
                // Complex (action-taking) filters
                Command::JOIN(ref chan, _, _) if filter_client.list_channels().map(|chans| {
                    chans.contains(chan)
                }).unwrap_or(false) => {
                    trace!("FILTERED LAST MESSAGE FROM {}", &baddr);
                    tx_bouncer_client.unbounded_send(Message {
                        tags: None,
                        prefix: filter_hostname.hostname(),
                        command: Command::JOIN(chan.to_owned(), None, None)
                    }).unwrap();
                    false
                }

                // Ordinary filters
                Command::Raw(ref cmd, _, _) if cmd == "USER" => {
                    trace!("FILTERED LAST MESSAGE FROM {}", &baddr);
                    false
                },
                Command::NICK(_) |
                Command::USER(_, _, _) |
                Command::CAP(_, _, _, _) |
                Command::QUIT(_) => {
                    trace!("FILTERED LAST MESSAGE FROM {}", &baddr);
                    false
                },
                _ => true,
            }
        }).for_each(move |message| {
            info!("TO {}: {}", &saddr, message.to_string().trimmed());
            local_client.send(message)
        });
        let baddr = baddr.clone();
        server_handle.spawn(to_bouncer_client.join(to_real_client).map(|_| ()).map_err(move |e| {
            let report = e.causes().skip(1).fold(format!("FOR {}: {}", baddr, e), |acc, err| {
                format!("{}: {}", acc, err)
            });
            error!("{}", report);
            if let Some(backtrace) = e.backtrace() {
                debug!("{}", backtrace);
            }
        }));
        Ok(())
    });

    reactor.run(bouncer.join(client_send_future)
                .join(client_splitter_future)
                .join(split_to_nowhere)
                .map(|_| ()))
}

trait StringTrim {
    fn trimmed(self) -> Self;
}

impl StringTrim for String {
    fn trimmed(mut self) -> Self {
        if self.ends_with('\n') {
            self.pop();
        }
        if self.ends_with('\r') {
            self.pop();
        }
        self
    }
}

#[derive(Clone)]
struct LatestHostname(Arc<Mutex<Option<String>>>);

impl LatestHostname {
    pub fn new() -> LatestHostname {
        LatestHostname(Arc::new(Mutex::new(None)))
    }

    pub fn update_hostname(&self, hostname: Option<String>) {
        debug!("HOST: updating hostname to {:?}", hostname);
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
        debug!("REPLAY SEALED");
        self.1.store(true, Ordering::SeqCst)
    }

    pub fn is_ready(&self) -> bool {
        self.1.load(Ordering::SeqCst)
    }

    pub fn push(&self, message: Message) {
        debug!("REPLAY PUSHED: {}", message.to_string().trimmed());
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
            trace!("REPLAY: replaying: {}", message.to_string().trimmed());
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
        if self.buffer.is_ready() == false {
            self.buffer.push(item.clone());
            match item.command {
                Command::Response(Response::RPL_ENDOFMOTD, _, _) |
                Command::Response(Response::ERR_NOMOTD, _, _) => self.buffer.ready(),
                _ => ()
            }
        }
        let mut senders = self.senders.lock().expect("unreachable");
        *senders = senders.clone().into_iter().filter_map(|mut sender| {
            sender.start_send(item.clone()).ok().map(|_| sender)
        }).collect();
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        let mut senders = self.senders.lock().expect("unreachable");
        *senders = senders.clone().into_iter().filter_map(|mut sender| {
            sender.poll_complete().ok().map(|_| sender)
        }).collect();
        Ok(Async::Ready(()))
    }
}
