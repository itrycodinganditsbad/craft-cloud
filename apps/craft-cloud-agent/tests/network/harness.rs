//! The simulated world of `tests/network`: the server host (hyper serving the real router, with the server's faults
//! and bookkeeping), the agents' hosts and their [`Connection`] ([`SimConn`]), the weather on each agent's link, and
//! how a seed's world is made ([`world`]) and run ([`simulate`]).

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::ConnectInfo;
use bytes::Bytes;
use craft_cloud_agent::http::{Answer, AnswerBody, Connection, DeviceToken, HttpTransport, Request, ServerUrl};
use craft_cloud_proto::api::CommitRequest;
use craft_cloud_server::AppState;
use craft_cloud_sync::{Config, Engine, MemFs, State, TransportError};
use http_body::{Frame, SizeHint};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HOST, HeaderMap, HeaderValue};
use hyper::{Method, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::sync::{mpsc, oneshot};
use tower_service::Service;

use crate::common::sim::{Rng, tracing};
use crate::common::{Machine, STAMP, Server};

pub(crate) const SERVER: &str = "server";
const PORT: u16 = 80;
pub(crate) const MACHINES: usize = 3;
/// Each request's number, from the agent's connection to the server's, to match an answer that never arrived with
/// what the server did.
const REQUEST_ID: &str = "x-sim-request";
/// The server's time of day when a simulation starts (milliseconds since the Unix epoch): its clock is
/// this plus the simulated time, never the real one.
pub(crate) const SIM_EPOCH_MS: i64 = 1_760_000_000_000;

pub(crate) type Agent = Machine<HttpTransport<SimConn>>;

/// What the hosts of one seed share: the server's state, the way to each agent's host, what became of each request,
/// and the server's faults.
pub(crate) struct Net {
    pub(crate) server: Server,
    router: axum::Router,
    /// Where an agent's connection hands its requests: its host's network, once the host runs.
    hosts: Vec<Mutex<Option<mpsc::UnboundedSender<Exchange>>>>,
    next_request: AtomicU64,
    /// Every request sent, by its number.
    requests: Mutex<BTreeMap<u64, Sent>>,
    /// The server's faults.
    pub(crate) faults: Mutex<Faults>,
    /// Whether a task keeps time still while the database works (see [`hold_time`]).
    holding_time: AtomicBool,
    /// The server's page size for the tree and the change feed.
    page: u32,
    /// What happened, for `SIM_TRACE` and the determinism check.
    pub(crate) events: Mutex<Vec<String>>,
    /// Every request the server took, by method and path, for the scenarios.
    served: Mutex<Vec<(Method, String)>>,
}

/// One request: which machine sent it, and what became of it.
#[derive(Default)]
struct Sent {
    /// The machine that sent it.
    host: usize,
    commit: bool,
    /// A long-poll (it waits for changes).
    poll: bool,
    /// When it was sent, in simulated time.
    at: Duration,
    /// The server applied it (a commit that went through).
    applied: bool,
    /// Its whole answer reached the machine.
    delivered: bool,
    /// Already counted as a lost answer.
    counted: bool,
}

/// What the server does to an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    /// The request applies, then the connection breaks before the answer goes out.
    Lose,
    /// The answer breaks off halfway through its body.
    Cut,
}

/// The server's faults: random ones (one request in `lose` loses its answer, one answer in `cut` is cut; 0: never),
/// and ones a scenario arms for the next request with a method and a path prefix.
pub(crate) struct Faults {
    rng: Rng,
    lose: usize,
    cut: usize,
    /// Whether the random faults are on (they stop when the network heals).
    pub(crate) on: bool,
    armed: Vec<(Method, &'static str, Fault)>,
}

impl Net {
    /// Records `line` in the trace (and prints it with `SIM_TRACE`).
    pub(crate) fn event(&self, line: String) {
        if tracing() {
            eprintln!("TRACE {line}");
        }
        self.events.lock().unwrap().push(line);
    }

