use std::{
    collections::HashMap, future::Future, net::SocketAddr, pin::Pin, sync::Arc, time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use derive_more::Debug;
use futures_lite::FutureExt;
use http::{header::CONNECTION, response::Builder as ResponseBuilder};
use hyper::{
    body::Incoming,
    header::{HeaderValue, UPGRADE},
    service::Service,
    upgrade::Upgraded,
    HeaderMap, Method, Request, Response, StatusCode,
};
use iroh_metrics::inc;
use tokio::{
    net::{TcpListener, TcpSocket, TcpStream},
    sync::mpsc,
};
use tokio_rustls_acme::AcmeAcceptor;
use tokio_tungstenite::{
    tungstenite::{handshake::derive_accept_key, protocol::Role},
    WebSocketStream,
};
use tokio_util::{codec::Framed, sync::CancellationToken, task::AbortOnDropHandle};
use tracing::{debug, debug_span, error, info, info_span, trace, warn, Instrument};

use crate::{
    defaults::DEFAULT_KEY_CACHE_CAPACITY,
    http::{Protocol, LEGACY_RELAY_PATH, RELAY_PATH, SUPPORTED_WEBSOCKET_VERSION},
    protos::relay::{recv_client_key, RelayCodec, PER_CLIENT_SEND_QUEUE_DEPTH, PROTOCOL_VERSION},
    server::{
        actor::{Message, ServerActorTask},
        client_conn::ClientConnConfig,
        metrics::Metrics,
        streams::{MaybeTlsStream, RelayedStream},
        ClientConnRateLimit,
    },
    KeyCache,
};

type BytesBody = http_body_util::Full<hyper::body::Bytes>;
type HyperError = Box<dyn std::error::Error + Send + Sync>;
type HyperResult<T> = std::result::Result<T, HyperError>;
type HyperHandler = Box<
    dyn Fn(Request<Incoming>, ResponseBuilder) -> HyperResult<Response<BytesBody>>
        + Send
        + Sync
        + 'static,
>;

/// Creates a new [`BytesBody`] with no content.
fn body_empty() -> BytesBody {
    http_body_util::Full::new(hyper::body::Bytes::new())
}

/// Creates a new [`BytesBody`] with given content.
fn body_full(content: impl Into<hyper::body::Bytes>) -> BytesBody {
    http_body_util::Full::new(content.into())
}

fn downcast_upgrade(upgraded: Upgraded) -> Result<(MaybeTlsStream, Bytes)> {
    match upgraded.downcast::<hyper_util::rt::TokioIo<MaybeTlsStream>>() {
        Ok(parts) => Ok((parts.io.into_inner(), parts.read_buf)),
        Err(_) => {
            bail!("could not downcast the upgraded connection to MaybeTlsStream")
        }
    }
}

/// The Relay HTTP server.
///
/// A running HTTP server serving the relay endpoint and optionally a number of additional
/// HTTP services added with [`ServerBuilder::request_handler`].  If configured using
/// [`ServerBuilder::tls_config`] the server will handle TLS as well.
///
/// Created using [`ServerBuilder::spawn`].
#[derive(Debug)]
pub(super) struct Server {
    addr: SocketAddr,
    http_server_task: AbortOnDropHandle<()>,
    cancel_server_loop: CancellationToken,
}

impl Server {
    /// Returns a handle for this server.
    ///
    /// The server runs in the background as several async tasks.  This allows controlling
    /// the server, in particular it allows gracefully shutting down the server.
    pub(super) fn handle(&self) -> ServerHandle {
        ServerHandle {
            cancel_token: self.cancel_server_loop.clone(),
        }
    }

    /// Closes the underlying relay server and the HTTP(S) server tasks.
    pub(super) fn shutdown(&self) {
        self.cancel_server_loop.cancel();
    }

    /// Returns the [`AbortOnDropHandle`] for the supervisor task managing the server.
    ///
    /// This is the root of all the tasks for the server.  Aborting it will abort all the
    /// other tasks for the server.  Awaiting it will complete when all the server tasks are
    /// completed.
    pub(super) fn task_handle(&mut self) -> &mut AbortOnDropHandle<()> {
        &mut self.http_server_task
    }

    /// Returns the local address of this server.
    pub(super) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// A handle for the [`Server`].
///
/// This does not allow access to the task but can communicate with it.
#[derive(Debug, Clone)]
pub(super) struct ServerHandle {
    cancel_token: CancellationToken,
}

impl ServerHandle {
    /// Gracefully shut down the server.
    pub(super) fn shutdown(&self) {
        self.cancel_token.cancel()
    }
}

/// Configuration to use for the TLS connection
#[derive(Debug, Clone)]
pub(super) struct TlsConfig {
    /// The server config
    pub(super) config: Arc<rustls::ServerConfig>,
    /// The kind
    pub(super) acceptor: TlsAcceptor,
}

/// Builder for the Relay HTTP Server.
///
/// Defaults to handling relay requests on the "/relay" (and "/derp" for backwards compatibility) endpoint.
/// Other HTTP endpoints can be added using [`ServerBuilder::request_handler`].
#[derive(derive_more::Debug)]
pub(super) struct ServerBuilder {
    /// The ip + port combination for this server.
    addr: SocketAddr,
    /// Optional tls configuration/TlsAcceptor combination.
    ///
    /// When `None`, the server will serve HTTP, otherwise it will serve HTTPS.
    tls_config: Option<TlsConfig>,
    /// A map of request handlers to routes.
    ///
    /// Used when certain routes in your server should be made available at the same port as
    /// the relay server, and so must be handled along side requests to the relay endpoint.
    handlers: Handlers,
    /// Headers to use for HTTP responses.
    headers: HeaderMap,
    /// Rate-limiting configuration for an individual client connection.
    ///
    /// Rate-limiting is enforced on received traffic from individual clients.  This
    /// configuration applies to a single client connection.
    client_rx_ratelimit: Option<ClientConnRateLimit>,
    /// The capacity of the key cache.
    key_cache_capacity: usize,
}

impl ServerBuilder {
    /// Creates a new [ServerBuilder].
    pub(super) fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            tls_config: None,
            handlers: Default::default(),
            headers: HeaderMap::new(),
            client_rx_ratelimit: None,
            key_cache_capacity: DEFAULT_KEY_CACHE_CAPACITY,
        }
    }

    /// Serves all requests content using TLS.
    pub(super) fn tls_config(mut self, config: Option<TlsConfig>) -> Self {
        self.tls_config = config;
        self
    }

    /// Sets the per-client rate-limit configuration for incoming data.
    ///
    /// On each client connection the incoming data is rate-limited.  By default
    /// no rate limit is enforced.
    pub(super) fn client_rx_ratelimit(mut self, config: ClientConnRateLimit) -> Self {
        self.client_rx_ratelimit = Some(config);
        self
    }

    /// Adds a custom handler for a specific Method & URI.
    pub(super) fn request_handler(
        mut self,
        method: Method,
        uri_path: &'static str,
        handler: HyperHandler,
    ) -> Self {
        self.handlers.insert((method, uri_path), handler);
        self
    }

    /// Adds HTTP headers to responses.
    pub(super) fn headers(mut self, headers: HeaderMap) -> Self {
        for (k, v) in headers.iter() {
            self.headers.insert(k.clone(), v.clone());
        }
        self
    }

    /// Set the capacity of the cache for public keys.
    pub fn key_cache_capacity(mut self, capacity: usize) -> Self {
        self.key_cache_capacity = capacity;
        self
    }

    /// Builds and spawns an HTTP(S) Relay Server.
    #[allow(clippy::unused_async)]
    pub(super) async fn spawn(self) -> Result<Server> {
        let server_task = ServerActorTask::spawn();
        let service = RelayService::new(
            self.handlers,
            self.headers,
            server_task.server_channel.clone(),
            server_task.write_timeout,
            self.client_rx_ratelimit,
            KeyCache::new(self.key_cache_capacity),
        );

        let tls_config = self.tls_config;

        let addr = self.addr;

        // Bind a TCP listener on `addr` and handles content using HTTPS.
        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        socket.bind(addr)?;
        let listener = socket
            .listen(2048)
            .with_context(|| format!("failed to bind server socket to {addr}"))?;

        // let listener = TcpListener::bind(&addr)
        //     .await
        //     .with_context(|| format!("failed to bind server socket to {addr}"))?;

        // we will use this cancel token to stop the infinite loop in the `listener.accept() task`
        let cancel_server_loop = CancellationToken::new();

        let addr = listener.local_addr()?;
        let http_str = tls_config.as_ref().map_or("HTTP/WS", |_| "HTTPS/WSS");
        info!("[{http_str}] relay: serving on {addr}");

        let cancel = cancel_server_loop.clone();
        let task = tokio::task::spawn(
            async move {
                // create a join set to track all our connection tasks
                let mut set = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            break;
                        }
                        Some(res) = set.join_next() => {
                            if let Err(err) = res {
                                if err.is_panic() {
                                    panic!("task panicked: {:#?}", err);
                                }
                            }
                        }
                        res = listener.accept() => match res {
                            Ok((stream, peer_addr)) => {
                                debug!("connection opened from {peer_addr}");
                                let tls_config = tls_config.clone();
                                let service = service.clone();
                                // spawn a task to handle the connection
                                set.spawn(async move {
                                    service
                                        .handle_connection(stream, tls_config)
                                        .await
                                }.instrument(info_span!("conn", peer = %peer_addr)));
                            }
                            Err(err) => {
                                error!("failed to accept connection: {err}");
                            }
                        }
                    }
                }
                // TODO: if the task this is running in is aborted this server is not shut
                // down.
                server_task.close().await;
                set.shutdown().await;
                debug!("server has been shutdown.");
            }
            .instrument(info_span!("relay-http-serve")),
        );

        Ok(Server {
            addr,
            http_server_task: AbortOnDropHandle::new(task),
            cancel_server_loop,
        })
    }
}

