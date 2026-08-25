//! The libp2p node shared by the client, the worker and the bootstrap server.
//!
//! Everything the rest of the codebase needs is on [`Node`]: join the network,
//! say what you serve, find who serves something, open a stream to them. The
//! swarm itself lives on one task and is reached by message, so callers never
//! touch libp2p types beyond `PeerId` and `Multiaddr`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use libp2p::futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    dcutr, identify, kad, mdns, noise, ping, relay, tcp, yamux, Multiaddr, PeerId, Swarm,
};
use libp2p_stream as stream;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{ident, P2pError, Result};

/// The one stream protocol rootmode speaks. The framing inside it is the same
/// newline-delimited RootmodeProtocol v1 JSON the WebSocket transport uses.
pub const PROTOCOL: libp2p::StreamProtocol = libp2p::StreamProtocol::new("/rootmode/1.0.0");

/// Identify's agent string, so `rootmode-worker` nodes are recognisable in a
/// packet capture or a bootstrap log.
const AGENT: &str = concat!("rootmode/", env!("CARGO_PKG_VERSION"));

/// How long a provider record lives in the DHT after the last publish.
///
/// libp2p's default is 48 hours, which is why a worker that died yesterday
/// still shows up as a provider. Twenty minutes is long enough that a live
/// node republishing every few minutes never flickers out, and short enough
/// that a crashed one is gone from the next lookup.
pub const PROVIDER_TTL: Duration = Duration::from_secs(20 * 60);

/// DHT key for "who can do this?". `cap` is `llm` or `image`.
pub fn cap_key(cap: &str) -> kad::RecordKey {
    kad::RecordKey::new(&format!("rootmode/cap/{cap}").into_bytes())
}

/// DHT key for "who serves this exact model?".
pub fn model_key(model_id: &str) -> kad::RecordKey {
    kad::RecordKey::new(&format!("rootmode/model/{model_id}").into_bytes())
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    /// Learns peers' addresses and tells them ours. Without it, a NAT'd node
    /// never discovers the address others can actually reach it on.
    identify: identify::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    /// Keeps NAT bindings warm and surfaces dead peers.
    ping: ping::Behaviour,
    stream: stream::Behaviour,
    dcutr: dcutr::Behaviour,
    /// Present on every node; used only when a reservation is requested.
    relay_client: relay::client::Behaviour,
    /// The bootstrap node runs the relay side of that.
    relay: Toggle<relay::Behaviour>,
    /// Finds peers on the same network with nothing configured. This is what
    /// makes a laptop and a GPU box on one LAN discover each other without
    /// anybody pasting an address.
    mdns: Toggle<mdns::tokio::Behaviour>,
}

// No `Debug`: `Identity` deliberately has none, so a config carrying a secret
// key cannot be logged by accident.
#[derive(Clone)]
pub struct NodeConfig {
    pub identity: rootmode_core::Identity,
    /// Addresses to listen on. Empty means outbound-only (a desktop client).
    pub listen: Vec<Multiaddr>,
    /// Entry points into the network. `/ip4/…/tcp/4001/p2p/<peer id>`.
    pub bootstrap: Vec<Multiaddr>,
    /// Serve DHT queries for others. True for the bootstrap node and for any
    /// worker with a public address.
    pub dht_server: bool,
    /// Run the relay service so NAT'd nodes can be reached through this one.
    pub relay_server: bool,
    /// Ask a bootstrap node for a relay reservation, so peers can reach this
    /// node even though it cannot accept a direct connection.
    pub relay_reservation: bool,
    /// Find peers on the local network automatically. On by default: it is
    /// the difference between "it just works at home" and "paste this string".
    pub local_discovery: bool,
    /// Addresses to advertise instead of, or as well as, what we bound to.
    ///
    /// Needed whenever the address others must dial is not the address this
    /// process sees: a container with published ports, a static NAT mapping, a
    /// host behind a load balancer.
    pub external: Vec<Multiaddr>,
    /// How long other nodes keep our provider records after the last publish.
    /// Override only in tests that need a record to expire in seconds.
    pub provider_ttl: Duration,
}

impl NodeConfig {
    pub fn new(identity: rootmode_core::Identity) -> Self {
        Self {
            identity,
            listen: Vec::new(),
            bootstrap: Vec::new(),
            dht_server: false,
            relay_server: false,
            relay_reservation: false,
            local_discovery: true,
            external: Vec::new(),
            provider_ttl: PROVIDER_TTL,
        }
    }
}

