use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::{fmt, mem};

use failure::ResultExt;
use mqtt::proto;
use tokio::clock;
use tracing::warn;

use crate::subscription::Subscription;
use crate::{ClientId, ConnReq, ConnectionHandle, Error, ErrorKind, Event, Message};

#[derive(Debug)]
pub struct ConnectedSession {
    state: SessionState,
    handle: ConnectionHandle,
}

impl ConnectedSession {
    fn new(state: SessionState, handle: ConnectionHandle) -> Self {
        Self { state, handle }
    }

    pub fn handle(&self) -> &ConnectionHandle {
        &self.handle
    }

    pub fn into_handle(self) -> ConnectionHandle {
        self.handle
    }

    pub fn into_parts(self) -> (SessionState, ConnectionHandle) {
        (self.state, self.handle)
    }

    pub fn subscribe(&mut self, subscribe: proto::Subscribe) -> Result<proto::SubAck, Error> {
        let mut acks = Vec::with_capacity(subscribe.subscribe_to.len());
        let packet_identifier = subscribe.packet_identifier;

        for subscribe_to in subscribe.subscribe_to.into_iter() {
            let ack_qos = match subscribe_to.topic_filter.parse() {
                Ok(filter) => {
                    let proto::SubscribeTo { topic_filter, qos } = subscribe_to;

                    let subscription = Subscription::new(filter, qos);
                    self.state.update_subscription(topic_filter, subscription);
                    proto::SubAckQos::Success(qos)
                }
                Err(e) => {
                    warn!("invalid topic filter {}: {}", subscribe_to.topic_filter, e);
                    proto::SubAckQos::Failure
                }
            };
            acks.push(ack_qos);
        }

        let suback = proto::SubAck {
            packet_identifier,
            qos: acks,
        };
        Ok(suback)
    }

    pub fn unsubscribe(
        &mut self,
        unsubscribe: proto::Unsubscribe,
    ) -> Result<proto::UnsubAck, Error> {
        for filter in unsubscribe.unsubscribe_from.iter() {
            self.state.remove_subscription(&filter);
        }

        let unsuback = proto::UnsubAck {
            packet_identifier: unsubscribe.packet_identifier,
        };
        Ok(unsuback)
    }