    /// The fault for an answer to `method` `path`: an armed one first, then a random one.
    fn fault(&self, method: &Method, path: &str) -> Fault {
        let mut faults = self.faults.lock().unwrap();
        if let Some(k) = faults.armed.iter().position(|(m, p, _)| m == method && path.starts_with(p)) {
            return faults.armed.remove(k).2;
        }
        if !faults.on {
            return Fault::None;
        }
        let (lose, cut) = (faults.lose, faults.cut);
        if *method != Method::GET {
            if lose > 0 && faults.rng.below(lose) == 0 { Fault::Lose } else { Fault::None }
        } else if cut > 0 && faults.rng.below(cut) == 0 {
            Fault::Cut
        } else {
            Fault::None
        }
    }

    /// Arms a fault for the next answer to `method` `path…`.
    pub(crate) fn arm(&self, method: Method, path: &'static str, fault: Fault) {
        self.faults.lock().unwrap().armed.push((method, path, fault));
    }

    /// Whether a commit `host` sent applied on the server while its answer never arrived, since the last look.
    pub(crate) fn lost_commit_answer(&self, host: usize) -> bool {
        let mut lost = false;
        for sent in self.requests.lock().unwrap().values_mut().filter(|s| s.host == host && s.commit && s.applied && !s.delivered && !s.counted) {
            sent.counted = true;
            lost = true;
        }
        lost
    }

    /// How many requests the server took for `method` `path…` so far.
    pub(crate) fn served(&self, method: &Method, path: &str) -> usize {
        self.served.lock().unwrap().iter().filter(|(m, p)| m == method && p.starts_with(path)).count()
    }

    /// When machine `host` sent its long-polls, in simulated time.
    pub(crate) fn polls_of(&self, host: usize) -> Vec<Duration> {
        self.requests.lock().unwrap().values().filter(|r| r.host == host && r.poll).map(|r| r.at).collect()
    }
}

// The server.

/// The server's answer to one request from `peer`, through the real router, with the server's faults.
async fn answer(net: Arc<Net>, peer: SocketAddr, req: hyper::Request<Incoming>) -> Result<hyper::Response<Out>, Lost> {
    let (mut parts, body) = req.into_parts();
    let body = body.collect().await.map_err(|_| Lost)?.to_bytes();
    let n = parts.headers.get(REQUEST_ID).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok());
    let path = parts.uri.path().to_owned();
    let method = parts.method.clone();
    // A server with small pages: the tree and the change feed come in many.
    if (path == "/api/v1/changes" || path == "/api/v1/tree") && !parts.uri.query().unwrap_or("").contains("limit=") {
        let query = parts.uri.query().map_or_else(String::new, |q| format!("{q}&"));
        if let Ok(uri) = format!("{path}?{query}limit={}", net.page).parse() {
            parts.uri = uri;
        }
    }
    // What `axum::serve` gives every request (the sign-in's rate limit keys on it).
    parts.extensions.insert(ConnectInfo(peer));
    net.served.lock().unwrap().push((method.clone(), path.clone()));
    let commit = (method == Method::POST && path == "/api/v1/commit").then(|| serde_json::from_slice::<CommitRequest>(&body).ok()).flatten();
    let request = hyper::Request::from_parts(parts, Full::new(body));
    let Ok(response) = Guarded::new(&net, net.router.clone().call(request)).await;
    let (parts, body) = response.into_parts();
    let bytes = Guarded::new(&net, body.collect()).await.map_err(|_| Lost)?.to_bytes();
    if tracing()
        && let Some(commit) = &commit
    {
        eprintln!("TRACE {} commit {n:?} answered {}: {:?}", stamp(), parts.status.as_u16(), commit.ops);
    }
    if parts.status.is_success()
        && let Some(commit) = commit
    {
        net.server.record_commit(&commit.ops);
        if let Some(n) = n
            && let Some(sent) = net.requests.lock().unwrap().get_mut(&n)
        {
            sent.applied = true;
        }
    }
    match net.fault(&method, &path) {
        Fault::Lose => {
            net.event(format!("server lost the answer to {method} {path} ({})", parts.status.as_u16()));
            Err(Lost)
        }
        Fault::Cut if bytes.len() > 1 => {
            net.event(format!("server cut the answer to {method} {path}"));
            Ok(hyper::Response::from_parts(parts, Out::cut(bytes)))
        }
        _ => Ok(hyper::Response::from_parts(parts, Out::whole(bytes))),
    }
}