enum Command {
    Listeners(oneshot::Sender<Vec<Multiaddr>>),
    LocalPeers(oneshot::Sender<Vec<PeerId>>),
    Provide(Vec<kad::RecordKey>, oneshot::Sender<()>),
    FindProviders(kad::RecordKey, oneshot::Sender<Vec<PeerId>>),
    Bootstrap,
    PeerCount(oneshot::Sender<usize>),
    ConnectedPeers(oneshot::Sender<Vec<PeerId>>),
    AddAddress(PeerId, Multiaddr),
    FindPeer(PeerId, oneshot::Sender<()>),
    Dial(Multiaddr, oneshot::Sender<Result<()>>),
    KnownAddresses(PeerId, oneshot::Sender<Vec<Multiaddr>>),
}

/// Something happened that the rest of the app may want to act on.
#[derive(Debug, Clone)]
pub enum NodeEvent {
    /// A peer announced itself on the local network. Worth looking at
    /// immediately rather than at the next poll.
    PeerDiscovered(PeerId),
}

/// Handle to a running node.
///
/// **Keep it alive.** The swarm runs for as long as at least one handle
/// exists; dropping the last one stops the event loop and closes every
/// listener. Clone it freely — that is how a node is shared.
#[derive(Clone)]
pub struct Node {
    peer_id: PeerId,
    hex_peer_id: String,
    commands: mpsc::Sender<Command>,
    control: stream::Control,
    events: broadcast::Sender<NodeEvent>,
}

