//! Correlated asynchronous responses and command submission for order requests.

use crate::auth::{Credentials, StandXSigner};
use crate::client::order::{cancel_order_body, create_order_body, CreateOrderParams};
use crate::endpoints::StandXEndpoints;
use crate::error::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const ORDER_RESPONSE_ROTATE_AFTER: Duration = Duration::from_secs(23 * 60 * 60 + 50 * 60);
const ORDER_RESPONSE_PING_INTERVAL: Duration = Duration::from_secs(30);
/// A healthy server sends a ping about every 10 seconds; a longer silent
/// interval means the connection may be half-open and cannot be trusted.
const ORDER_RESPONSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const ORDER_COMMAND_QUEUE_CAPACITY: usize = 256;

/// Asynchronous acceptance or rejection for an order request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderResponse {
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl OrderResponse {
    pub fn accepted(&self) -> bool {
        self.code == 0
    }

    /// The venue has finished processing the request and reports success.
    ///
    /// The exchange sends a gateway-ack with `"message": "accepted"` for some
    /// commands (see `accepted()`); this predicate requires the stronger
    /// `"message": "success"` response that indicates the action is complete.
    pub fn is_success(&self) -> bool {
        self.code == 0 && self.message == "success"
    }
}

/// WebSocket stream paired with the `x-session-id` used by HTTP order calls.
pub struct OrderResponseStream {
    url: String,
    token: String,
    signer: Option<Arc<StandXSigner>>,
    session_id: String,
    verbose: bool,
    ping_interval: Duration,
    idle_timeout: Duration,
    rotate_after: Duration,
}

/// Sender for authenticated `order:new` and `order:cancel` WebSocket commands.
///
/// A successful call means the complete frame was written to the local
/// WebSocket sink. It does *not* mean the venue accepted the order; callers
/// must still correlate the returned request ID with [`OrderResponse`] and
/// account-order events.
#[derive(Clone, Debug)]
pub struct OrderCommandSender {
    session_id: String,
    signer: Option<Arc<StandXSigner>>,
    commands: mpsc::Sender<OutboundOrderCommand>,
}

struct OutboundOrderCommand {
    text: String,
    written: oneshot::Sender<Result<()>>,
}

/// A signed order command whose response correlation ID is available before
/// any asynchronous socket write begins.
///
/// Callers that maintain an order ledger should register [`Self::request_id`]
/// before passing the command to [`OrderCommandSender::send_prepared`]. The
/// wire payload stays private so exchange signing and envelope construction
/// remain owned by the SDK.
pub struct PreparedOrderCommand {
    request_id: String,
    text: String,
}

