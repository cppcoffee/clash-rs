use std::{collections::HashMap, io};

use erased_serde::Serialize;
use mockall::mock;

use crate::{
    app::{
        proxy_manager::providers::{
            proxy_provider::ProxyProvider, Provider, ProviderType, ProviderVehicleType,
        },
        ThreadSafeDNSResolver,
    },
    session::{Session, SocksAddr},
};

use super::{AnyOutboundDatagram, AnyOutboundHandler, AnyStream, OutboundHandler, OutboundType};

mock! {
    pub DummyProxyProvider {}

    #[async_trait::async_trait]
    impl Provider for DummyProxyProvider {
        fn name(&self) -> &str;
        fn vehicle_type(&self) -> ProviderVehicleType;
        fn typ(&self) -> ProviderType;
        async fn initialize(&mut self) -> std::io::Result<()>;
        async fn update(&self) -> std::io::Result<()>;
    }

    #[async_trait::async_trait]
    impl ProxyProvider for DummyProxyProvider {
        async fn proxies(&self) -> Vec<AnyOutboundHandler>;
        async fn touch(&self);
        async fn healthcheck(&self);
    }
}

mock! {
    pub DummyOutboundHandler {}

    #[async_trait::async_trait]
    impl OutboundHandler for DummyOutboundHandler {
        /// The name of the outbound handler
        fn name(&self) -> &str;

        /// The protocol of the outbound handler
        /// only contains Type information, do not rely on the underlying value
        fn proto(&self) -> OutboundType;

        /// The proxy remote address
        async fn remote_addr(&self) -> Option<SocksAddr>;

        /// whether the outbound handler support UDP
        async fn support_udp(&self) -> bool;

        /// connect to remote target via TCP
        async fn connect_stream(
            &self,
            sess: &Session,
            resolver: ThreadSafeDNSResolver,
        ) -> io::Result<AnyStream>;

        /// wraps a stream with outbound handler
        async fn proxy_stream(
            &self,
            s: AnyStream,
            sess: &Session,
            resolver: ThreadSafeDNSResolver,
        ) -> io::Result<AnyStream>;

        /// connect to remote target via UDP
        async fn connect_datagram(
            &self,
            sess: &Session,
            resolver: ThreadSafeDNSResolver,
        ) -> io::Result<AnyOutboundDatagram>;

        /// for API
        fn as_map(&self) -> HashMap<String, Box<dyn Serialize + Send>>;
    }
}