impl Node {
    /// Start the node. Returns the handle and the stream of inbound rootmode
    /// connections — a client that never serves can drop the latter.
    pub fn start(config: NodeConfig) -> Result<(Node, stream::IncomingStreams)> {
        let keypair = ident::keypair_from(&config.identity)?;
        let peer_id = keypair.public().to_peer_id();
        let bootstrap_count = config.bootstrap.len();

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| P2pError::Startup(e.to_string()))?
            .with_dns()
            .map_err(|e| P2pError::Startup(e.to_string()))?
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| P2pError::Startup(e.to_string()))?
            .with_behaviour(|key, relay_client| {
                let mut kad_config = kad::Config::new(kad::PROTOCOL_NAME);
                // The TTL is what actually evicts a dead worker. Republish at
                // a third of that so a live node refreshes well inside the
                // window even if the worker's own loop is late.
                let ttl = config.provider_ttl;
                kad_config.set_provider_record_ttl(Some(ttl));
                kad_config.set_provider_publication_interval(Some((ttl / 3).max(Duration::from_secs(1))));
                // A node with no bootstrap addresses *is* the entry point;
                // periodically retrying a walk it cannot start only logs noise.
                if bootstrap_count == 0 {
                    kad_config.set_periodic_bootstrap_interval(None);
                }

                Behaviour {
                    identify: identify::Behaviour::new(
                        identify::Config::new("/rootmode/id/1.0.0".into(), key.public())
                            .with_agent_version(AGENT.into()),
                    ),
                    kad: kad::Behaviour::with_config(
                        key.public().to_peer_id(),
                        kad::store::MemoryStore::new(key.public().to_peer_id()),
                        kad_config,
                    ),
                    ping: ping::Behaviour::new(ping::Config::new()),
                    stream: stream::Behaviour::new(),
                    dcutr: dcutr::Behaviour::new(key.public().to_peer_id()),
                    relay_client,
                    relay: Toggle::from(config.relay_server.then(|| {
                        relay::Behaviour::new(key.public().to_peer_id(), relay::Config::default())
                    })),
                    mdns: Toggle::from(
                        config
                            .local_discovery
                            .then(|| {
                                mdns::tokio::Behaviour::new(
                                    mdns::Config::default(),
                                    key.public().to_peer_id(),
                                )
                                .inspect_err(|e| {
                                    tracing::warn!("local network discovery unavailable: {e}")
                                })
                                .ok()
                            })
                            .flatten(),
                    ),
                }
            })
            .map_err(|e| P2pError::Startup(e.to_string()))?
            // libp2p's own idle timer, not this app's job timeout: it counts
            // a connection idle from the moment it has no open substream, and
            // a long-running job's substream stays open (silently, waiting on
            // a slow or cold-starting worker) for as long as the job takes.
            // 120s — libp2p's usual default order of magnitude — was closing
            // that connection out from under jobs that take longer than that
            // to produce a first token, most visibly a worker whose GPU is
            // still loading a model: the connection was reaped mid-job, the
            // worker's generation aborted, and the buyer side saw nothing
            // but a closed stream. Matches `net::JOB_TIMEOUT` /
            // `p2p::run_job`'s own per-job deadline, which is the timeout
            // that should actually decide when a slow job gives up.
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(600)))
            .build();

        if config.dht_server {
            swarm.behaviour_mut().kad.set_mode(Some(kad::Mode::Server));
        }

        // Declared addresses win over guessed ones: these go into the provider
        // records other peers dial.
        for addr in &config.external {
            tracing::info!("advertising external address {addr}");
            swarm.add_external_address(addr.clone());
        }

        for addr in &config.listen {
            swarm
                .listen_on(addr.clone())
                .map_err(|e| P2pError::Startup(format!("cannot listen on {addr}: {e}")))?;
        }

        // Relays to reserve with once connected, by peer id.
        let mut relays: HashMap<PeerId, Multiaddr> = HashMap::new();

        // Seed the routing table, then ask for a relay slot if we need one.
        for addr in &config.bootstrap {
            // Only an address that names its peer can be put in the routing
            // table up front. Without one we dial and learn who answered,
            // which identify then feeds into Kademlia.
            if let Some(peer) = peer_of(addr) {
                swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
            }
            if let Err(e) = swarm.dial(addr.clone()) {
                tracing::warn!("cannot dial bootstrap {addr}: {e}");
            }
            if config.relay_reservation {
                // A circuit address has to name the relay: there is no way to
                // ask "whoever answers" to hold a reservation for you.
                match peer_of(addr) {
                    None => tracing::warn!(
                        "cannot use {addr} as a relay: add /p2p/<peer id> to it. \
                         Without a relay this node is only reachable if it can \
                         accept inbound connections."
                    ),
                    // Asked for once we are *connected*, not here. Requesting a
                    // reservation while our own dial to the relay is still in
                    // flight makes the relay client issue a second dial, which
                    // the swarm refuses (`DisconnectedAndNotDialing`) — and the
                    // refusal silently closes the listener. Reserving after the
                    // connection exists avoids the race entirely, and re-runs
                    // if the relay drops and comes back.
                    Some(peer) => {
                        relays.insert(peer, addr.clone());
                    }
                }
            }
        }

        let control = swarm.behaviour().stream.new_control();
        let incoming = control
            .clone()
            .accept(PROTOCOL)
            .map_err(|e| P2pError::Startup(format!("cannot accept {PROTOCOL}: {e}")))?;

        let (tx, rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(64);
        let node = Node {
            peer_id,
            hex_peer_id: config.identity.peer_id(),
            commands: tx,
            control,
            events: events.clone(),
        };

        tokio::spawn(
            EventLoop::new(
                swarm,
                rx,
                !config.bootstrap.is_empty(),
                events,
                relays,
                config.bootstrap.clone(),
            )
            .run(),
        );

        Ok((node, incoming))
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// The rootmode form of the same identity.
    pub fn hex_peer_id(&self) -> &str {
        &self.hex_peer_id
    }

    pub async fn listeners(&self) -> Vec<Multiaddr> {
        self.ask(Command::Listeners).await.unwrap_or_default()
    }

    /// Announce that this node serves these keys, and only these keys.
    ///
    /// Replaces the previous set: a model that dropped off the announce is
    /// withdrawn, and calling this again with the same keys refreshes their
    /// TTL. Pass nothing to withdraw everything — that is [`Node::withdraw`].
    pub async fn provide(&self, keys: impl IntoIterator<Item = kad::RecordKey>) {
        let keys: Vec<_> = keys.into_iter().collect();
        let _ = self.ask(|tx| Command::Provide(keys, tx)).await;
    }

    /// Stop announcing. Remote records live until their TTL; this stops us
    /// from refreshing them.
    pub async fn withdraw(&self) {
        self.provide(std::iter::empty()).await;
    }

    /// Remember where a peer lives, for one you were told about rather than
    /// found. Without this a pasted address is only a key, and dialling it
    /// depends on somebody else knowing the route.
    pub async fn add_address(&self, peer: PeerId, addr: Multiaddr) {
        let _ = self.commands.send(Command::AddAddress(peer, addr)).await;
    }

    /// Watch for things worth reacting to. Polling for a peer that announced
    /// itself a moment ago is how you end up waiting a minute for something
    /// that already happened.
    pub fn events(&self) -> broadcast::Receiver<NodeEvent> {
        self.events.subscribe()
    }

    /// Peers seen on this network, whether or not the DHT knows anything
    /// about them.
    pub async fn local_peers(&self) -> Vec<PeerId> {
        self.ask(Command::LocalPeers).await.unwrap_or_default()
    }

    pub async fn find_providers(&self, key: kad::RecordKey) -> Vec<PeerId> {
        self.ask(|tx| Command::FindProviders(key, tx))
            .await
            .unwrap_or_default()
    }

    /// How many peers this node is connected to right now. Zero after a
    /// reasonable wait means the entry points did not answer.
    pub async fn connected_peers(&self) -> usize {
        self.ask(Command::PeerCount).await.unwrap_or(0)
    }

    /// Who we are connected to. Dialling an address with no `/p2p/` suffix
    /// and then reading this is how you learn a node's identity.
    pub async fn connected_peer_ids(&self) -> Vec<PeerId> {
        self.ask(Command::ConnectedPeers).await.unwrap_or_default()
    }

    pub async fn bootstrap(&self) {
        let _ = self.commands.send(Command::Bootstrap).await;
    }

    pub async fn dial(&self, addr: Multiaddr) -> Result<()> {
        match self.ask(|tx| Command::Dial(addr, tx)).await {
            Some(result) => result,
            None => Err(P2pError::Startup("node event loop stopped".into())),
        }
    }

    pub async fn known_addresses(&self, peer: PeerId) -> Vec<Multiaddr> {
        self.ask(|tx| Command::KnownAddresses(peer, tx))
            .await
            .unwrap_or_default()
    }

    /// Open a rootmode stream to `peer`, dialling if needed.
    ///
    /// A provider record names a peer, not a route to it. If we do not yet
    /// know where this one lives, ask the DHT before dialling — otherwise a
    /// peer we just discovered fails with "no addresses", which looks like the
    /// peer is broken when it is only unfamiliar.
    pub async fn open(&self, peer: PeerId) -> Result<libp2p::Stream> {
        if self.known_addresses(peer).await.is_empty() {
            self.find_peer(peer).await;
        }
        self.control
            .clone()
            .open_stream(peer, PROTOCOL)
            .await
            .map_err(|e| P2pError::Dial(format!("{peer}: {e}")))
    }

    /// Walk the DHT towards `peer` so its addresses land in the routing table.
    pub async fn find_peer(&self, peer: PeerId) {
        let _ = self.ask(|tx| Command::FindPeer(peer, tx)).await;
    }

    async fn ask<T>(&self, build: impl FnOnce(oneshot::Sender<T>) -> Command) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.commands.send(build(tx)).await.ok()?;
        rx.await.ok()
    }
}

