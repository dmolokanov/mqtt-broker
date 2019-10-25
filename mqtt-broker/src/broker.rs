use std::collections::HashMap;

use failure::ResultExt;
use mqtt::proto;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::{debug, info, span, warn, Level};
use tracing_futures::Instrument;

use crate::{ClientId, ConnectionHandle, Error, ErrorKind, Event, Message};

macro_rules! try_send {
    ($session:ident, $msg:expr) => {{
        if let Err(e) = $session.send($msg).await {
            warn!(message = "error processing message", %e);
        }
    }};
}

pub struct Session {
    client_id: ClientId,
    handle: ConnectionHandle,
}

impl Session {
    pub fn new(client_id: ClientId, handle: ConnectionHandle) -> Self {
        Self { client_id, handle }
    }

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub async fn send(&mut self, message: Message) -> Result<(), Error> {
        self.handle
            .send(message)
            .await
            .context(ErrorKind::SendConnectionMessage)?;
        Ok(())
    }
}

pub struct Broker {
    sender: Sender<Message>,
    messages: Receiver<Message>,
    sessions: HashMap<ClientId, Session>,
}

impl Broker {
    pub fn new() -> Self {
        let (sender, messages) = mpsc::channel(1024);
        Self {
            sender,
            messages,
            sessions: HashMap::new(),
        }
    }

    pub fn handle(&self) -> BrokerHandle {
        BrokerHandle(self.sender.clone())
    }

    pub async fn run(mut self) -> Result<(), Error> {
        while let Some(message) = self.messages.recv().await {
            let span = span!(Level::INFO, "broker", client_id=%message.client_id());
            self.handle_message(message).instrument(span).await?
        }
        info!("broker task exiting");
        Ok(())
    }

    async fn handle_message(&mut self, message: Message) -> Result<(), Error> {
        let client_id = message.client_id().clone();
        let result = match message.into_event() {
            Event::Connect(connect, handle) => {
                self.handle_connect(client_id, connect, handle).await
            }
            Event::ConnAck(_) => Ok(debug!("broker received CONNACK, ignoring")),
            Event::Disconnect(_) => self.handle_disconnect(client_id).await,
            Event::DropConnection => self.handle_drop_connection(client_id).await,
            Event::CloseSession => self.handle_close_session(client_id).await,
            Event::PingReq(ping) => self.handle_ping_req(client_id, ping).await,
            Event::PingResp(_) => Ok(debug!("broker received PINGRESP, ignoring")),
            Event::Unknown => Ok(debug!("broker received unknown event, ignoring")),
        };

        if let Err(e) = result {
            warn!(message = "error processing message", %e);
        }

        Ok(())
    }

    async fn handle_connect(
        &mut self,
        client_id: ClientId,
        _connect: proto::Connect,
        mut handle: ConnectionHandle,
    ) -> Result<(), Error> {
        debug!("handling connect...");

        let mut new_session = if let Some(mut session) = self.sessions.remove(&client_id) {
            if session.handle == handle {
                // [MQTT-3.1.0-2] - The Server MUST process a second CONNECT Packet
                // sent from a Client as a protocol violation and disconnect the Client.
                //
                // If the handles are equal, this is a second CONNECT packet on the
                // same physical connection. We need to treat this as a protocol
                // violation, move the session to offline, drop the connection, and return.

                // TODO add session state for clean session

                warn!("CONNECT packet received on an already established connection, dropping connection due to protocol violation");
                let message = Message::new(client_id.clone(), Event::DropConnection);
                handle.send(message).await?;
                return Ok(());
            } else {
                // [MQTT-3.1.4-2] If the ClientId represents a Client already connected to the Server
                // then the Server MUST disconnect the existing Client.
                //
                // Send a DropConnection to the current handle.
                // Update the session to use the new handle.

                info!(
                    "connection request for an in use client id ({}). closing previous connection",
                    client_id
                );
                let message = Message::new(client_id.clone(), Event::DropConnection);
                try_send!(session, message);

                session.handle = handle;
                session
            }
        } else {
            // No session present - create a new one.
            debug!("creating new session");
            Session::new(client_id.clone(), handle)
        };

        // TODO validate CONNECT packet
        let ack = proto::ConnAck {
            session_present: false,
            return_code: proto::ConnectReturnCode::Accepted,
        };
        let event = Event::ConnAck(ack);
        let message = Message::new(client_id.clone(), event);
        debug!("sending connack...");

        try_send!(new_session, message);
        self.sessions.insert(client_id.clone(), new_session);
        debug!("connect handled.");
        Ok(())
    }