    async fn send(&mut self, event: Event) -> Result<(), Error> {
        self.state.last_active = clock::now();

        let message = Message::new(self.state.client_id.clone(), event);
        self.handle
            .send(message)
            .await
            .context(ErrorKind::SendConnectionMessage)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct OfflineSession {
    state: SessionState,
}

impl OfflineSession {
    fn new(state: SessionState) -> Self {
        Self { state }
    }

    pub fn into_state(self) -> SessionState {
        self.state
    }
}

#[derive(Debug)]
pub struct SessionState {
    client_id: ClientId,
    keep_alive: Duration,
    last_active: Instant,
    subscriptions: HashMap<String, Subscription>,
}

impl SessionState {
    pub fn new(client_id: ClientId, connreq: &ConnReq) -> Self {
        Self {
            client_id,
            keep_alive: connreq.connect().keep_alive,
            last_active: clock::now(),
            subscriptions: HashMap::new(),
        }
    }

    pub fn update_subscription(
        &mut self,
        topic_filter: String,
        subscription: Subscription,
    ) -> Option<Subscription> {
        self.subscriptions.insert(topic_filter, subscription)
    }

    pub fn remove_subscription(&mut self, topic_filter: &str) -> Option<Subscription> {
        self.subscriptions.remove(topic_filter)
    }
}

#[derive(Debug)]
pub enum Session {
    Transient(ConnectedSession),
    Persistent(ConnectedSession),
    Disconnecting(ClientId, ConnectionHandle),
    Offline(OfflineSession),
}

impl Session {
    pub fn new_transient(connreq: ConnReq) -> Self {
        let state = SessionState::new(connreq.client_id().clone(), &connreq);
        let connected = ConnectedSession::new(state, connreq.into_handle());
        Session::Transient(connected)
    }

    pub fn new_persistent(connreq: ConnReq, state: SessionState) -> Self {
        let connected = ConnectedSession::new(state, connreq.into_handle());
        Session::Persistent(connected)
    }

    pub fn new_offline(state: SessionState) -> Self {
        let offline = OfflineSession::new(state);
        Session::Offline(offline)
    }

    pub fn subscribe(&mut self, subscribe: proto::Subscribe) -> Result<proto::SubAck, Error> {
        match self {
            Session::Transient(connected) => connected.subscribe(subscribe),
            Session::Persistent(connected) => connected.subscribe(subscribe),
            Session::Offline(_) => Err(Error::from(ErrorKind::SessionOffline)),
            Session::Disconnecting(_, _) => Err(Error::from(ErrorKind::SessionOffline)),
        }
    }

    pub fn unsubscribe(
        &mut self,
        unsubscribe: proto::Unsubscribe,
    ) -> Result<proto::UnsubAck, Error> {
        match self {
            Session::Transient(connected) => connected.unsubscribe(unsubscribe),
            Session::Persistent(connected) => connected.unsubscribe(unsubscribe),
            Session::Offline(_) => Err(Error::from(ErrorKind::SessionOffline)),
            Session::Disconnecting(_, _) => Err(Error::from(ErrorKind::SessionOffline)),
        }
    }

    pub async fn send(&mut self, event: Event) -> Result<(), Error> {
        match self {
            Session::Transient(ref mut connected) => connected.send(event).await,
            Session::Persistent(ref mut connected) => connected.send(event).await,
            Session::Disconnecting(ref client_id, ref mut handle) => {
                let message = Message::new(client_id.clone(), event);
                handle
                    .send(message)
                    .await
                    .context(ErrorKind::SendConnectionMessage)?;
                Ok(())
            }
            _ => Err(ErrorKind::SessionOffline.into()),
        }
    }
}

struct PacketIdentifiers {
    in_use: Box<[usize; PacketIdentifiers::SIZE]>,
    previous: proto::PacketIdentifier,
}

impl PacketIdentifiers {
    /// Size of a bitset for every packet identifier
    ///
    /// Packet identifiers are u16's, so the number of usize's required
    /// = number of u16's / number of bits in a usize
    /// = pow(2, number of bits in a u16) / number of bits in a usize
    /// = pow(2, 16) / (size_of::<usize>() * 8)
    ///
    /// We use a bitshift instead of usize::pow because the latter is not a const fn
    const SIZE: usize = (1 << 16) / (mem::size_of::<usize>() * 8);

    fn reserve(&mut self) -> Result<proto::PacketIdentifier, Error> {
        let start = self.previous;
        let mut current = start;

        current += 1;

        let (block, mask) = self.entry(current);
        if (*block & mask) != 0 {
            return Err(Error::from(ErrorKind::PacketIdentifiersExhausted));
        }

        *block |= mask;
        self.previous = current;
        Ok(current)
    }

    fn discard(&mut self, packet_identifier: proto::PacketIdentifier) {
        let (block, mask) = self.entry(packet_identifier);
        *block &= !mask;
    }

    fn entry(&mut self, packet_identifier: proto::PacketIdentifier) -> (&mut usize, usize) {
        let packet_identifier = usize::from(packet_identifier.get());
        let (block, offset) = (
            packet_identifier / (mem::size_of::<usize>() * 8),
            packet_identifier % (mem::size_of::<usize>() * 8),
        );
        (&mut self.in_use[block], 1 << offset)
    }
}

impl fmt::Debug for PacketIdentifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketIdentifiers")
            .field("previous", &self.previous)
            .finish()
    }
}