/// `0.0.0.0` / `::` are placeholders, not addresses anyone can dial.
/// Whether an address has any chance of working from another network: its
/// transport starts with a public IP or a DNS name. Loopback, RFC1918,
/// CGNAT, link-local and docker-bridge ranges do not travel. For a relay
/// circuit this judges the relay's own address — a circuit through a
/// private relay address is as dead as the address itself.
fn reachable_from_afar(addr: &Multiaddr) -> bool {
    match addr.iter().next() {
        Some(Protocol::Ip4(ip)) => {
            let cgnat = ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000) == 64;
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || cgnat)
        }
        Some(Protocol::Ip6(ip)) => {
            let unique_local = (ip.segments()[0] & 0xfe00) == 0xfc00;
            let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
            !(ip.is_loopback() || ip.is_unspecified() || unique_local || link_local)
        }
        Some(Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) | Protocol::Dnsaddr(_)) => {
            true
        }
        _ => false,
    }
}

fn is_unspecified(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        libp2p::multiaddr::Protocol::Ip4(ip) => ip.is_unspecified(),
        libp2p::multiaddr::Protocol::Ip6(ip) => ip.is_unspecified(),
        _ => false,
    })
}

fn peer_of(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| match p {
        libp2p::multiaddr::Protocol::P2p(peer) => Some(peer),
        _ => None,
    })
}