/// The hyper Service that serves the actual relay endpoints.
#[derive(Clone, Debug)]
struct RelayService(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    handlers: Handlers,
    headers: HeaderMap,
    server_channel: mpsc::Sender<Message>,
    write_timeout: Duration,
    rate_limit: Option<ClientConnRateLimit>,
    key_cache: KeyCache,
}

impl RelayService {
    /// Upgrades the HTTP connection to the relay protocol, runs relay client.
    fn call_client_conn(
        &self,
        mut req: Request<Incoming>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<BytesBody>, hyper::Error>> + Send>> {
        // TODO: soooo much cloning. See if there is an alternative
        let this = self.clone();
        let mut builder = Response::builder();
        for (key, value) in self.0.headers.iter() {
            builder = builder.header(key, value);
        }

        async move {
            {
                // Send a 400 to any request that doesn't have an `Upgrade` header.
                let Some(protocol) = req.headers().get(UPGRADE).and_then(Protocol::parse_header)
                else {
                    return Ok(builder
                        .status(StatusCode::BAD_REQUEST)
                        .body(body_empty())
                        .expect("valid body"));
                };

                let websocket_headers = if protocol == Protocol::Websocket {
                    let Some(key) = req.headers().get("Sec-WebSocket-Key").cloned() else {
                        warn!("missing header Sec-WebSocket-Key for websocket relay protocol");
                        return Ok(builder
                            .status(StatusCode::BAD_REQUEST)
                            .body(body_empty())
                            .expect("valid body"));
                    };

                    let Some(version) = req.headers().get("Sec-WebSocket-Version").cloned() else {
                        warn!("missing header Sec-WebSocket-Version for websocket relay protocol");
                        return Ok(builder
                            .status(StatusCode::BAD_REQUEST)
                            .body(body_empty())
                            .expect("valid body"));
                    };

                    if version.as_bytes() != SUPPORTED_WEBSOCKET_VERSION.as_bytes() {
                        warn!("invalid header Sec-WebSocket-Version: {:?}", version);
                        return Ok(builder
                            .status(StatusCode::BAD_REQUEST)
                            // It's convention to send back the version(s) we *do* support
                            .header("Sec-WebSocket-Version", SUPPORTED_WEBSOCKET_VERSION)
                            .body(body_empty())
                            .expect("valid body"));
                    }

                    Some((key, version))
                } else {
                    None
                };

                debug!(?protocol, "upgrading connection");

                // Setup a future that will eventually receive the upgraded
                // connection and talk a new protocol, and spawn the future
                // into the runtime.
                //
                // Note: This can't possibly be fulfilled until the 101 response
                // is returned below, so it's better to spawn this future instead
                // waiting for it to complete to then return a response.
                tokio::task::spawn(
                    async move {
                        match hyper::upgrade::on(&mut req).await {
                            Ok(upgraded) => {
                                if let Err(err) =
                                    this.0.relay_connection_handler(protocol, upgraded).await
                                {
                                    warn!(
                                        ?protocol,
                                        "error accepting upgraded connection: {err:#}",
                                    );
                                } else {
                                    debug!(?protocol, "upgraded connection completed");
                                };
                            }
                            Err(err) => warn!("upgrade error: {err:#}"),
                        }
                    }
                    .instrument(debug_span!("handler")),
                );

                // Now return a 101 Response saying we agree to the upgrade to the
                // HTTP_UPGRADE_PROTOCOL
                builder = builder
                    .status(StatusCode::SWITCHING_PROTOCOLS)
                    .header(UPGRADE, HeaderValue::from_static(protocol.upgrade_header()));

                if let Some((key, _version)) = websocket_headers {
                    Ok(builder
                        .header("Sec-WebSocket-Accept", &derive_accept_key(key.as_bytes()))
                        .header(CONNECTION, "upgrade")
                        .body(body_full("switching to websocket protocol"))
                        .expect("valid body"))
                } else {
                    Ok(builder.body(body_empty()).expect("valid body"))
                }
            }
        }
        .boxed()
    }
}

impl Service<Request<Incoming>> for RelayService {
    type Response = Response<BytesBody>;
    type Error = HyperError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        // Create a client if the request hits the relay endpoint.
        if matches!(
            (req.method(), req.uri().path()),
            (&hyper::Method::GET, LEGACY_RELAY_PATH | RELAY_PATH)
        ) {
            let this = self.clone();
            return Box::pin(async move { this.call_client_conn(req).await.map_err(Into::into) });
        }
        // Otherwise handle the relay connection as normal.
        // Check all other possible endpoints.
        let uri = req.uri().clone();
        if let Some(res) = self.0.handlers.get(&(req.method().clone(), uri.path())) {
            let f = res(req, self.0.default_response());
            return Box::pin(async move { f });
        }
        // Otherwise return 404
        let res = self.0.not_found_fn(req, self.0.default_response());
        Box::pin(async move { res })
    }
}

