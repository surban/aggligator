#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: WebSocket on the web targeting WebAssembly.

#[cfg(not(target_family = "wasm"))]
compile_error!("aggligator-transport-websocket-web requires a WebAssembly target");

use async_trait::async_trait;
use bytes::Bytes;
use futures::{future, Sink, SinkExt, StreamExt};
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::watch;

use websocket_web::WebSocketSender;

#[doc(no_inline)]
pub use websocket_web::{Interface, WebSocketBuilder};

use aggligator::{
    control::Direction,
    io::{StreamBox, TxRxBox},
    transport::{ConnectingTransport, LinkTag, LinkTagBox},
};

mod thread_bound;
use thread_bound::ThreadBound;

static NAME: &str = "websocket";

/// Link tag for outgoing WebSocket link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutgoingWebSocketLinkTag {
    /// Remote URL.
    pub url: String,
}

impl fmt::Display for OutgoingWebSocketLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", &self.url)
    }
}

impl LinkTag for OutgoingWebSocketLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        Direction::Outgoing
    }

    fn user_data(&self) -> Vec<u8> {
        "web".into()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }

    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// WebSocket transport for outgoing connections using the browser's WebSocket API.
///
/// This transport is packet-based.
#[derive(Clone)]
pub struct WebSocketConnector {
    urls: Vec<String>,
    cfg_fn: Arc<dyn Fn(&mut WebSocketBuilder) + Send + Sync + 'static>,
}

impl fmt::Debug for WebSocketConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebSocketConnector").field("urls", &self.urls).finish()
    }
}

impl fmt::Display for WebSocketConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let urls: Vec<_> = self.urls.iter().map(|url| url.to_string()).collect();
        if self.urls.len() > 1 {
            write!(f, "[{}]", urls.join(", "))
        } else {
            write!(f, "{}", &urls[0])
        }
    }
}

impl WebSocketConnector {
    /// Create a new WebSocket transport for outgoing connections.
    ///
    /// `urls` contains one or more WebSocket URLs of the target.
    ///
    /// Name resolution and certificate validation is handled by the browser.
    pub async fn new(urls: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self> {
        let urls = urls.into_iter().map(|url| url.as_ref().to_string()).collect::<Vec<_>>();

        if urls.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one URL is required"));
        }

        Ok(Self { urls, cfg_fn: Arc::new(|_| ()) })
    }

    /// Sets the configuration function that is applied to each
    /// [WebSocket builder](WebSocketBuilder) before it is connected.
    pub fn set_cfg(&mut self, cfg_fn: impl Fn(&mut WebSocketBuilder) + Send + Sync + 'static) {
        self.cfg_fn = Arc::new(cfg_fn);
    }
}

#[async_trait]
impl ConnectingTransport for WebSocketConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        let mut tags: HashSet<LinkTagBox> = HashSet::new();
        for url in &self.urls {
            tags.insert(Box::new(OutgoingWebSocketLinkTag { url: url.clone() }));
        }

        tx.send_replace(tags);

        future::pending().await
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &OutgoingWebSocketLinkTag = tag.as_any().downcast_ref().unwrap();

        // WebSocket is not Send + Sync, thus we need to wrap the following
        // code in a ThreadBound. It ensures that execution takes place on
        // a single thread but appears to be Send + Sync.
        ThreadBound::new(async {
            // Configure WebSocket.
            let mut builder = WebSocketBuilder::new(&tag.url);
            (self.cfg_fn)(&mut builder);

            // Establish WebSocket connection.
            let websocket = builder.connect().await?;

            // Adapt WebSocket IO.
            let (tx, rx) = websocket.into_split();
            let tx = Box::pin(ThreadBound::new(WebSocketSink(tx)));
            let rx = Box::pin(ThreadBound::new(rx.map(|res| res.map(|msg| Bytes::from(msg.to_vec())))));
            Ok(TxRxBox::new(tx, rx).into())
        })
        .await
    }
}

struct WebSocketSink(WebSocketSender);

impl Sink<Bytes> for WebSocketSink {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        <WebSocketSender as SinkExt<&[u8]>>::poll_ready_unpin(&mut self.0, cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<()> {
        <WebSocketSender as SinkExt<&[u8]>>::start_send_unpin(&mut self.0, &*item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        <WebSocketSender as SinkExt<&[u8]>>::poll_flush_unpin(&mut self.0, cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        <WebSocketSender as SinkExt<&[u8]>>::poll_close_unpin(&mut self.0, cx)
    }
}