/// The connection broke before the answer went out.
#[derive(Debug)]
struct Lost;

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the answer was lost")
    }
}

impl std::error::Error for Lost {}

/// An answer's body: all of it, or its first half and then a broken connection (it still announces its whole
/// length, so the agent sees a body that ends early).
struct Out {
    data: Option<Bytes>,
    len: u64,
    cut: bool,
}

impl Out {
    fn whole(data: Bytes) -> Self {
        Self { len: data.len() as u64, data: (!data.is_empty()).then_some(data), cut: false }
    }

    fn cut(data: Bytes) -> Self {
        Self { len: data.len() as u64, data: Some(data.slice(..data.len() / 2)), cut: true }
    }
}

impl http_body::Body for Out {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        Poll::Ready(match this.data.take() {
            Some(data) => Some(Ok(Frame::data(data))),
            None if this.cut => {
                this.cut = false;
                Some(Err(std::io::Error::other("the answer was cut")))
            }
            None => None,
        })
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none() && !self.cut
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.len)
    }
}

/// A future of the server's (a request being handled) that keeps simulated time still whenever it waits while the
/// database works.
struct Guarded<F> {
    net: Arc<Net>,
    inner: Pin<Box<F>>,
}

impl<F> Guarded<F> {
    fn new(net: &Arc<Net>, inner: F) -> Self {
        Self { net: net.clone(), inner: Box::pin(inner) }
    }
}

impl<F: Future> Future for Guarded<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let out = this.inner.as_mut().poll(cx);
        if out.is_pending() {
            hold_time(&this.net);
        }
        out
    }
}

/// Clears [`Net::holding_time`] when the task that keeps time still ends, finished or cancelled (a runtime that
/// stops drops its tasks), so a later request starts a new one.
struct Holding(Arc<Net>);

impl Drop for Holding {
    fn drop(&mut self) {
        self.0.holding_time.store(false, Ordering::Relaxed);
    }
}

/// SQLite answers from a thread of its own, outside the simulation. If simulated time went on while it works, how
/// far it got before an answer would depend on the real machine's speed, and a seed wouldn't replay. So while the
/// database is busy ([`craft_cloud_meta::Meta::busy`]) a blocking task waits for it: tokio's paused clock doesn't
/// move while a blocking task runs, so the server's work takes no simulated time. The task starts when a request
/// waits, looks again after the tasks queued before it ran (the commit runs in a task of its own), and ends once the
/// database stays idle for a few looks.
///
/// What this rests on:
/// - tokio's paused clock doesn't auto-advance while a `spawn_blocking` task runs (turmoil advances a host's time
///   by letting its paused runtime sleep through a tick);
/// - the database is one connection (SQLite in memory), so `busy` sees all its work; a pool of several would let
///   requests' database work overlap in an order the real machine decides;
/// - a request's database work starts within a few scheduler turns of its future first waiting (4 idle looks): one
///   that started later, with no further wait in between, would let time move;
/// - the busy wait looks every 20 µs of real time (it costs real time only, never simulated time).
fn hold_time(net: &Arc<Net>) {
    if net.holding_time.swap(true, Ordering::Relaxed) {
        return;
    }
    let net = net.clone();
    tokio::spawn(async move {
        let _holding = Holding(net.clone());
        let mut idle = 0;
        while idle < 4 {
            if net.server.meta.busy() {
                idle = 0;
                let meta = net.server.meta.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    while meta.busy() {
                        std::thread::sleep(Duration::from_micros(20));
                    }
                })
                .await;
            } else {
                idle += 1;
                tokio::task::yield_now().await;
            }
        }
    });
}