impl Inner {
    fn default_response(&self) -> ResponseBuilder {
        let mut response = Response::builder();
        for (key, value) in self.headers.iter() {
            response = response.header(key.clone(), value.clone());
        }
        response
    }

    fn not_found_fn(
        &self,
        _req: Request<Incoming>,
        mut res: ResponseBuilder,
    ) -> HyperResult<Response<BytesBody>> {
        for (k, v) in self.headers.iter() {
            res = res.header(k.clone(), v.clone());
        }
        let body = body_full("Not Found");
        let r = res.status(StatusCode::NOT_FOUND).body(body)?;
        HyperResult::Ok(r)
    }

    /// The server HTTP handler to do HTTP upgrades.
    ///
    /// This handler runs while doing the connection upgrade handshake.  Once the connection
    /// is upgraded it sends the stream to the relay server which takes it over.  After
    /// having sent off the connection this handler returns.
    async fn relay_connection_handler(&self, protocol: Protocol, upgraded: Upgraded) -> Result<()> {
        debug!(?protocol, "relay_connection upgraded");
        let (io, read_buf) = downcast_upgrade(upgraded)?;
        ensure!(
            read_buf.is_empty(),
            "can not deal with buffered data yet: {:?}",
            read_buf
        );

        self.accept(protocol, io).await
    }