impl Default for PacketIdentifiers {
    fn default() -> Self {
        PacketIdentifiers {
            in_use: Box::new([0; PacketIdentifiers::SIZE]),
            previous: proto::PacketIdentifier::max_value(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc;
    use uuid::Uuid;

    use crate::ConnectionHandle;

    fn connection_handle() -> ConnectionHandle {
        let id = Uuid::new_v4();
        let (tx1, _rx1) = mpsc::channel(128);
        ConnectionHandle::new(id, tx1)
    }

    fn transient_connect(id: String) -> proto::Connect {
        proto::Connect {
            username: None,
            password: None,
            will: None,
            client_id: proto::ClientId::IdWithCleanSession(id),
            keep_alive: Default::default(),
            protocol_name: "MQTT".to_string(),
            protocol_level: 0x4,
        }
    }

    #[test]
    fn test_subscribe() {
        let id = "id1".to_string();
        let client_id = ClientId::from(id.clone());
        let connect1 = transient_connect(id.clone());
        let handle1 = connection_handle();
        let req1 = ConnReq::new(client_id.clone(), connect1, handle1);
        let mut session = Session::new_transient(req1);

        let subscribe = proto::Subscribe {
            packet_identifier: proto::PacketIdentifier::new(23).unwrap(),
            subscribe_to: vec![proto::SubscribeTo {
                topic_filter: "topic/new".to_string(),
                qos: proto::QoS::AtMostOnce,
            }],
        };
        let suback = session.subscribe(subscribe).unwrap();
        assert_eq!(
            proto::PacketIdentifier::new(23).unwrap(),
            suback.packet_identifier
        );
        match session {
            Session::Transient(ref connected) => {
                assert_eq!(1, connected.state.subscriptions.len());
                assert_eq!(
                    proto::QoS::AtMostOnce,
                    *connected.state.subscriptions["topic/new"].max_qos()
                );
            }
            _ => panic!("not transient"),
        }

        let subscribe = proto::Subscribe {
            packet_identifier: proto::PacketIdentifier::new(1).unwrap(),
            subscribe_to: vec![proto::SubscribeTo {
                topic_filter: "topic/new".to_string(),
                qos: proto::QoS::AtLeastOnce,
            }],
        };
        session.subscribe(subscribe).unwrap();

        match session {
            Session::Transient(ref connected) => {
                assert_eq!(1, connected.state.subscriptions.len());
                assert_eq!(
                    proto::QoS::AtLeastOnce,
                    *connected.state.subscriptions["topic/new"].max_qos()
                );
            }
            _ => panic!("not transient"),
        }
    }

    #[test]
    fn test_unsubscribe() {
        let id = "id1".to_string();
        let client_id = ClientId::from(id.clone());
        let connect1 = transient_connect(id.clone());
        let handle1 = connection_handle();
        let req1 = ConnReq::new(client_id.clone(), connect1, handle1);
        let mut session = Session::new_transient(req1);

        let subscribe = proto::Subscribe {
            packet_identifier: proto::PacketIdentifier::new(1).unwrap(),
            subscribe_to: vec![proto::SubscribeTo {
                topic_filter: "topic/new".to_string(),
                qos: proto::QoS::AtMostOnce,
            }],
        };
        session.subscribe(subscribe).unwrap();
        match session {
            Session::Transient(ref connected) => {
                assert_eq!(1, connected.state.subscriptions.len());
                assert_eq!(
                    proto::QoS::AtMostOnce,
                    *connected.state.subscriptions["topic/new"].max_qos()
                );
            }
            _ => panic!("not transient"),
        }

        let unsubscribe = proto::Unsubscribe {
            packet_identifier: proto::PacketIdentifier::new(1).unwrap(),
            unsubscribe_from: vec!["topic/different".to_string()],
        };
        session.unsubscribe(unsubscribe).unwrap();

        match session {
            Session::Transient(ref connected) => {
                assert_eq!(1, connected.state.subscriptions.len());
                assert_eq!(
                    proto::QoS::AtMostOnce,
                    *connected.state.subscriptions["topic/new"].max_qos()
                );
            }
            _ => panic!("not transient"),
        }

        let unsubscribe = proto::Unsubscribe {
            packet_identifier: proto::PacketIdentifier::new(24).unwrap(),
            unsubscribe_from: vec!["topic/new".to_string()],
        };
        let unsuback = session.unsubscribe(unsubscribe).unwrap();
        assert_eq!(
            proto::PacketIdentifier::new(24).unwrap(),
            unsuback.packet_identifier
        );

        match session {
            Session::Transient(ref connected) => {
                assert_eq!(0, connected.state.subscriptions.len());
            }
            _ => panic!("not transient"),
        }
    }

    #[test]
    fn test_offline_subscribe() {
        let id = "id1".to_string();
        let client_id = ClientId::from(id.clone());
        let connect1 = transient_connect(id.clone());
        let handle1 = connection_handle();
        let req1 = ConnReq::new(client_id.clone(), connect1, handle1);
        let mut session = Session::new_offline(SessionState::new(client_id, &req1));

        let subscribe = proto::Subscribe {
            packet_identifier: proto::PacketIdentifier::new(1).unwrap(),
            subscribe_to: vec![proto::SubscribeTo {
                topic_filter: "topic/new".to_string(),
                qos: proto::QoS::AtMostOnce,
            }],
        };
        let err = session.subscribe(subscribe).unwrap_err();
        assert_eq!(ErrorKind::SessionOffline, *err.kind());
    }

    #[test]
    fn packet_identifiers() {
        #[cfg(target_pointer_width = "32")]
        assert_eq!(PacketIdentifiers::SIZE, 2048);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(PacketIdentifiers::SIZE, 1024);

        let mut packet_identifiers: PacketIdentifiers = Default::default();
        assert_eq!(
            packet_identifiers.in_use[..],
            Box::new([0; PacketIdentifiers::SIZE])[..]
        );

        assert_eq!(packet_identifiers.reserve().unwrap().get(), 1);
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = 1 << 1;
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        assert_eq!(packet_identifiers.reserve().unwrap().get(), 2);
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = (1 << 1) | (1 << 2);
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        assert_eq!(packet_identifiers.reserve().unwrap().get(), 3);
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = (1 << 1) | (1 << 2) | (1 << 3);
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        packet_identifiers.discard(crate::proto::PacketIdentifier::new(2).unwrap());
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = (1 << 1) | (1 << 3);
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        assert_eq!(packet_identifiers.reserve().unwrap().get(), 4);
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = (1 << 1) | (1 << 3) | (1 << 4);
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        packet_identifiers.discard(crate::proto::PacketIdentifier::new(1).unwrap());
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = (1 << 3) | (1 << 4);
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        packet_identifiers.discard(crate::proto::PacketIdentifier::new(3).unwrap());
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = 1 << 4;
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        packet_identifiers.discard(crate::proto::PacketIdentifier::new(4).unwrap());
        assert_eq!(
            packet_identifiers.in_use[..],
            Box::new([0; PacketIdentifiers::SIZE])[..]
        );

        assert_eq!(packet_identifiers.reserve().unwrap().get(), 5);
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        expected[0] = 1 << 5;
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        let goes_in_next_block = std::mem::size_of::<usize>() * 8;
        #[allow(clippy::cast_possible_truncation)]
        for i in 6..=goes_in_next_block {
            assert_eq!(packet_identifiers.reserve().unwrap().get(), i as u16);
        }
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        #[allow(clippy::identity_op)]
        {
            expected[0] = usize::max_value() - (1 << 0) - (1 << 1) - (1 << 2) - (1 << 3) - (1 << 4);
            expected[1] |= 1 << 0;
        }
        assert_eq!(packet_identifiers.in_use[..], expected[..]);

        #[allow(clippy::cast_possible_truncation, clippy::range_minus_one)]
        for i in 5..=(goes_in_next_block - 1) {
            packet_identifiers.discard(crate::proto::PacketIdentifier::new(i as u16).unwrap());
        }
        let mut expected = Box::new([0; PacketIdentifiers::SIZE]);
        #[allow(clippy::identity_op)]
        {
            expected[1] |= 1 << 0;
        }
        assert_eq!(packet_identifiers.in_use[..], expected[..]);
    }
}