impl PreparedOrderCommand {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

impl fmt::Debug for PreparedOrderCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedOrderCommand")
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl OrderCommandSender {
    /// Submit a signed order creation request over the authenticated socket.
    pub async fn create_order(&self, params: &CreateOrderParams) -> Result<String> {
        let command = self.prepare_create_order(params)?;
        let request_id = command.request_id.clone();
        self.send_prepared(command).await?;
        Ok(request_id)
    }

    /// Submit a signed cancellation request by exchange order ID.
    pub async fn cancel_order(&self, order_id: &str) -> Result<String> {
        let command = self.prepare_cancel_order(order_id)?;
        let request_id = command.request_id.clone();
        self.send_prepared(command).await?;
        Ok(request_id)
    }

    /// Prepare a signed order creation request without performing I/O.
    pub fn prepare_create_order(&self, params: &CreateOrderParams) -> Result<PreparedOrderCommand> {
        self.prepare("order:new", create_order_body(params).to_string())
    }

    /// Prepare a signed cancellation request without performing I/O.
    pub fn prepare_cancel_order(&self, order_id: &str) -> Result<PreparedOrderCommand> {
        let order_id = order_id.parse::<i64>().map_err(|_| Error::Validation {
            field: "order_id".to_string(),
            message: format!("expected an integer order ID, got '{order_id}'"),
        })?;
        self.prepare("order:cancel", cancel_order_body(order_id).to_string())
    }

    fn prepare(&self, method: &str, params: String) -> Result<PreparedOrderCommand> {
        let request_id = uuid::Uuid::new_v4().to_string();
        // The WebSocket envelope request ID is the asynchronous response
        // correlation key. The signature carries its own request ID, just as
        // the HTTP headers do, so the two protocol roles cannot be confused.
        let signer = self.signer.as_ref().ok_or_else(|| Error::AuthRequired {
            message: "order command stream requires an Ed25519 private key".to_string(),
            resolution: "Run 'standx auth login' with --private-key before live trading"
                .to_string(),
        })?;
        let signature = signer.sign_request_now(&params);
        let text = serde_json::json!({
            "session_id": self.session_id,
            "request_id": request_id,
            "method": method,
            "header": {
                "x-request-sign-version": signature.version,
                "x-request-id": signature.request_id,
                "x-request-timestamp": signature.timestamp.to_string(),
                "x-request-signature": signature.signature,
            },
            "params": params,
        })
        .to_string();
        Ok(PreparedOrderCommand { request_id, text })
    }

    /// Write a previously prepared command to the authenticated socket.
    ///
    /// Success means the complete frame reached the local WebSocket sink, not
    /// that the venue accepted the request.
    pub async fn send_prepared(&self, command: PreparedOrderCommand) -> Result<()> {
        let (written_tx, written_rx) = oneshot::channel();
        self.commands
            .send(OutboundOrderCommand {
                text: command.text,
                written: written_tx,
            })
            .await
            .map_err(|_| Error::WebSocket {
                message: "order-command stream is unavailable".to_string(),
            })?;
        written_rx.await.map_err(|_| Error::WebSocket {
            message: "order-command stream stopped before writing request".to_string(),
        })??;
        Ok(())
    }
}

/// Shared liveness state for an authenticated order-response connection.
///
/// The failure reason is written before `healthy` flips to false, so callers
/// that observe an unhealthy stream can include the close code/reason or the
/// underlying WebSocket error in their fail-safe log.
#[derive(Debug, Clone)]
pub struct OrderResponseHealth {
    healthy: Arc<AtomicBool>,
    failure_reason: Arc<Mutex<Option<String>>>,
}

impl Default for OrderResponseHealth {
    fn default() -> Self {
        Self {
            healthy: Arc::new(AtomicBool::new(true)),
            failure_reason: Arc::new(Mutex::new(None)),
        }
    }
}

impl OrderResponseHealth {
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn failure_reason(&self) -> Option<String> {
        self.failure_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone())
    }

    /// Mark the response stream unusable. This is public so the supervised
    /// controlled-disconnect hook can exercise the same production fail-safe.
    pub fn mark_unhealthy(&self, reason: impl Into<String>) {
        if let Ok(mut failure_reason) = self.failure_reason.lock() {
            *failure_reason = Some(reason.into());
        }
        self.healthy.store(false, Ordering::Release);
    }
}

impl OrderResponseStream {
    /// Construct a production stream from the currently-loaded credentials.
    pub fn new(session_id: impl Into<String>) -> Result<Self> {
        Self::from_endpoints(session_id, &StandXEndpoints::default())
    }

    /// Construct a stream for a validated endpoint set.
    pub fn from_endpoints(
        session_id: impl Into<String>,
        endpoints: &StandXEndpoints,
    ) -> Result<Self> {
        let credentials = Credentials::load()?;
        if credentials.is_expired() {
            return Err(Error::AuthRequired {
                message: "Token expired".to_string(),
                resolution: "Run 'standx auth login' or set STANDX_JWT environment variable"
                    .to_string(),
            });
        }

        Ok(Self {
            url: endpoints.order_response_url().to_string(),
            token: credentials.token,
            signer: (!credentials.private_key.is_empty())
                .then(|| StandXSigner::from_base58(&credentials.private_key))
                .transpose()?
                .map(Arc::new),
            session_id: session_id.into(),
            verbose: false,
            ping_interval: ORDER_RESPONSE_PING_INTERVAL,
            idle_timeout: ORDER_RESPONSE_IDLE_TIMEOUT,
            rotate_after: ORDER_RESPONSE_ROTATE_AFTER,
        })
    }