struct EventLoop {
    swarm: Swarm<Behaviour>,
    commands: mpsc::Receiver<Command>,
    listeners: Vec<Multiaddr>,
    /// Provider queries waiting on a result, and what they have seen so far.
    pending_providers: HashMap<kad::QueryId, (oneshot::Sender<Vec<PeerId>>, HashSet<PeerId>)>,
    /// Peer lookups waiting to finish.
    pending_lookups: HashMap<kad::QueryId, oneshot::Sender<()>>,
    /// Peers seen on this network via mDNS.
    local_peers: HashSet<PeerId>,
    events: broadcast::Sender<NodeEvent>,
    bootstrapped: bool,
    have_bootstrap: bool,
    /// Relays we want a reservation from, by peer id.
    relays: HashMap<PeerId, Multiaddr>,
    /// Those we have already asked, so a reconnect does not ask twice while
    /// the first reservation is still good.
    reserved: HashSet<PeerId>,
    /// Entry points, kept so we can dial them again.
    ///
    /// A laptop changes networks: wifi to cellular, one office to another, a
    /// lid closed and opened somewhere else. Every connection dies with the
    /// old route, and Kademlia cannot recover on its own — its periodic
    /// bootstrap walks the DHT *through* peers, and there are none left. Until
    /// something re-dials, the node is silently alone for as long as it stays
    /// running.
    entry_points: Vec<Multiaddr>,
    /// Keys we are currently announcing, so a later `provide` can withdraw
    /// the ones that are no longer in the set.
    provided: HashSet<kad::RecordKey>,
}

impl EventLoop {
    fn new(
        swarm: Swarm<Behaviour>,
        commands: mpsc::Receiver<Command>,
        have_bootstrap: bool,
        events: broadcast::Sender<NodeEvent>,
        relays: HashMap<PeerId, Multiaddr>,
        entry_points: Vec<Multiaddr>,
    ) -> Self {
        Self {
            swarm,
            commands,
            listeners: Vec::new(),
            pending_providers: HashMap::new(),
            pending_lookups: HashMap::new(),
            local_peers: HashSet::new(),
            events,
            bootstrapped: false,
            have_bootstrap,
            relays,
            reserved: HashSet::new(),
            entry_points,
            provided: HashSet::new(),
        }
    }