/// The server host: hyper serves the real router on turmoil's TCP.
async fn serve(net: Arc<Net>) -> turmoil::Result {
    let listener = turmoil::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, PORT)).await?;
    loop {
        let (stream, peer) = listener.accept().await?;
        let net = net.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| answer(net.clone(), peer, req));
            // A broken connection or a lost answer ends it; nothing to do.
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}

// The agents' connections.

/// One request handed to an agent's host, and where its answer goes.
struct Exchange {
    n: u64,
    request: hyper::Request<Full<Bytes>>,
    timeout: Duration,
    reply: oneshot::Sender<Result<Head, TransportError>>,
}

/// An answer's status and headers, and its body as it arrives.
struct Head {
    status: StatusCode,
    headers: HeaderMap,
    body: mpsc::UnboundedReceiver<Piece>,
}

/// A piece of an answer's body, as the agent's host passes it on.
enum Piece {
    Data(Bytes),
    End,
    Failed(String),
}

fn unavailable(why: impl std::fmt::Display) -> TransportError {
    TransportError::Unavailable(why.to_string())
}

/// An agent host: it carries each request its machine hands it to the server, on a connection of its own.
async fn agent_host(net: Arc<Net>, i: usize) -> turmoil::Result {
    let (tx, mut rx) = mpsc::unbounded_channel();
    if let Some(slot) = net.hosts.get(i) {
        *slot.lock().unwrap() = Some(tx);
    }
    while let Some(exchange) = rx.recv().await {
        tokio::spawn(carry(net.clone(), exchange));
    }
    Ok(())
}

/// Connects, sends the request and passes the answer on as it arrives, all within the request's timeout (the
/// whole-request timeout of `HttpTransport`; a partition stalls a connection until it ends it).
async fn carry(net: Arc<Net>, exchange: Exchange) {
    let Exchange { n, request, timeout, reply } = exchange;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut connection = None;
    let started = async {
        let stream = turmoil::net::TcpStream::connect((SERVER, PORT)).await.map_err(|e| unavailable(format!("can't connect: {e}")))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(unavailable)?;
        connection = Some(tokio::spawn(conn));
        sender.send_request(request).await.map_err(unavailable)
    };
    let response = match tokio::time::timeout_at(deadline, started).await {
        Ok(Ok(response)) => response,
        failed => {
            let _ = reply.send(Err(failed.map_or_else(|_| unavailable("timed out"), |r| r.err().unwrap_or_else(|| unavailable("no answer")))));
            if let Some(c) = connection {
                c.abort();
            }
            return;
        }
    };
    let (parts, mut body) = response.into_parts();
    let (pieces, rx) = mpsc::unbounded_channel();
    if reply.send(Ok(Head { status: parts.status, headers: parts.headers, body: rx })).is_ok() {
        loop {
            let piece = match tokio::time::timeout_at(deadline, body.frame()).await {
                Err(_) => Piece::Failed("timed out".into()),
                Ok(None) => {
                    if let Some(sent) = net.requests.lock().unwrap().get_mut(&n) {
                        sent.delivered = true;
                    }
                    Piece::End
                }
                Ok(Some(Err(e))) => Piece::Failed(e.to_string()),
                Ok(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => Piece::Data(data),
                    Err(_) => continue,
                },
            };
            let last = !matches!(piece, Piece::Data(_));
            if pieces.send(piece).is_err() || last {
                break;
            }
        }
    }
    if let Some(c) = connection {
        c.abort();
    }
}

/// An agent's [`Connection`]: hands each request to the machine's host, which carries it over the simulated network.
#[derive(Clone)]
pub(crate) struct SimConn {
    /// The machine whose host carries the requests.
    pub(crate) host: usize,
    /// The world it belongs to.
    pub(crate) net: Arc<Net>,
}

impl std::fmt::Debug for SimConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SimConn(m{})", self.host)
    }
}