    #[cfg(test)]
    fn with_url_and_token(
        url: impl Into<String>,
        token: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            signer: None,
            session_id: session_id.into(),
            verbose: false,
            ping_interval: ORDER_RESPONSE_PING_INTERVAL,
            idle_timeout: ORDER_RESPONSE_IDLE_TIMEOUT,
            rotate_after: ORDER_RESPONSE_ROTATE_AFTER,
        }
    }

    #[cfg(test)]
    fn with_url_token_and_signer(
        url: impl Into<String>,
        token: impl Into<String>,
        session_id: impl Into<String>,
        signer: StandXSigner,
    ) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            signer: Some(Arc::new(signer)),
            session_id: session_id.into(),
            verbose: false,
            ping_interval: ORDER_RESPONSE_PING_INTERVAL,
            idle_timeout: ORDER_RESPONSE_IDLE_TIMEOUT,
            rotate_after: ORDER_RESPONSE_ROTATE_AFTER,
        }
    }

    #[cfg(test)]
    fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    #[cfg(test)]
    fn with_liveness(
        mut self,
        ping_interval: Duration,
        idle_timeout: Duration,
        rotate_after: Duration,
    ) -> Self {
        self.ping_interval = ping_interval;
        self.idle_timeout = idle_timeout;
        self.rotate_after = rotate_after;
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Print raw post-authentication order responses to stderr.
    ///
    /// Outbound signed commands and authentication payloads are never logged.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Connect, wait for authentication, and return a command sender,
    /// asynchronous responses, shared liveness state, and supervisor handle.
    pub async fn connect(
        &self,
    ) -> Result<(
        OrderCommandSender,
        mpsc::Receiver<OrderResponse>,
        OrderResponseHealth,
        tokio::task::JoinHandle<()>,
    )> {
        let (stream, _) = connect_async(&self.url).await?;
        let (mut write, mut read) = stream.split();
        let auth_request_id = uuid::Uuid::new_v4().to_string();
        let auth = auth_request(&self.session_id, &self.token, &auth_request_id);
        write.send(Message::Text(auth.to_string().into())).await?;

        let auth_response = loop {
            let message = tokio::time::timeout(Duration::from_secs(10), read.next())
                .await
                .map_err(|_| Error::WebSocket {
                    message: "timed out waiting for order-response authentication".to_string(),
                })?
                .ok_or_else(|| Error::WebSocket {
                    message: "order-response stream closed before authentication".to_string(),
                })??;
            match message {
                Message::Text(text) => {
                    let response = serde_json::from_str::<OrderResponse>(&text)?;
                    if response.request_id.as_deref() != Some(auth_request_id.as_str()) {
                        return Err(Error::WebSocket {
                            message: "received an unexpected response before authentication"
                                .to_string(),
                        });
                    }
                    break response;
                }
                Message::Ping(payload) => write.send(Message::Pong(payload)).await?,
                Message::Close(_) => {
                    return Err(Error::WebSocket {
                        message: "order-response stream closed before authentication".to_string(),
                    });
                }
                _ => {}
            }
        };
        if !auth_response.accepted() {
            return Err(Error::AuthRequired {
                message: format!(
                    "order-response authentication rejected: {}",
                    auth_response.message
                ),
                resolution: "Run 'standx auth login' and retry".to_string(),
            });
        }

        let (tx, rx) = mpsc::channel(ORDER_COMMAND_QUEUE_CAPACITY);
        let (command_tx, mut command_rx) = mpsc::channel(ORDER_COMMAND_QUEUE_CAPACITY);
        let commands = OrderCommandSender {
            session_id: self.session_id.clone(),
            signer: self.signer.clone(),
            commands: command_tx,
        };
        let health = OrderResponseHealth::default();
        let task_health = health.clone();
        let verbose = self.verbose;
        let ping_interval = self.ping_interval;
        let idle_timeout = self.idle_timeout;
        let rotate_after = self.rotate_after;
        let handle = tokio::spawn(async move {
            let rotation = tokio::time::sleep(rotate_after);
            tokio::pin!(rotation);
            let mut ping = tokio::time::interval_at(
                tokio::time::Instant::now() + ping_interval,
                ping_interval,
            );
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let idle = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle);
            loop {
                tokio::select! {
                    _ = &mut rotation => {
                        task_health.mark_unhealthy("order-response stream proactive 23h50m rotation");
                        return;
                    }
                    _ = ping.tick() => {
                        if let Err(error) = write.send(Message::Ping(Vec::new().into())).await {
                            task_health.mark_unhealthy(format!(
                                "failed to send order-response ping: {error}"
                            ));
                            return;
                        }
                    }
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            task_health.mark_unhealthy("order-command sender dropped".to_string());
                            return;
                        };
                        match write.send(Message::Text(command.text.into())).await {
                            Ok(()) => {
                                let _ = command.written.send(Ok(()));
                            }
                            Err(error) => {
                                let detail = format!("failed to send order command: {error}");
                                task_health.mark_unhealthy(detail.clone());
                                let _ = command.written.send(Err(Error::WebSocket { message: detail }));
                                return;
                            }
                        }
                    }
                    _ = &mut idle => {
                        task_health.mark_unhealthy(format!(
                            "order-response stream idle for {}s (no ping/pong/data; connection likely half-open)",
                            idle_timeout.as_secs()
                        ));
                        return;
                    }
                    message = read.next() => {
                        // Only inbound traffic proves the peer is alive; command writes do not.
                        idle.as_mut()
                            .reset(tokio::time::Instant::now() + idle_timeout);
                        match message {
                        Some(Ok(Message::Text(text))) => {
                            if verbose {
                                eprintln!("[WebSocket Order Debug] Received: {text}");
                            }
                            let response = match serde_json::from_str::<OrderResponse>(&text) {
                                Ok(response) if response.request_id.is_some() => response,
                                Ok(_) => {
                                    task_health.mark_unhealthy(
                                        "invalid order-response payload: missing request_id"
                                    );
                                    return;
                                }
                                Err(error) => {
                                    task_health.mark_unhealthy(format!(
                                        "invalid order-response payload: {error}"
                                    ));
                                    return;
                                }
                            };
                            if tx.send(response).await.is_err() {
                                task_health.mark_unhealthy("order-response receiver dropped".to_string());
                                return;
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if let Err(error) = write.send(Message::Pong(payload)).await {
                                task_health.mark_unhealthy(format!(
                                    "failed to send order-response pong: {error}"
                                ));
                                return;
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let reason = frame.map_or_else(
                                || "order-response WebSocket closed without a close frame".to_string(),
                                |frame| {
                                    format!(
                                        "order-response WebSocket closed: code={} reason={:?}",
                                        u16::from(frame.code),
                                        frame.reason
                                    )
                                },
                            );
                            task_health.mark_unhealthy(reason);
                            return;
                        }
                        Some(Err(error)) => {
                            task_health.mark_unhealthy(format!("order-response WebSocket error: {error}"));
                            return;
                        }
                        Some(Ok(_)) => {}
                        None => {
                            task_health.mark_unhealthy(
                                "order-response WebSocket ended without a close frame or reported error",
                            );
                            return;
                        }
                        }
                    }
                }
            }
        });

        Ok((commands, rx, health, handle))
    }
}