    async fn handle_disconnect(&mut self, client_id: ClientId) -> Result<(), Error> {
        debug!("handling disconnect...");
        if let Some(mut session) = self.sessions.remove(&client_id) {
            let message = Message::new(client_id.clone(), Event::Disconnect(proto::Disconnect));
            session.send(message).await?;
        } else {
            debug!("no session for {}", client_id);
        }
        debug!("disconnect handled.");
        Ok(())
    }

    async fn handle_drop_connection(&mut self, client_id: ClientId) -> Result<(), Error> {
        debug!("handling drop connection...");
        if let Some(mut session) = self.sessions.remove(&client_id) {
            let message = Message::new(client_id.clone(), Event::DropConnection);
            session.send(message).await?;
        } else {
            debug!("no session for {}", client_id);
        }
        debug!("drop connection handled.");
        Ok(())
    }

    async fn handle_close_session(&mut self, client_id: ClientId) -> Result<(), Error> {
        debug!("handling close session...");
        if self.sessions.remove(&client_id).is_some() {
            debug!("session removed");
        } else {
            debug!("no session for {}", client_id);
        }
        debug!("close session handled.");
        Ok(())
    }

    async fn handle_ping_req(
        &mut self,
        client_id: ClientId,
        _ping: proto::PingReq,
    ) -> Result<(), Error> {
        debug!("handling ping request...");
        if let Some(session) = self.sessions.get_mut(&client_id) {
            session
                .send(Message::new(client_id, Event::PingResp(proto::PingResp)))
                .await?;
        } else {
            debug!("no session for {}", client_id);
        }
        debug!("ping request handled.");
        Ok(())
    }
}

impl Default for Broker {
    fn default() -> Self {
        Broker::new()
    }
}

#[derive(Clone, Debug)]
pub struct BrokerHandle(Sender<Message>);

impl BrokerHandle {
    pub async fn send(&mut self, message: Message) -> Result<(), Error> {
        self.0
            .send(message)
            .await
            .context(ErrorKind::SendBrokerMessage)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::future::FutureExt;
    use matches::assert_matches;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_double_connect_protocol_violation() {
        let broker = Broker::default();
        let mut broker_handle = broker.handle();
        tokio::spawn(broker.run().map(drop));

        let connect1 = proto::Connect {
            username: None,
            password: None,
            will: None,
            client_id: proto::ClientId::IdWithCleanSession("blah".to_string()),
            keep_alive: Default::default(),
        };
        let connect2 = proto::Connect {
            username: None,
            password: None,
            will: None,
            client_id: proto::ClientId::IdWithCleanSession("blah".to_string()),
            keep_alive: Default::default(),
        };
        let id = Uuid::new_v4();
        let (tx1, mut rx1) = mpsc::channel(128);
        let conn1 = ConnectionHandle::new(id, tx1);
        let conn2 = conn1.clone();
        let client_id = ClientId::from("blah".to_string());

        broker_handle
            .send(Message::new(
                client_id.clone(),
                Event::Connect(connect1, conn1),
            ))
            .await
            .unwrap();
        broker_handle
            .send(Message::new(
                client_id.clone(),
                Event::Connect(connect2, conn2),
            ))
            .await
            .unwrap();

        assert_matches!(rx1.recv().await.unwrap().event(), Event::ConnAck(_));
        assert_matches!(rx1.recv().await.unwrap().event(), Event::DropConnection);
        assert!(rx1.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_double_connect_drop_first() {
        let broker = Broker::default();
        let mut broker_handle = broker.handle();
        tokio::spawn(broker.run().map(drop));

        let connect1 = proto::Connect {
            username: None,
            password: None,
            will: None,
            client_id: proto::ClientId::IdWithCleanSession("blah".to_string()),
            keep_alive: Default::default(),
        };
        let connect2 = proto::Connect {
            username: None,
            password: None,
            will: None,
            client_id: proto::ClientId::IdWithCleanSession("blah".to_string()),
            keep_alive: Default::default(),
        };
        let (tx1, mut rx1) = mpsc::channel(128);
        let (tx2, mut rx2) = mpsc::channel(128);
        let conn1 = ConnectionHandle::from_sender(tx1);
        let conn2 = ConnectionHandle::from_sender(tx2);
        let client_id = ClientId::from("blah".to_string());

        broker_handle
            .send(Message::new(
                client_id.clone(),
                Event::Connect(connect1, conn1),
            ))
            .await
            .unwrap();
        broker_handle
            .send(Message::new(
                client_id.clone(),
                Event::Connect(connect2, conn2),
            ))
            .await
            .unwrap();

        assert_matches!(rx1.recv().await.unwrap().event(), Event::ConnAck(_));
        assert_matches!(rx1.recv().await.unwrap().event(), Event::DropConnection);
        assert!(rx1.recv().await.is_none());

        assert_matches!(rx2.recv().await.unwrap().event(), Event::ConnAck(_));
    }
}