    async fn run(mut self) {
        // Cheap enough to run often, and the thing it recovers from — a
        // network change — is invisible until something tries to dial.
        //
        // Starts one period late on purpose: `interval` fires immediately, and
        // at that instant the opening dial is still in flight, so the first
        // tick would always find zero peers and dial everything a second time.
        let mut watchdog = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(20),
            Duration::from_secs(20),
        );
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_event(event),
                command = self.commands.recv() => match command {
                    Some(c) => self.on_command(c),
                    None => return, // every handle dropped
                },
                _ = watchdog.tick() => self.reconnect_if_alone(),
            }
        }
    }

    /// With nobody left to talk to, dial the entry points again.
    ///
    /// Only when *every* connection is gone, so this stays quiet in normal
    /// operation and does not fight libp2p over a peer that is merely slow.
    fn reconnect_if_alone(&mut self) {
        if self.entry_points.is_empty() || self.swarm.connected_peers().next().is_some() {
            return;
        }

        tracing::info!("no peers left — dialling the entry points again");
        for addr in &self.entry_points {
            if let Err(e) = self.swarm.dial(addr.clone()) {
                tracing::debug!("cannot re-dial {addr}: {e}");
            }
        }
        // A fresh walk once something answers; and let a lost reservation be
        // asked for again.
        self.bootstrapped = false;
        self.reserved.clear();
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Listeners(tx) => {
                let _ = tx.send(self.listeners.clone());
            }
            Command::LocalPeers(tx) => {
                let _ = tx.send(self.local_peers.iter().copied().collect());
            }
            Command::Provide(keys, tx) => {
                let next: HashSet<_> = keys.into_iter().collect();
                for key in self.provided.difference(&next) {
                    self.swarm.behaviour_mut().kad.stop_providing(key);
                }
                for key in &next {
                    if let Err(e) = self.swarm.behaviour_mut().kad.start_providing(key.clone()) {
                        tracing::warn!("cannot advertise: {e}");
                    }
                }
                self.provided = next;
                let _ = tx.send(());
            }
            Command::FindProviders(key, tx) => {
                let id = self.swarm.behaviour_mut().kad.get_providers(key);
                self.pending_providers.insert(id, (tx, HashSet::new()));
            }
            Command::FindPeer(peer, tx) => {
                let id = self.swarm.behaviour_mut().kad.get_closest_peers(peer);
                self.pending_lookups.insert(id, tx);
            }
            Command::PeerCount(tx) => {
                let _ = tx.send(self.swarm.connected_peers().count());
            }
            Command::AddAddress(peer, addr) => {
                self.swarm.behaviour_mut().kad.add_address(&peer, addr);
            }
            Command::ConnectedPeers(tx) => {
                let _ = tx.send(self.swarm.connected_peers().copied().collect());
            }
            Command::Bootstrap => {
                if let Err(e) = self.swarm.behaviour_mut().kad.bootstrap() {
                    tracing::debug!("bootstrap: {e}");
                }
            }
            Command::Dial(addr, tx) => {
                let result = self
                    .swarm
                    .dial(addr.clone())
                    .map_err(|e| P2pError::Dial(format!("{addr}: {e}")));
                let _ = tx.send(result);
            }
            Command::KnownAddresses(peer, tx) => {
                let addrs = self
                    .swarm
                    .behaviour_mut()
                    .kad
                    .kbucket(peer)
                    .and_then(|bucket| {
                        bucket
                            .iter()
                            .find(|entry| *entry.node.key.preimage() == peer)
                            .map(|entry| entry.node.value.iter().cloned().collect::<Vec<_>>())
                    })
                    .unwrap_or_default();
                let _ = tx.send(addrs);
            }
        }
    }

    fn on_event(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                tracing::info!("listening on {address}");
                self.listeners.push(address.clone());

                // Provider records carry the addresses others should dial. A
                // node that never declares one is discoverable but unreachable,
                // so publish what we bound to — including relay circuit
                // addresses, which are exactly the reachable ones when this
                // node is behind NAT.
                if !is_unspecified(&address) {
                    self.swarm.add_external_address(address);
                }
            }

            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                tracing::debug!("connected to {peer_id}");
                // The first connection is usually the bootstrap node; once we
                // have one, walk the DHT to fill the routing table.
                if !self.bootstrapped && self.have_bootstrap {
                    self.bootstrapped = true;
                    let _ = self.swarm.behaviour_mut().kad.bootstrap();
                }

                // Now that a connection exists, ask this relay to hold a slot.
                if let Some(addr) = self.relays.get(&peer_id).cloned() {
                    if self.reserved.insert(peer_id) {
                        let circuit = addr.with(Protocol::P2pCircuit);
                        tracing::debug!("requesting a relay reservation via {circuit}");
                        if let Err(e) = self.swarm.listen_on(circuit.clone()) {
                            tracing::warn!("cannot request a relay reservation via {circuit}: {e}");
                            self.reserved.remove(&peer_id);
                        }
                    }
                }
            }

            // Identify is how we learn addresses that are actually reachable,
            // including relayed ones. Feed them to Kademlia — but not
            // wholesale: a peer reports every interface it bound, docker
            // bridges and private subnets included, plus one relay circuit
            // per such address. From outside its network those are black
            // holes: each dial sits in a TCP timeout, and the circuit
            // variants drown the relay connection's stream budget. A fleet
            // of 24 workers advertising three dead addresses each was enough
            // to make most of them look offline. The exception is a peer
            // mdns has seen on our own network — its private addresses are
            // exactly the reachable ones.
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                ..
            })) => {
                let local = self.local_peers.contains(&peer_id);
                for addr in info.listen_addrs {
                    if local || reachable_from_afar(&addr) {
                        self.swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                    }
                }
            }

            // A peer on this network announced itself. Put it in the routing
            // table so lookups can use it, and remember it so the client can
            // ask it directly what it serves.
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
                for (peer, addr) in peers {
                    self.swarm.behaviour_mut().kad.add_address(&peer, addr);
                    if self.local_peers.insert(peer) {
                        tracing::info!(%peer, "found a peer on this network");
                        // No receivers is normal — a worker does not listen.
                        let _ = self.events.send(NodeEvent::PeerDiscovered(peer));
                    }
                }
            }

            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
                for (peer, _) in peers {
                    self.local_peers.remove(&peer);
                }
            }

            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                id,
                result: kad::QueryResult::GetProviders(Ok(result)),
                step,
                ..
            })) => {
                if let Some((_, seen)) = self.pending_providers.get_mut(&id) {
                    if let kad::GetProvidersOk::FoundProviders { providers, .. } = &result {
                        seen.extend(providers.iter().copied());
                    }
                }
                if step.last {
                    if let Some((tx, seen)) = self.pending_providers.remove(&id) {
                        let _ = tx.send(seen.into_iter().collect());
                    }
                }
            }

            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                id,
                result: kad::QueryResult::GetProviders(Err(e)),
                ..
            })) => {
                tracing::debug!("provider query failed: {e}");
                if let Some((tx, seen)) = self.pending_providers.remove(&id) {
                    let _ = tx.send(seen.into_iter().collect());
                }
            }

            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                id,
                result: kad::QueryResult::GetClosestPeers(_),
                step,
                ..
            })) => {
                if step.last {
                    if let Some(tx) = self.pending_lookups.remove(&id) {
                        let _ = tx.send(());
                    }
                }
            }

            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                result: kad::QueryResult::StartProviding(result),
                ..
            })) => match result {
                Ok(kad::AddProviderOk { key }) => {
                    tracing::info!("advertised {}", String::from_utf8_lossy(key.as_ref()))
                }
                Err(e) => tracing::warn!("could not advertise: {e}"),
            },

            SwarmEvent::Behaviour(BehaviourEvent::RelayClient(
                relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
            )) => {
                tracing::info!("relay reservation accepted by {relay_peer_id}");
            }

            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(dcutr::Event {
                remote_peer_id,
                result,
            })) => match result {
                Ok(_) => tracing::info!("upgraded to a direct connection with {remote_peer_id}"),
                Err(e) => tracing::debug!("hole punch to {remote_peer_id} failed: {e}"),
            },

            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                tracing::debug!("dial failed for {peer_id:?}: {error}");
            }

            // A listener that dies takes a capability with it, and until now
            // both of these were discarded. That is how a relay reservation
            // can fail to happen while every other log line looks healthy:
            // the node keeps working perfectly, and is simply unreachable
            // from outside its own network.
            SwarmEvent::ListenerError { error, .. } => {
                tracing::warn!("a listener failed: {error}");
            }

            SwarmEvent::ListenerClosed {
                addresses, reason, ..
            } => {
                let circuit = addresses
                    .iter()
                    .any(|a| a.iter().any(|p| matches!(p, Protocol::P2pCircuit)));
                let addrs: Vec<String> = addresses.iter().map(|a| a.to_string()).collect();
                if circuit {
                    tracing::warn!(
                        "the relay reservation was lost ({}): {reason:?}. Peers outside this \
                         network cannot reach this node until it is granted again.",
                        addrs.join(", ")
                    );
                    // Let the next connection to that relay try again.
                    for addr in &addresses {
                        if let Some(peer) = peer_of(addr) {
                            self.reserved.remove(&peer);
                        }
                    }
                } else {
                    tracing::debug!("listener closed ({}): {reason:?}", addrs.join(", "));
                }
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judged(addr: &str) -> bool {
        reachable_from_afar(&addr.parse().unwrap())
    }

    #[test]
    fn addresses_that_cannot_travel_are_refused() {
        // What a dockerised worker actually advertises: every host interface.
        assert!(!judged("/ip4/172.17.0.1/tcp/4108"), "docker bridge");
        assert!(!judged("/ip4/10.114.0.7/tcp/4103"), "private VPC");
        assert!(!judged("/ip4/127.0.0.1/tcp/4001"), "loopback");
        assert!(!judged("/ip4/192.168.1.20/tcp/9944"), "home LAN");
        assert!(!judged("/ip4/100.64.0.1/tcp/1"), "CGNAT");
        assert!(!judged("/ip6/fe80::1/tcp/1"), "link-local v6");
        // A circuit is only as reachable as the relay it goes through.
        assert!(!judged(
            "/ip4/172.17.0.1/tcp/4001/p2p/12D3KooWLXbwVxwKHvEEMdbEbNCv49wVUKc2mieGZfDGw73hj3YW/p2p-circuit"
        ));
    }

    #[test]
    fn addresses_the_wider_internet_can_dial_are_kept() {
        assert!(judged("/ip4/165.245.246.193/tcp/4101"));
        assert!(judged("/dns4/bootstrap.rootmode.ai/tcp/4001"));
        assert!(judged(
            "/ip4/165.245.246.193/tcp/4001/p2p/12D3KooWLXbwVxwKHvEEMdbEbNCv49wVUKc2mieGZfDGw73hj3YW/p2p-circuit"
        ));
    }
}
