use std::{io, sync::Arc};

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::debug;

use crate::{
    app::{
        proxy_manager::{
            providers::proxy_provider::ThreadSafeProxyProvider, ThreadSafeProxyManager,
        },
        ThreadSafeDNSResolver,
    },
    config::internal::proxy::OutboundProxy,
    session::{Session, SocksAddr},
};

use super::{
    utils::provider_helper::get_proxies_from_providers, AnyOutboundDatagram, AnyOutboundHandler,
    AnyStream, CommonOption, OutboundHandler, OutboundType,
};

#[derive(Default)]
pub struct HandlerOptions {
    pub name: String,
    pub udp: bool,

    pub common_option: CommonOption,
}

struct HandlerInner {
    fastest_proxy: Option<AnyOutboundHandler>,
}

pub struct Handler {
    opts: HandlerOptions,
    tolerance: u16,

    providers: Vec<ThreadSafeProxyProvider>,
    proxy_manager: ThreadSafeProxyManager,

    inner: Arc<Mutex<HandlerInner>>,
}

impl Handler {
    pub fn new(
        opts: HandlerOptions,
        tolerance: u16,
        providers: Vec<ThreadSafeProxyProvider>,
        proxy_manager: ThreadSafeProxyManager,
    ) -> Self {
        Self {
            opts,
            tolerance,
            providers,
            proxy_manager,
            inner: Arc::new(Mutex::new(HandlerInner {
                fastest_proxy: None,
            })),
        }
    }

    async fn get_proxies(&self, touch: bool) -> Vec<AnyOutboundHandler> {
        get_proxies_from_providers(&self.providers, touch).await
    }

    async fn fastest(&self, touch: bool) -> AnyOutboundHandler {
        let proxy_manager = self.proxy_manager.lock().await;
        let mut inner = self.inner.lock().await;

        let proxies = self.get_proxies(touch).await;
        let mut fastest = proxies
            .first()
            .expect(format!("no proxy found for {}", self.name()).as_str());

        let mut fastest_delay = proxy_manager.last_delay(fastest.name()).await;
        let mut fast_not_exist = true;

        for proxy in proxies.iter().skip(1) {
            if inner.fastest_proxy.is_some()
                && proxy.name() == inner.fastest_proxy.as_ref().unwrap().name()
            {
                fast_not_exist = false;
            }

            if !proxy_manager.alive(proxy.name()).await {
                continue;
            }

            let delay = proxy_manager.last_delay(proxy.name()).await;
            if delay < fastest_delay {
                fastest = proxy;
                fastest_delay = delay;
            }

            if inner.fastest_proxy.is_some()
                || fast_not_exist
                || proxy_manager.alive(fastest.name()).await
                || proxy_manager
                    .last_delay(inner.fastest_proxy.as_ref().unwrap().name())
                    .await
                    > fastest_delay + self.tolerance
            {
                inner.fastest_proxy = Some(fastest.clone());
            }
        }

        debug!(
            "{} fastest {} is {}",
            self.name(),
            fastest.name(),
            fastest_delay
        );

        return inner.fastest_proxy.as_ref().unwrap().clone();
    }
}

#[async_trait::async_trait]
impl OutboundHandler for Handler {
    /// The name of the outbound handler
    fn name(&self) -> &str {
        &self.opts.name
    }

    /// The protocol of the outbound handler
    fn proto(&self) -> OutboundType {
        OutboundType::UrlTest
    }

    /// The proxy remote address
    async fn remote_addr(&self) -> Option<SocksAddr> {
        self.fastest(false).await.remote_addr().await
    }

    /// whether the outbound handler support UDP
    async fn support_udp(&self) -> bool {
        self.opts.udp || self.fastest(false).await.support_udp().await
    }

    /// connect to remote target via TCP
    async fn connect_stream(
        &self,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> io::Result<AnyStream> {
        self.fastest(false)
            .await
            .connect_stream(sess, resolver)
            .await
    }

    /// wraps a stream with outbound handler
    async fn proxy_stream(
        &self,
        s: AnyStream,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> io::Result<AnyStream> {
        self.fastest(true)
            .await
            .proxy_stream(s, sess, resolver)
            .await
    }

    /// connect to remote target via UDP
    async fn connect_datagram(
        &self,
        sess: &Session,
        resolver: ThreadSafeDNSResolver,
    ) -> io::Result<AnyOutboundDatagram> {
        self.fastest(false)
            .await
            .connect_datagram(sess, resolver)
            .await
    }
}
