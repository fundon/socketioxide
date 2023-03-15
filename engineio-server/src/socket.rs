use std::{ops::ControlFlow, time::Duration};

use futures::{stream::SplitSink, FutureExt, SinkExt};
use hyper::upgrade::Upgraded;
use tokio::time::{self, Instant, Timeout};
use tokio_tungstenite::{tungstenite, WebSocketStream};
use tracing::debug;

use crate::{errors::Error, layer::EngineIoHandler, packet::Packet};

#[derive(Debug)]
pub struct Socket {
    sid: i64,
    http_tx: Option<hyper::body::Sender>,
    ws_tx: Option<SplitSink<WebSocketStream<Upgraded>, tungstenite::Message>>,
    ping_timeout: Option<Timeout<()>>,
    last_pong: Instant,
}

impl Socket {
    pub(crate) fn new_http(sid: i64, sender: hyper::body::Sender) -> Self {
        Self {
            sid,
            http_tx: Some(sender),
            ws_tx: None,
            ping_timeout: None,
            last_pong: time::Instant::now(),
        }
    }
    pub(crate) fn new_ws(
        sid: i64,
        sender: SplitSink<WebSocketStream<Upgraded>, tungstenite::Message>,
    ) -> Self {
        let socket = Self {
            sid,
            http_tx: None,
            ws_tx: Some(sender),
            ping_timeout: None,
            last_pong: time::Instant::now(),
        };
        socket
    }

    pub(crate) fn upgrade_from_http(
        &mut self,
        tx: SplitSink<WebSocketStream<Upgraded>, tungstenite::Message>,
    ) {
        self.http_tx = None;
        self.ws_tx = Some(tx);
    }

    pub(crate) fn is_http(&self) -> bool {
        self.http_tx.is_some()
    }
    pub(crate) fn is_ws(&self) -> bool {
        self.ws_tx.is_some()
    }

    pub(crate) async fn handle_packet<H>(
        &mut self,
        packet: Packet,
        handler: &H,
    ) -> ControlFlow<Result<(), Error>, Result<(), Error>>
    where
        H: EngineIoHandler,
    {
        tracing::debug!(
            "Received packet from conn http({}) ws({}): {:?}",
            self.is_http(),
            self.is_ws(),
            packet
        );
        match packet {
            Packet::Open(_) => ControlFlow::Continue(Err(Error::BadPacket(
                "Unexpected Open packet, it should be only used in upgrade process",
            ))),
            Packet::Close => ControlFlow::Break(Ok(())),
            Packet::Ping => ControlFlow::Continue(Err(Error::BadPacket("Unexpected Ping packet"))),
            Packet::Pong => {
                self.last_pong = Instant::now();
                ControlFlow::Continue(Ok(()))
            }
            Packet::Message(msg) => {
                tracing::debug!("Received message: {}", msg);
                match handler.handle::<H>(msg, self).await {
                    Ok(_) => ControlFlow::Continue(Ok(())),
                    Err(e) => ControlFlow::Continue(Err(e)),
                }
            }
            Packet::Upgrade => ControlFlow::Continue(Err(Error::BadPacket(
                "Unexpected Upgrade packet, upgrade from ws connection not supported",
            ))),
            Packet::Noop => ControlFlow::Continue(Err(Error::BadPacket(
                "Unexpected Noop packet, it should be only used in upgrade process",
            ))),
        }
    }

    pub(crate) async fn handle_binary<H>(&mut self, data: Vec<u8>, handler: &H) -> Result<(), Error>
    where
        H: EngineIoHandler,
    {
        handler.handle_binary::<H>(data, self).await
    }

    pub(crate) async fn close(mut self) -> Result<(), Error> {
        if let Some(tx) = self.http_tx {
            self.http_tx = None;
            tx.abort();
        }
        if let Some(mut tx) = self.ws_tx {
            self.ws_tx = None;
            return tx.close().await.map_err(|e| Error::from(e));
        }
        Ok(())
    }

    pub(crate) async fn send(&mut self, packet: Packet) -> Result<(), Error> {
        let msg = packet.try_into().map_err(Error::from)?;
        debug!("Sending packet: {:?}", msg);
        if let Some(tx) = &mut self.http_tx {
            tx.send_data(hyper::body::Bytes::from(msg))
                .await
                .map_err(Error::from)?;
        } else if let Some(tx) = &mut self.ws_tx {
            tx.send(tungstenite::Message::Text(msg))
                .await
                .map_err(Error::from)?;
        }
        Ok(())
    }

    pub(crate) async fn spawn_heartbeat(&mut self, interval: u64, timeout: u64) -> Result<(), Error> {
        // let timeout = self.ping_timeout;
        tokio::time::sleep(Duration::from_millis(interval * 2)).await;
        let mut interval = tokio::time::interval(Duration::from_millis(interval - timeout));
        loop {
            if !self.send_heartbeat(timeout).await? {
                //TODO: handle heartbeat failure
                break;
            }
            interval.tick().await;
        }
        Ok(())
    }

    async fn send_heartbeat(&mut self, timeout: u64) -> Result<bool, Error> {
        let instant = Instant::now();
        self.send(Packet::Ping).await?;
        debug!("Sending ping packet for sid={}, waiting for pong (timeout: {})", self.sid, timeout);
        tokio::time::sleep(Duration::from_millis(timeout)).await;
        Ok(self.last_pong.elapsed().as_millis() > instant.elapsed().as_millis()
            && self.last_pong.elapsed().as_millis() < timeout.into())
    }

    pub async fn emit(&mut self, msg: String) -> Result<(), Error> {
        self.send(Packet::Message(msg)).await
    }

    pub async fn emit_binary(&mut self, data: Vec<u8>) -> Result<(), Error> {
        if let Some(tx) = &mut self.http_tx {
            tx.send_data(hyper::body::Bytes::from(data))
                .await
                .map_err(Error::from)?;
        } else if let Some(tx) = &mut self.ws_tx {
            tx.send(tungstenite::Message::Binary(data))
                .await
                .map_err(Error::from)?;
        }
        Ok(())
    }
}
