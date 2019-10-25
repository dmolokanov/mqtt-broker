use std::fmt::Display;

use failure::ResultExt;
use futures_util::stream::StreamExt;
use futures_util::FutureExt;
use tokio::net::TcpListener;
use tokio_net::ToSocketAddrs;
use tracing::{debug, info, span, trace, warn, Level};
use tracing_futures::Instrument;

use crate::broker::Broker;
use crate::{connection, Error, ErrorKind};

pub struct Server {
    broker: Broker,
}

impl Server {
    pub fn new() -> Self {
        Self {
            broker: Default::default(),
        }
    }

    pub async fn serve<A>(self, addr: A) -> Result<(), Error>
    where
        A: ToSocketAddrs + Display,
    {
        let Server { broker } = self;
        let handle = broker.handle();
        let span = span!(Level::INFO, "server", listener=%addr);
        let _enter = span.enter();

        let mut incoming = TcpListener::bind(&addr)
            .await
            .context(ErrorKind::BindServer)?
            .incoming();
        info!("Listening on address {}", addr);

        // TODO: handle the broker returning an error.
        // TODO: handle server graceful shutdown
        tokio::spawn(broker.run().map(drop));

        while let Some(Ok(stream)) = incoming.next().await {
            let broker_handle = handle.clone();
            let span = span.clone();
            tokio::spawn(async move {
                if let Err(e) = connection::process(stream, broker_handle)
                    .instrument(span)
                    .await
                {
                    warn!(message = "failed to process connection", error=%e);
                }
            });
        }
        Ok(())
    }
}