    /// Adds a new connection to the server and serves it.
    ///
    /// Will error if it takes too long (10 sec) to write or read to the connection, if there is
    /// some read or write error to the connection,  if the server is meant to verify clients,
    /// and is unable to verify this one, or if there is some issue communicating with the server.
    ///
    /// The provided [`AsyncRead`] and [`AsyncWrite`] must be already connected to the connection.
    ///
    /// [`AsyncRead`]: tokio::io::AsyncRead
    /// [`AsyncWrite`]: tokio::io::AsyncWrite
    async fn accept(&self, protocol: Protocol, io: MaybeTlsStream) -> Result<()> {
        trace!(?protocol, "accept: start");
        let mut io = match protocol {
            Protocol::Relay => {
                inc!(Metrics, derp_accepts);
                RelayedStream::Derp(Framed::new(io, RelayCodec::new(self.key_cache.clone())))
            }
            Protocol::Websocket => {
                inc!(Metrics, websocket_accepts);
                RelayedStream::Ws(
                    WebSocketStream::from_raw_socket(io, Role::Server, None).await,
                    self.key_cache.clone(),
                )
            }
        };
        trace!("accept: recv client key");
        let (client_key, info) = recv_client_key(&mut io)
            .await
            .context("unable to receive client information")?;

        if info.version != PROTOCOL_VERSION {
            bail!(
                "unexpected client version {}, expected {}",
                info.version,
                PROTOCOL_VERSION
            );
        }

        trace!("accept: build client conn");
        let client_conn_builder = ClientConnConfig {
            node_id: client_key,
            stream: io,
            write_timeout: self.write_timeout,
            channel_capacity: PER_CLIENT_SEND_QUEUE_DEPTH,
            rate_limit: self.rate_limit,
            server_channel: self.server_channel.clone(),
        };
        trace!("accept: create client");
        self.server_channel
            .send(Message::CreateClient(client_conn_builder))
            .await
            .map_err(|_| {
                anyhow::anyhow!("server channel closed, the server is probably shutdown")
            })?;
        Ok(())
    }
}