#[async_trait]
impl Connection for SimConn {
    async fn send(&self, request: Request) -> Result<Answer, TransportError> {
        let Request { method, url, headers, body, timeout } = request;
        let n = self.net.next_request.fetch_add(1, Ordering::Relaxed);
        let commit = method == Method::POST && url.path().ends_with("/commit");
        let poll = url.query().is_some_and(|q| q.contains("wait="));
        self.net.requests.lock().unwrap().insert(n, Sent { host: self.host, commit, poll, at: turmoil::elapsed(), ..Sent::default() });
        let target = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let mut req = hyper::Request::builder().method(method).uri(target);
        if let Some(h) = req.headers_mut() {
            h.extend(headers);
            h.insert(HOST, HeaderValue::from_static("localhost"));
            h.insert(REQUEST_ID, HeaderValue::from(n));
        }
        let req = req.body(Full::new(body.unwrap_or_default())).map_err(|e| TransportError::Protocol(e.to_string()))?;
        let to = self.net.hosts.get(self.host).and_then(|h| h.lock().unwrap().clone()).ok_or_else(|| unavailable("no network yet"))?;
        let (reply, answer) = oneshot::channel();
        to.send(Exchange { n, request: req, timeout, reply }).map_err(|_| unavailable("the network is gone"))?;
        let head = answer.await.map_err(|_| unavailable("the request was dropped"))??;
        Ok(Answer { status: head.status, headers: head.headers, body: Box::new(SimBody(head.body)) })
    }
}

/// An answer's body as the agent's host passes it on.
struct SimBody(mpsc::UnboundedReceiver<Piece>);

#[async_trait]
impl AnswerBody for SimBody {
    async fn next(&mut self) -> Result<Option<Bytes>, TransportError> {
        match self.0.recv().await {
            Some(Piece::Data(data)) => Ok(Some(data)),
            Some(Piece::End) => Ok(None),
            Some(Piece::Failed(why)) => Err(TransportError::Unavailable(why)),
            None => Err(unavailable("the connection broke")),
        }
    }
}

// The weather.

/// How the network between machine `i` and the server misbehaves until `calm`: one thing at a time, at random
/// moments, for random lengths of time.
pub(crate) async fn weather(net: Arc<Net>, i: usize, mut rng: Rng, calm: Arc<AtomicBool>) {
    let host = format!("m{i}");
    loop {
        tokio::time::sleep(Duration::from_millis(rng.below(40_000) as u64)).await;
        if calm.load(Ordering::Relaxed) {
            return;
        }
        let (what, held, lasts) = match rng.below(8) {
            0 | 1 => {
                turmoil::partition(host.as_str(), SERVER);
                ("cut off", false, 1_000 + rng.below(90_000))
            }
            2 => {
                turmoil::partition_oneway(SERVER, host.as_str());
                ("loses the answers", false, 1_000 + rng.below(60_000))
            }
            3 => {
                turmoil::partition_oneway(host.as_str(), SERVER);
                ("loses the requests", false, 1_000 + rng.below(60_000))
            }
            _ => {
                turmoil::hold(host.as_str(), SERVER);
                ("waits", true, 100 + rng.below(10_000))
            }
        };
        net.event(format!("{} {host} {what} for {lasts} ms", stamp()));
        tokio::time::sleep(Duration::from_millis(lasts as u64)).await;
        if held {
            turmoil::release(host.as_str(), SERVER);
        } else {
            turmoil::repair(host.as_str(), SERVER);
        }
        net.event(format!("{} {host} back", stamp()));
    }
}

/// The simulated time, for the trace.
pub(crate) fn stamp() -> String {
    format!("@{}", turmoil::elapsed().as_millis())
}

// The world.

/// What a run's server and network are like.
pub(crate) struct Setup {
    /// The server's page size for the tree and the change feed.
    pub(crate) page: u32,
    /// The longest a message takes, in milliseconds.
    pub(crate) latency: u64,
    /// One answer in `lose` is lost after its request applied (0: none).
    pub(crate) lose: usize,
    /// One answer in `cut` is cut halfway (0: none).
    pub(crate) cut: usize,
    /// A second account, `bo`, that signs in with this password (the machines' account, `ana`, has none).
    pub(crate) password: Option<&'static str>,
}

