use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{future, Future};
use futures::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_core::reactor::{Handle, Timeout};
use tokio_service::Service;

use hyper::{self, StatusCode};
use hyper::server::{Request, Response};

use cocaine::{Resolver, ServiceBuilder};
use cocaine::service::Locator;
use cocaine::logging::{Severity, Logger};

use crate::{Metrics, DEFAULT_LOCATOR_NAME};
use crate::config::Config;
use crate::metrics::{Meter, Count};
use crate::pool::{Event, PoolTask};
use crate::route::Router;
use crate::service::{ServiceFactory, ServiceFactorySpawn};

pub struct ProxyService {
    addr: Option<SocketAddr>,
    router: Router,
    metrics: Arc<Metrics>,
    log: Logger,
}

impl ProxyService {
    fn new(addr: Option<SocketAddr>, router: Router, metrics: Arc<Metrics>, log: Logger) -> Self {
        metrics.connections.active.add(1);
        metrics.connections.accepted.add(1);

        if let Some(addr) = addr {
            cocaine_log!(log, Severity::Info, "accepted connection from {}", addr);
        } else {
            cocaine_log!(log, Severity::Info, "accepted connection from Unix socket");
        }

        Self {
            addr: addr,
            router: router,
            metrics: metrics,
            log: log,
        }
    }
}

impl Service for ProxyService {
    type Request  = Request;
    type Response = Response;
    type Error    = hyper::Error;
    type Future   = Box<dyn Future<Item = Response, Error = Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        let metrics = self.metrics.clone();

        metrics.requests.mark(1);
        Box::new(self.router.process(req).and_then(move |resp| {
            if resp.status().is_server_error() {
                metrics.responses.c5xx.mark(1);
            }

            Ok(resp)
        }))
    }
}

impl Drop for ProxyService {
    fn drop(&mut self) {
        if let Some(addr) = self.addr.take() {
            cocaine_log!(self.log, Severity::Info, "closed connection from {}", addr);
        } else {
            cocaine_log!(self.log, Severity::Info, "closed connection from Unix socket");
        }

        self.metrics.connections.active.add(-1);
    }
}

pub struct TimedOut;

impl From<TimedOut> for Response {
    fn from(timeout: TimedOut) -> Self {
        match timeout {
            TimedOut => {
                Response::new()
                    .with_status(StatusCode::GatewayTimeout)
                    .with_body("Timed out while waiting for response from the Cocaine")
            }
        }
    }
}

pub struct TimeoutMiddleware<T> {
    upstream: T,
    timeout: Duration,
    handle: Handle,
}

impl<T> TimeoutMiddleware<T> {
    fn new(upstream: T, timeout: Duration, handle: Handle) -> Self {
        Self {
            upstream: upstream,
            timeout: timeout,
            handle: handle,
        }
    }
}

impl<T> Service for TimeoutMiddleware<T>
    where T: Service,
          T::Response: From<TimedOut>,
          T::Error: From<io::Error> + 'static,
          T::Future: 'static
{
    type Request  = T::Request;
    type Response = T::Response;
    type Error    = T::Error;
    type Future   = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let timeout = future::result(Timeout::new(self.timeout, &self.handle))
            .flatten()
            .map(|()| Self::Response::from(TimedOut))
            .map_err(From::from);

        let future = self.upstream.call(req)
            .select(timeout)
            .map(|v| v.0)
            .map_err(|e| e.0);

        Box::new(future)
    }
}

#[derive(Clone)]
pub struct ProxyServiceFactory {
    router: Router,
    timeout: Duration,
    handle: Handle,
    metrics: Arc<Metrics>,
    log: Logger,
}

impl ServiceFactory for ProxyServiceFactory {
    type Request  = Request;
    type Response = Response;
    type Instance = TimeoutMiddleware<ProxyService>;
    type Error    = hyper::Error;

    fn create_service(&mut self, addr: Option<SocketAddr>) -> Result<Self::Instance, io::Error> {
        let service = ProxyService::new(addr, self.router.clone(), self.metrics.clone(), self.log.clone());
        let wrapped = TimeoutMiddleware::new(service, self.timeout, self.handle.clone());

        Ok(wrapped)
    }
}

pub struct ProxyServiceFactoryFactory<I> {
    channels: Mutex<I>,
    cfg: Config,
    router: Router,
    metrics: Arc<Metrics>,
    log: Logger,
}

impl<I> ProxyServiceFactoryFactory<I>
where
    I: Iterator<Item = (UnboundedSender<Event>, UnboundedReceiver<Event>)> + Send
{
    pub fn new(channels: I,
               cfg: Config,
               router: Router,
               metrics: Arc<Metrics>,
               log: Logger) -> Self
    {
        Self {
            channels: Mutex::new(channels),
            cfg: cfg,
            router: router,
            metrics: metrics,
            log: log,
        }
    }
}

impl<I> ServiceFactorySpawn for ProxyServiceFactoryFactory<I>
where
    I: Iterator<Item = (UnboundedSender<Event>, UnboundedReceiver<Event>)> + Send
{
    type Factory = ProxyServiceFactory;

    fn create_factory(&self, handle: &Handle) -> Self::Factory {
        let (tx, rx) = self.channels.lock().unwrap().next()
            .expect("number of event channels must be exactly the same as the number of threads");

        let locator_addrs = self.cfg.locators().iter()
            .map(|&(addr, port)| SocketAddr::new(addr, port))
            .collect::<Vec<SocketAddr>>();
        let locator = ServiceBuilder::new(DEFAULT_LOCATOR_NAME)
            .locator_addrs(locator_addrs)
            .build(handle);
        let locator = Locator::new(locator);
        let resolver = Resolver::new(locator);

        // This will stop after all associated connections are closed.
        let pool = PoolTask::new(handle.clone(), resolver, self.log.clone(), tx, rx, self.cfg.clone());

        handle.spawn(pool);
        ProxyServiceFactory {
            router: self.router.clone(),
            timeout: self.cfg.timeout(),
            handle: handle.clone(),
            metrics: self.metrics.clone(),
            log: self.log.clone(),
        }
    }
}