/// TLS Certificate Authority acceptor.
#[derive(Clone, derive_more::Debug)]
pub(super) enum TlsAcceptor {
    /// Uses Let's Encrypt as the Certificate Authority. This is used in production.
    LetsEncrypt(#[debug("tokio_rustls_acme::AcmeAcceptor")] AcmeAcceptor),
    /// Manually added tls acceptor. Generally used for tests or for when we've passed in
    /// a certificate via a file.
    Manual(#[debug("tokio_rustls::TlsAcceptor")] tokio_rustls::TlsAcceptor),
}

impl RelayService {
    fn new(
        handlers: Handlers,
        headers: HeaderMap,
        server_channel: mpsc::Sender<Message>,
        write_timeout: Duration,
        rate_limit: Option<ClientConnRateLimit>,
        key_cache: KeyCache,
    ) -> Self {
        Self(Arc::new(Inner {
            handlers,
            headers,
            server_channel,
            write_timeout,
            rate_limit,
            key_cache,
        }))
    }

    /// Handle the incoming connection.
    ///
    /// If a `tls_config` is given, will serve the connection using HTTPS.
    async fn handle_connection(self, stream: TcpStream, tls_config: Option<TlsConfig>) {
        let res = match tls_config {
            Some(tls_config) => {
                debug!("HTTPS: serve connection");
                self.tls_serve_connection(stream, tls_config).await
            }
            None => {
                debug!("HTTP: serve connection");
                self.serve_connection(MaybeTlsStream::Plain(stream)).await
            }
        };
        match res {
            Ok(()) => {}
            Err(error) => match error.downcast_ref::<std::io::Error>() {
                Some(io_error) if io_error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!(reason=?error, "peer disconnected");
                }
                _ => {
                    error!(?error, "failed to handle connection");
                }
            },
        }
    }

    /// Serve the tls connection
    async fn tls_serve_connection(self, stream: TcpStream, tls_config: TlsConfig) -> Result<()> {
        let TlsConfig { acceptor, config } = tls_config;
        match acceptor {
            TlsAcceptor::LetsEncrypt(a) => match a.accept(stream).await? {
                None => {
                    info!("TLS[acme]: received TLS-ALPN-01 validation request");
                }
                Some(start_handshake) => {
                    debug!("TLS[acme]: start handshake");
                    let tls_stream = start_handshake
                        .into_stream(config)
                        .await
                        .context("TLS[acme] handshake")?;
                    self.serve_connection(MaybeTlsStream::Tls(tls_stream))
                        .await
                        .context("TLS[acme] serve connection")?;
                }
            },
            TlsAcceptor::Manual(a) => {
                debug!("TLS[manual]: accept");
                let tls_stream = a.accept(stream).await.context("TLS[manual] accept")?;
                self.serve_connection(MaybeTlsStream::Tls(tls_stream))
                    .await
                    .context("TLS[manual] serve connection")?;
            }
        }
        Ok(())
    }

    /// Wrapper for the actual http connection (with upgrades)
    async fn serve_connection<I>(self, io: I) -> Result<()>
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
    {
        hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(io), self)
            .with_upgrades()
            .await?;
        Ok(())
    }
}

#[derive(Default)]
struct Handlers(HashMap<(Method, &'static str), HyperHandler>);

impl std::fmt::Debug for Handlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0.keys().fold(String::new(), |curr, next| {
            let (method, uri) = next;
            format!("{curr}\n({method},{uri}): Box<Fn(ResponseBuilder) -> Result<Response<Body>> + Send + Sync + 'static>")
        });
        write!(f, "HashMap<{s}>")
    }
}