/// The server's address, as the agents know it.
pub(crate) fn server_url() -> ServerUrl {
    ServerUrl::parse("http://localhost").unwrap()
}

/// The server's stores, the account and three machines (m0 lands downloads with one exchange, m1 in steps, m2 one
/// way or the other by seed, as in `tests/simulation.rs`), and what the hosts share. The server's clock is
/// [`SIM_EPOCH_MS`] plus the simulated time.
pub(crate) fn world(seed: u64, setup: &Setup) -> (Arc<Net>, Vec<Agent>) {
    let racy = if seed.is_multiple_of(2) { 2_000 } else { 0 };
    // The server's stores, the account and the devices, made before the simulation starts. The setup runtime must
    // let SQLite's connection go back to its pool before it ends: dropped on the way, it would take the in-memory
    // database with it.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (server, tokens) = runtime.block_on(async {
        let server = Server::new().await;
        let ana = server.user("ana").await;
        let mut tokens = Vec::new();
        for i in 0..MACHINES {
            let (token, hash) = craft_cloud_server::auth::new_token().unwrap();
            let device = server.meta.create_device(ana.id, &format!("m{i}"), &hash, 0).await.unwrap();
            tokens.push((device, token));
        }
        if let Some(password) = setup.password {
            let hash = craft_cloud_server::auth::hash_password(password).unwrap();
            server.meta.create_user("bo", &hash, craft_cloud_meta::Role::Member, 0).await.unwrap();
        }
        while server.meta.busy() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        (server, tokens)
    });
    drop(runtime);
    let clock = Arc::new(|| SIM_EPOCH_MS + turmoil::sim_elapsed().map_or(0, |t| i64::try_from(t.as_millis()).unwrap_or(i64::MAX)));
    let state = AppState::new(server.meta.clone(), server.store.clone(), Vec::new()).unwrap().with_clock(clock);
    let net = Arc::new(Net {
        server,
        router: craft_cloud_server::api::router(Arc::new(state)),
        hosts: (0..MACHINES).map(|_| Mutex::new(None)).collect(),
        next_request: AtomicU64::new(0),
        requests: Mutex::default(),
        faults: Mutex::new(Faults { rng: Rng::new(seed ^ 0xFA17_0000_0000_0007), lose: setup.lose, cut: setup.cut, on: true, armed: Vec::new() }),
        holding_time: AtomicBool::new(false),
        page: setup.page,
        events: Mutex::default(),
        served: Mutex::default(),
    });
    let url = server_url();
    let mut machines = Vec::new();
    for (i, (device, token)) in tokens.into_iter().enumerate() {
        let swap = match i {
            0 => true,
            1 => false,
            _ => seed % 4 < 2,
        };
        let fs = if swap { MemFs::new() } else { MemFs::new().without_swap() };
        let transport = HttpTransport::over(SimConn { host: i, net: net.clone() }, url.clone(), &DeviceToken::new(token)).unwrap();
        let mut config = Config::new(format!("m{i}"), device);
        config.racy_window_ms = racy;
        machines.push(Engine::new(fs, transport, State::default(), config, || STAMP).unwrap());
    }
    (net, machines)
}

/// Runs `driver` as the simulation's client, with the server and the agents' hosts, on a network whose messages
/// take up to `latency` ms. Time moves in steps of 10 ms.
pub(crate) fn simulate(seed: u64, latency: u64, net: &Arc<Net>, driver: impl Future<Output = ()> + 'static) {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(7 * 24 * 3600))
        .tick_duration(Duration::from_millis(10))
        .min_message_latency(Duration::from_millis(1))
        .max_message_latency(Duration::from_millis(latency))
        .rng_seed(seed)
        .build();
    let on_server = net.clone();
    sim.host(SERVER, move || serve(on_server.clone()));
    for i in 0..MACHINES {
        let on_agent = net.clone();
        sim.host(format!("m{i}"), move || agent_host(on_agent.clone(), i));
    }
    sim.client("driver", async move {
        driver.await;
        Ok(())
    });
    if let Err(e) = sim.run() {
        panic!("seed {seed}: {e}");
    }
}