fn auth_request(session_id: &str, token: &str, request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "request_id": request_id,
        "method": "auth:login",
        "params": serde_json::json!({ "token": token }).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};

    #[test]
    fn auth_request_uses_stable_session_and_unique_request() {
        let first = auth_request("maker-session", "jwt", "request-1");
        let second = auth_request("maker-session", "jwt", "request-2");

        assert_eq!(first["session_id"], "maker-session");
        assert_eq!(first["method"], "auth:login");
        assert_eq!(first["request_id"], "request-1");
        assert_ne!(first["request_id"], second["request_id"]);
        assert!(first["params"].as_str().unwrap().contains("jwt"));
    }

    #[test]
    fn parses_acceptance_and_rejection_responses() {
        let accepted: OrderResponse = serde_json::from_value(serde_json::json!({
            "code": 0,
            "message": "success",
            "request_id": "request-1"
        }))
        .unwrap();
        assert!(accepted.accepted());
        assert!(accepted.is_success());

        let gateway_ack: OrderResponse = serde_json::from_value(serde_json::json!({
            "code": 0,
            "message": "accepted",
            "request_id": "request-1a"
        }))
        .unwrap();
        assert!(gateway_ack.accepted());
        assert!(!gateway_ack.is_success());

        let rejected: OrderResponse = serde_json::from_value(serde_json::json!({
            "code": 400,
            "message": "alo order rejected",
            "request_id": "request-2"
        }))
        .unwrap();
        assert!(!rejected.accepted());
        assert!(!rejected.is_success());
    }

    #[test]
    fn test_constructor_keeps_session_id() {
        let stream = OrderResponseStream::with_url_and_token(
            "ws://localhost.invalid",
            "jwt",
            "maker-session",
        );
        assert_eq!(stream.session_id(), "maker-session");
        assert!(!stream.verbose);
        assert!(stream.with_verbose(true).verbose);
    }

    #[tokio::test]
    async fn authenticated_connection_becomes_unhealthy_after_server_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            // Drop the socket: the client must flip the liveness flag rather
            // than continuing to place orders on a dead response stream.
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session");
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        assert!(health.is_healthy());
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection close should mark the response stream unhealthy");
        handle.await.unwrap();
        assert!(health
            .failure_reason()
            .is_some_and(|reason| reason.contains("order-response WebSocket")));
    }

    #[tokio::test]
    async fn idle_connection_is_marked_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while let Some(Ok(_)) = websocket.next().await {}
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session")
            .with_idle_timeout(Duration::from_millis(200));
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idle timeout should mark the response stream unhealthy");
        handle.await.unwrap();
        assert!(health
            .failure_reason()
            .is_some_and(|reason| reason.contains("idle")));
        server.abort();
    }

    #[tokio::test]
    async fn authenticated_close_preserves_code_and_reason() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "maintenance".into(),
                })))
                .await
                .unwrap();
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session");
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close frame should mark the response stream unhealthy");
        handle.await.unwrap();
        server.await.unwrap();

        let reason = health.failure_reason().unwrap();
        assert!(reason.contains("code=1008"), "{reason}");
        assert!(reason.contains("maintenance"), "{reason}");
    }

    #[tokio::test]
    async fn command_sender_writes_signed_order_and_delivers_correlated_response() {
        use crate::models::{OrderSide, OrderType, TimeInForce};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            let command = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let command: serde_json::Value = serde_json::from_str(&command).unwrap();
            assert_eq!(command["session_id"], "maker-session");
            assert_eq!(command["method"], "order:new");
            assert_eq!(command["header"]["x-request-sign-version"], "v1");
            assert!(command["header"]["x-request-id"].as_str().is_some());
            assert!(command["header"]["x-request-timestamp"].as_str().is_some());
            assert!(command["header"]["x-request-signature"].as_str().is_some());
            let params: serde_json::Value =
                serde_json::from_str(command["params"].as_str().unwrap()).unwrap();
            assert_eq!(params["cl_ord_id"], "sxmk-test");
            assert_eq!(params["time_in_force"], "alo");
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "accepted",
                        "request_id": command["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let private_key = bs58::encode(signing_key.to_bytes()).into_string();
        let signer = StandXSigner::from_base58(&private_key).unwrap();
        let stream =
            OrderResponseStream::with_url_token_and_signer(url, "jwt", "maker-session", signer)
                .with_verbose(true);
        let (commands, mut responses, _health, handle) = stream.connect().await.unwrap();
        let command = commands
            .prepare_create_order(&CreateOrderParams {
                symbol: "BTC-USD".to_string(),
                cl_ord_id: Some("sxmk-test".to_string()),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: "0.001".to_string(),
                price: Some("50000".to_string()),
                time_in_force: Some(TimeInForce::Alo),
                reduce_only: false,
                stop_price: None,
                sl_price: None,
                tp_price: None,
            })
            .unwrap();
        // The correlation key is available before the first await so a caller
        // can register it before cancellable runtime work begins.
        let request_id = command.request_id().to_string();
        commands.send_prepared(command).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), responses.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.request_id.as_deref(), Some(request_id.as_str()));
        assert!(response.accepted());
        server.await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn command_sender_writes_signed_cancel_and_delivers_correlated_rejection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            let command = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let command: serde_json::Value = serde_json::from_str(&command).unwrap();
            assert_eq!(command["session_id"], "maker-session");
            assert_eq!(command["method"], "order:cancel");
            assert_eq!(command["header"]["x-request-sign-version"], "v1");
            assert!(command["header"]["x-request-id"].as_str().is_some());
            assert!(command["header"]["x-request-timestamp"].as_str().is_some());
            assert!(command["header"]["x-request-signature"].as_str().is_some());
            let params: serde_json::Value =
                serde_json::from_str(command["params"].as_str().unwrap()).unwrap();
            assert_eq!(params, serde_json::json!({ "order_id": 42 }));
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 400,
                        "message": "order already closed",
                        "request_id": command["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let private_key = bs58::encode(signing_key.to_bytes()).into_string();
        let signer = StandXSigner::from_base58(&private_key).unwrap();
        let stream =
            OrderResponseStream::with_url_token_and_signer(url, "jwt", "maker-session", signer);
        let (commands, mut responses, _health, handle) = stream.connect().await.unwrap();
        let request_id = commands.cancel_order("42").await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), responses.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.request_id.as_deref(), Some(request_id.as_str()));
        assert!(!response.accepted());
        assert_eq!(response.message, "order already closed");
        server.await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn client_heartbeat_keeps_an_observably_live_connection_healthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (ping_seen_tx, ping_seen_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let frame = websocket.next().await.unwrap().unwrap();
            let Message::Ping(payload) = frame else {
                panic!("expected client heartbeat ping, got {frame:?}");
            };
            websocket.send(Message::Pong(payload)).await.unwrap();
            let _ = ping_seen_tx.send(());
            let _ = finish_rx.await;
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session")
            .with_liveness(
                Duration::from_millis(25),
                Duration::from_secs(1),
                Duration::from_secs(2),
            );
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), ping_seen_rx)
            .await
            .expect("client heartbeat should arrive")
            .unwrap();
        assert!(health.is_healthy());
        let _ = finish_tx.send(());
        handle.abort();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_response_marks_stream_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text("{malformed".into()))
                .await
                .unwrap();
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session");
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("malformed response should stop the stream task")
            .unwrap();
        assert!(!health.is_healthy());
        assert!(health
            .failure_reason()
            .is_some_and(|reason| reason.contains("invalid order-response payload")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_without_request_id_marks_stream_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "uncorrelated",
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session");
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("missing request ID should stop the stream task")
            .unwrap();
        assert!(!health.is_healthy());
        assert!(health
            .failure_reason()
            .is_some_and(|reason| reason.contains("missing request_id")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proactive_rotation_marks_stream_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 0,
                        "message": "authenticated",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while websocket.next().await.is_some() {}
        });

        let stream = OrderResponseStream::with_url_and_token(url, "jwt", "maker-session")
            .with_liveness(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_millis(50),
            );
        let (_commands, _responses, health, handle) = stream.connect().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("proactive rotation should stop the stream task")
            .unwrap();
        assert!(!health.is_healthy());
        assert!(health
            .failure_reason()
            .is_some_and(|reason| reason.contains("proactive 23h50m rotation")));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn authentication_rejection_prevents_connection_start() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "code": 401,
                        "message": "invalid token",
                        "request_id": auth["request_id"],
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let stream = OrderResponseStream::with_url_and_token(url, "bad-jwt", "maker-session");
        let error = stream.connect().await.unwrap_err();
        assert!(matches!(error, Error::AuthRequired { .. }));
        server.await.unwrap();
    }
}