impl std::ops::Deref for Handlers {
    type Target = HashMap<(Method, &'static str), HyperHandler>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Handlers {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use bytes::Bytes;
    use iroh_base::{PublicKey, SecretKey};
    use reqwest::Url;
    use tokio::{sync::mpsc, task::JoinHandle};
    use tokio_util::codec::{FramedRead, FramedWrite};
    use tracing::{info, info_span, Instrument};
    use tracing_subscriber::{prelude::*, EnvFilter};

    use super::*;
    use crate::client::{
        conn::{ConnBuilder, ConnReader, ConnWriter, ReceivedMessage},
        streams::{MaybeTlsStreamReader, MaybeTlsStreamWriter},
        Client, ClientBuilder,
    };

    pub(crate) fn make_tls_config() -> TlsConfig {
        let subject_alt_names = vec!["localhost".to_string()];

        let cert = rcgen::generate_simple_self_signed(subject_alt_names).unwrap();
        let rustls_certificate = cert.cert.der().clone();
        let rustls_key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocols supported by ring")
        .with_no_client_auth()
        .with_single_cert(vec![(rustls_certificate)], rustls_key.into())
        .expect("cert is right");

        let config = Arc::new(config);
        let acceptor = tokio_rustls::TlsAcceptor::from(config.clone());

        TlsConfig {
            config,
            acceptor: TlsAcceptor::Manual(acceptor),
        }
    }

    #[tokio::test]
    async fn test_http_clients_and_server() -> Result<()> {
        let _guard = iroh_test::logging::setup();

        let a_key = SecretKey::generate(rand::thread_rng());
        let b_key = SecretKey::generate(rand::thread_rng());

        // start server
        let server = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
            .spawn()
            .await?;

        let addr = server.addr();

        // get dial info
        let port = addr.port();
        let addr = {
            if let std::net::IpAddr::V4(ipv4_addr) = addr.ip() {
                ipv4_addr
            } else {
                anyhow::bail!("cannot get ipv4 addr from socket addr {addr:?}");
            }
        };
        info!("addr: {addr}:{port}");
        let relay_addr: Url = format!("http://{addr}:{port}").parse().unwrap();

        // create clients
        let (a_key, mut a_recv, client_a_task, client_a) = {
            let span = info_span!("client-a");
            let _guard = span.enter();
            create_test_client(a_key, relay_addr.clone())
        };
        info!("created client {a_key:?}");
        let (b_key, mut b_recv, client_b_task, client_b) = {
            let span = info_span!("client-b");
            let _guard = span.enter();
            create_test_client(b_key, relay_addr)
        };
        info!("created client {b_key:?}");

        info!("ping a");
        client_a.ping().await?;

        info!("ping b");
        client_b.ping().await?;

        info!("sending message from a to b");
        let msg = Bytes::from_static(b"hi there, client b!");
        client_a.send(b_key, msg.clone()).await?;
        info!("waiting for message from a on b");
        let (got_key, got_msg) = b_recv.recv().await.expect("expected message from client_a");
        assert_eq!(a_key, got_key);
        assert_eq!(msg, got_msg);

        info!("sending message from b to a");
        let msg = Bytes::from_static(b"right back at ya, client b!");
        client_b.send(a_key, msg.clone()).await?;
        info!("waiting for message b on a");
        let (got_key, got_msg) = a_recv.recv().await.expect("expected message from client_b");
        assert_eq!(b_key, got_key);
        assert_eq!(msg, got_msg);

        client_a.close().await?;
        client_a_task.abort();
        client_b.close().await?;
        client_b_task.abort();
        server.shutdown();

        Ok(())
    }

    fn create_test_client(
        key: SecretKey,
        server_url: Url,
    ) -> (
        PublicKey,
        mpsc::Receiver<(PublicKey, Bytes)>,
        JoinHandle<()>,
        Client,
    ) {
        let client = ClientBuilder::new(server_url).insecure_skip_cert_verify(true);
        let dns_resolver = crate::dns::default_resolver();
        let (client, mut client_reader) = client.build(key.clone(), dns_resolver.clone());
        let public_key = key.public();
        let (received_msg_s, received_msg_r) = tokio::sync::mpsc::channel(10);
        let client_reader_task = tokio::spawn(
            async move {
                loop {
                    info!("waiting for message on {:?}", key.public());
                    match client_reader.recv().await {
                        None => {
                            info!("client received nothing");
                            return;
                        }
                        Some(Err(e)) => {
                            info!("client {:?} `recv` error {e}", key.public());
                            return;
                        }
                        Some(Ok(msg)) => {
                            info!("got message on {:?}: {msg:?}", key.public());
                            if let ReceivedMessage::ReceivedPacket {
                                remote_node_id: source,
                                data,
                            } = msg
                            {
                                received_msg_s
                                    .send((source, data))
                                    .await
                                    .unwrap_or_else(|err| {
                                        panic!(
                                            "client {:?}, error sending message over channel: {:?}",
                                            key.public(),
                                            err
                                        )
                                    });
                            }
                        }
                    }
                }
            }
            .instrument(info_span!("test-client-reader")),
        );
        (public_key, received_msg_r, client_reader_task, client)
    }

    #[tokio::test]
    async fn test_https_clients_and_server() -> Result<()> {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(EnvFilter::from_default_env())
            .try_init()
            .ok();

        let a_key = SecretKey::generate(rand::thread_rng());
        let b_key = SecretKey::generate(rand::thread_rng());

        // create tls_config
        let tls_config = make_tls_config();

        // start server
        let mut server = ServerBuilder::new("127.0.0.1:0".parse().unwrap())
            .tls_config(Some(tls_config))
            .spawn()
            .await?;

        let addr = server.addr();

        // get dial info
        let port = addr.port();
        let addr = {
            if let std::net::IpAddr::V4(ipv4_addr) = addr.ip() {
                ipv4_addr
            } else {
                anyhow::bail!("cannot get ipv4 addr from socket addr {addr:?}");
            }
        };
        info!("Relay listening on: {addr}:{port}");

        let url: Url = format!("https://localhost:{port}").parse().unwrap();

        // create clients
        let (a_key, mut a_recv, client_a_task, client_a) = create_test_client(a_key, url.clone());
        info!("created client {a_key:?}");
        let (b_key, mut b_recv, client_b_task, client_b) = create_test_client(b_key, url);
        info!("created client {b_key:?}");

        client_a.ping().await?;
        client_b.ping().await?;

        info!("sending message from a to b");
        let msg = Bytes::from_static(b"hi there, client b!");
        client_a.send(b_key, msg.clone()).await?;
        info!("waiting for message from a on b");
        let (got_key, got_msg) = b_recv.recv().await.expect("expected message from client_a");
        assert_eq!(a_key, got_key);
        assert_eq!(msg, got_msg);

        info!("sending message from b to a");
        let msg = Bytes::from_static(b"right back at ya, client b!");
        client_b.send(a_key, msg.clone()).await?;
        info!("waiting for message b on a");
        let (got_key, got_msg) = a_recv.recv().await.expect("expected message from client_b");
        assert_eq!(b_key, got_key);
        assert_eq!(msg, got_msg);

        server.shutdown();
        server.task_handle().await?;
        client_a.close().await?;
        client_a_task.abort();
        client_b.close().await?;
        client_b_task.abort();
        Ok(())
    }

    fn make_test_client(secret_key: SecretKey) -> (tokio::io::DuplexStream, ConnBuilder) {
        let (client, server) = tokio::io::duplex(10);
        let (client_reader, client_writer) = tokio::io::split(client);

        let client_reader = MaybeTlsStreamReader::Mem(client_reader);
        let client_writer = MaybeTlsStreamWriter::Mem(client_writer);

        let client_reader = ConnReader::Derp(FramedRead::new(client_reader, RelayCodec::test()));
        let client_writer = ConnWriter::Derp(FramedWrite::new(client_writer, RelayCodec::test()));

        (
            server,
            ConnBuilder::new(secret_key, None, client_reader, client_writer),
        )
    }

    #[tokio::test]
    async fn test_server_basic() -> Result<()> {
        let _guard = iroh_test::logging::setup();

        // create the server!
        let server_task: ServerActorTask = ServerActorTask::spawn();
        let service = RelayService::new(
            Default::default(),
            Default::default(),
            server_task.server_channel.clone(),
            server_task.write_timeout,
            None,
            KeyCache::test(),
        );

        // create client a and connect it to the server
        let key_a = SecretKey::generate(rand::thread_rng());
        let public_key_a = key_a.public();
        let (rw_a, client_a_builder) = make_test_client(key_a);
        let s = service.clone();
        let handler_task = tokio::spawn(async move {
            s.0.accept(Protocol::Relay, MaybeTlsStream::Test(rw_a))
                .await
        });
        let (client_a, mut client_receiver_a) = client_a_builder.build().await?;
        handler_task.await??;

        // create client b and connect it to the server
        let key_b = SecretKey::generate(rand::thread_rng());
        let public_key_b = key_b.public();
        let (rw_b, client_b_builder) = make_test_client(key_b);
        let s = service.clone();
        let handler_task = tokio::spawn(async move {
            s.0.accept(Protocol::Relay, MaybeTlsStream::Test(rw_b))
                .await
        });
        let (client_b, mut client_receiver_b) = client_b_builder.build().await?;
        handler_task.await??;

        // send message from a to b!
        let msg = Bytes::from_static(b"hello client b!!");
        client_a.send(public_key_b, msg.clone()).await?;
        match client_receiver_b.recv().await? {
            ReceivedMessage::ReceivedPacket {
                remote_node_id,
                data,
            } => {
                assert_eq!(public_key_a, remote_node_id);
                assert_eq!(&msg[..], data);
            }
            msg => {
                anyhow::bail!("expected ReceivedPacket msg, got {msg:?}");
            }
        }

        // send message from b to a!
        let msg = Bytes::from_static(b"nice to meet you client a!!");
        client_b.send(public_key_a, msg.clone()).await?;
        match client_receiver_a.recv().await? {
            ReceivedMessage::ReceivedPacket {
                remote_node_id,
                data,
            } => {
                assert_eq!(public_key_b, remote_node_id);
                assert_eq!(&msg[..], data);
            }
            msg => {
                anyhow::bail!("expected ReceivedPacket msg, got {msg:?}");
            }
        }

        // close the server and clients
        server_task.close().await;

        // client connections have been shutdown
        let res = client_a
            .send(public_key_b, Bytes::from_static(b"try to send"))
            .await;
        assert!(res.is_err());
        assert!(client_receiver_b.recv().await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_server_replace_client() -> Result<()> {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(EnvFilter::from_default_env())
            .try_init()
            .ok();

        // create the server!
        let server_task: ServerActorTask = ServerActorTask::spawn();
        let service = RelayService::new(
            Default::default(),
            Default::default(),
            server_task.server_channel.clone(),
            server_task.write_timeout,
            None,
            KeyCache::test(),
        );

        // create client a and connect it to the server
        let key_a = SecretKey::generate(rand::thread_rng());
        let public_key_a = key_a.public();
        let (rw_a, client_a_builder) = make_test_client(key_a);
        let s = service.clone();
        let handler_task = tokio::spawn(async move {
            s.0.accept(Protocol::Relay, MaybeTlsStream::Test(rw_a))
                .await
        });
        let (client_a, mut client_receiver_a) = client_a_builder.build().await?;
        handler_task.await??;

        // create client b and connect it to the server
        let key_b = SecretKey::generate(rand::thread_rng());
        let public_key_b = key_b.public();
        let (rw_b, client_b_builder) = make_test_client(key_b.clone());
        let s = service.clone();
        let handler_task = tokio::spawn(async move {
            s.0.accept(Protocol::Relay, MaybeTlsStream::Test(rw_b))
                .await
        });
        let (client_b, mut client_receiver_b) = client_b_builder.build().await?;
        handler_task.await??;

        // send message from a to b!
        let msg = Bytes::from_static(b"hello client b!!");
        client_a.send(public_key_b, msg.clone()).await?;
        match client_receiver_b.recv().await? {
            ReceivedMessage::ReceivedPacket {
                remote_node_id,
                data,
            } => {
                assert_eq!(public_key_a, remote_node_id);
                assert_eq!(&msg[..], data);
            }
            msg => {
                anyhow::bail!("expected ReceivedPacket msg, got {msg:?}");
            }
        }

        // send message from b to a!
        let msg = Bytes::from_static(b"nice to meet you client a!!");
        client_b.send(public_key_a, msg.clone()).await?;
        match client_receiver_a.recv().await? {
            ReceivedMessage::ReceivedPacket {
                remote_node_id,
                data,
            } => {
                assert_eq!(public_key_b, remote_node_id);
                assert_eq!(&msg[..], data);
            }
            msg => {
                anyhow::bail!("expected ReceivedPacket msg, got {msg:?}");
            }
        }

        // create client b and connect it to the server
        let (new_rw_b, new_client_b_builder) = make_test_client(key_b);
        let s = service.clone();
        let handler_task = tokio::spawn(async move {
            s.0.accept(Protocol::Relay, MaybeTlsStream::Test(new_rw_b))
                .await
        });
        let (new_client_b, mut new_client_receiver_b) = new_client_b_builder.build().await?;
        handler_task.await??;

        // assert!(client_b.recv().await.is_err());

        // send message from a to b!
        let msg = Bytes::from_static(b"are you still there, b?!");
        client_a.send(public_key_b, msg.clone()).await?;
        match new_client_receiver_b.recv().await? {
            ReceivedMessage::ReceivedPacket {
                remote_node_id,
                data,
            } => {
                assert_eq!(public_key_a, remote_node_id);
                assert_eq!(&msg[..], data);
            }
            msg => {
                anyhow::bail!("expected ReceivedPacket msg, got {msg:?}");
            }
        }

        // send message from b to a!
        let msg = Bytes::from_static(b"just had a spot of trouble but I'm back now,a!!");
        new_client_b.send(public_key_a, msg.clone()).await?;
        match client_receiver_a.recv().await? {
            ReceivedMessage::ReceivedPacket {
                remote_node_id,
                data,
            } => {
                assert_eq!(public_key_b, remote_node_id);
                assert_eq!(&msg[..], data);
            }
            msg => {
                anyhow::bail!("expected ReceivedPacket msg, got {msg:?}");
            }
        }

        // close the server and clients
        server_task.close().await;

        // client connections have been shutdown
        let res = client_a
            .send(public_key_b, Bytes::from_static(b"try to send"))
            .await;
        assert!(res.is_err());
        assert!(new_client_receiver_b.recv().await.is_err());
        Ok(())
    }
}
