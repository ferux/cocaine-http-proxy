use std::borrow::Cow;
use std::collections::HashMap;
use std::error;
use std::fmt::{self, Display, Formatter};
use std::str;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use byteorder::{ByteOrder, LittleEndian};

use rand;

use futures::{self, Async, Future, Poll, Stream, future};
use futures::sync::oneshot;

use hyper::{self, HttpVersion, Method, StatusCode};
use hyper::header::{Headers, Header};
use hyper::server::{Request, Response};

use regex::Regex;

use rmps;

use serde::Serializer;

use cocaine::{self, Dispatch, Service};
use cocaine::hpack::{self, Header as CocaineHeader};
use cocaine::logging::Log;
use cocaine::protocol::{self, Flatten};

use crate::common::{TracingPolicy, XCocaineEvent, XCocaineService, XPoweredBy, XRequestId, XTracingPolicy,
    XCocaineApp, XErrorGeneratedBy};
use crate::logging::AccessLogger;
use crate::pool::{Event, EventDispatch, Settings};
use crate::route::{Match, Route, serialize};

fn pack_u64(v: u64) -> Vec<u8> {
    let mut buf = vec![0; 8];
    LittleEndian::write_u64(&mut buf[..], v);
    buf
}

trait Call {
    type Call: Fn(&Service, Settings) -> Box<dyn Future<Item = (), Error = ()> + Send> + Send;
    type Future: Future<Item = Response, Error = Error>;

    /// Selects an appropriate service with its settings from the pool and
    /// passes them to the provided callback.
    fn call_service(&self, name: String, callback: Self::Call) -> Self::Future;
}

pub struct AppRoute<L> {
    dispatcher: EventDispatch,
    headers: HashMap<String, String>,
    tracing_header: Cow<'static, str>,
    regex: Regex,
    log: L,
}

impl<L: Log + Clone + Send + Sync + 'static> AppRoute<L> {
    pub fn new(dispatcher: EventDispatch, log: L) -> Self {
        let header = XRequestId::header_name();
        Self {
            dispatcher: dispatcher,
            headers: HashMap::new(),
            tracing_header: header.into(),
            regex: Regex::new("/([^/]*)/([^/?]*)(.*)").expect("invalid URI regex in app route"),
            log: log,
        }
    }

    pub fn with_tracing_header<H>(mut self, header: H) -> Self
        where H: Into<Cow<'static, str>>
    {
        self.tracing_header = header.into();
        self
    }

    pub fn with_headers_mapping(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    /// Extracts required parameters from the request.
    fn extract_parameters(&self, req: &Request) -> Option<Result<(String, String, String), Error>> {
        let service = req.headers().get::<XCocaineService>();
        let event = req.headers().get::<XCocaineEvent>();

        match (service, event) {
            (Some(service), Some(event)) => {
                Some(Ok((service.to_string(), event.to_string(), req.uri().to_string())))
            }
            (Some(..), None) | (None, Some(..)) => Some(Err(Error::IncompleteHeadersMatch)),
            (None, None) => {
                self.regex.captures(req.uri().as_ref()).and_then(|cap| {
                    match (cap.get(1), cap.get(2), cap.get(3)) {
                        (Some(service), Some(event), Some(other)) => {
                            let uri = other.as_str();
                            let uri = if uri.starts_with("/") {
                                uri.into()
                            } else {
                                format!("/{}", uri)
                            };

                            Some(Ok((service.as_str().into(), event.as_str().into(), uri)))
                        }
                        (..) => None,
                    }
                })
            }
        }
    }

    fn map_headers(&self, headers: &Headers) -> Vec<hpack::RawHeader> {
        self.headers.iter()
            .filter_map(|(name, mapped)| headers.get_raw(name).map(|v| (mapped, v)))
            .map(|(name, value)| {
                let value = value.into_iter().fold(Vec::new(), |mut vec, v| {
                    vec.extend(v);
                    vec
                });

                hpack::RawHeader::new(name.clone().into_bytes(), value)
            })
            .collect()
    }

    fn invoke(&self, service: String, event: String, req: Request, uri: String)
        -> Box<dyn Future<Item = Response, Error = Error>>
    {
        let trace = if let Some(trace) = req.headers().get_raw(&self.tracing_header) {
            match XRequestId::parse_header(trace) {
                Ok(v) => v.into(),
                Err(..) => {
                    let err = Error::InvalidRequestIdHeader(self.tracing_header.clone());
                    return Box::new(future::err(err))
                }
            }
        } else {
            // TODO: Log (debug) trace id source (header or generated).
            rand::random::<u64>()
        };

        let tracing_policy = req.headers()
            .get::<XTracingPolicy>()
            .map(|&v| v.into())
            .unwrap_or(TracingPolicy::Auto);

        let log = AccessLogger::new(self.log.clone(), &req, service.clone(), event.clone(), trace);
        let headers = self.map_headers(req.headers());
        let mut app_request = AppRequest::new(service.clone(), event, trace, &req, uri);
        let dispatcher = self.dispatcher.clone();
        let future = req.body()
            .concat2()
            .map_err(Error::InvalidBodyRead)
            .and_then(move |body| {
                app_request.set_body(body.to_vec());
                AppWithSafeRetry::new(app_request, headers, dispatcher, 3, tracing_policy)
            })
            .then(move |result| {
                match result {
                    Ok((mut resp, size)) => {
                        resp.headers_mut().set(XPoweredBy::default());
                        resp.headers_mut().set(XCocaineApp(service));

                        log.commit(resp.status(), size, None);
                        Ok(resp)
                    }
                    Err(err) => {
                        log.commit(StatusCode::InternalServerError, 0, Some(&err));
                        Err(err)
                    }
                }
            });

        Box::new(future)
    }
}

impl<L: Log + Clone + Send + Sync + 'static> Route for AppRoute<L> {
    type Future = Box<dyn Future<Item = Response, Error = hyper::Error>>;

    fn process(&self, req: Request) -> Match<Self::Future> {
        match self.extract_parameters(&req) {
            Some(Ok((service, event, uri))) => {
                let future = self.invoke(service, event, req, uri).then(|resp| {
                    resp.or_else(|err| {
                        let resp = Response::new()
                            .with_status(err.code())
                            .with_body(err.to_string());
                        Ok(resp)
                    })
                });
                Match::Some(Box::new(future))
            }
            Some(Err(err)) => {
                let resp = Response::new()
                    .with_status(err.code())
                    .with_body(err.to_string());
                Match::Some(Box::new(future::ok(resp)))
            }
            None => Match::None(req),
        }
    }
}

/// A meta frame of HTTP request for cocaine application HTTP protocol.
#[derive(Clone, Serialize)]
pub(crate) struct RequestMeta {
    #[serde(serialize_with = "serialize_method")]
    pub(crate) method: Method,
    pub(crate) uri: String,
    #[serde(serialize_with = "serialize_version")]
    pub(crate) version: HttpVersion,
    pub(crate) headers: Vec<(String, String)>,
    /// HTTP body. May be empty either when there is no body in the request or if it is transmitted
    /// later.
    #[serde(serialize_with = "serialize_body")]
    pub(crate) body: Vec<u8>,
}

#[inline]
fn serialize_method<S>(method: &Method, se: S) -> Result<S::Ok, S::Error>
    where S: Serializer
{
    se.serialize_str(&method.to_string())
}

#[inline]
fn serialize_version<S>(version: &HttpVersion, se: S) -> Result<S::Ok, S::Error>
    where S: Serializer
{
    let v = if let &HttpVersion::Http11 = version {
        "1.1"
    } else {
        "1.0"
    };

    se.serialize_str(v)
}

#[inline]
fn serialize_body<S>(body: &[u8], se: S) -> Result<S::Ok, S::Error>
    where S: Serializer
{
    se.serialize_str(unsafe { str::from_utf8_unchecked(body) })
}

#[derive(Debug, Deserialize)]
struct ResponseMeta {
    code: u32,
    headers: Vec<(String, String)>
}

#[derive(Clone)]
struct AppRequest {
    service: String,
    event: String,
    trace: u64,
    frame: RequestMeta,
}

impl AppRequest {
    fn new(service: String, event: String, trace: u64, req: &Request, uri: String) -> Self {
        let headers = req.headers()
            .iter()
            .map(|header| {
                let value = header.raw().into_iter().fold(Vec::new(), |mut vec, v| {
                    vec.extend(v);
                    vec
                });
                let value = unsafe { String::from_utf8_unchecked(value) };

                (header.name().to_string(), value)
            })
            .collect();

        let frame = RequestMeta {
            method: req.method().clone(),
            version: req.version(),
            // TODO: Test that uri is sent properly (previously only path was sent).
            uri: uri,
            headers: headers,
            body: Vec::new(),
        };

        Self {
            service: service,
            event: event,
            trace: trace,
            frame: frame,
        }
    }

    fn set_body(&mut self, body: Vec<u8>) {
        self.frame.body = body;
    }
}

/// A future that retries application invocation on receiving "safe" error,
/// meaning that it is safe to retry it again until the specified limit reached.
///
/// In this context "safety" means, that the request is guaranteed not to be
/// delivered to the worker, for example when the queue is full.
struct AppWithSafeRetry {
    attempts: u32,
    limit: u32,
    request: Arc<AppRequest>,
    dispatcher: EventDispatch,
    headers: Vec<hpack::RawHeader>,
    current: Option<Box<dyn Future<Item=Option<(Response, u64)>, Error=Error> + Send>>,
    verbose: Arc<AtomicBool>,
    tracing_policy: TracingPolicy,
}

impl AppWithSafeRetry {
    fn new(request: AppRequest, headers: Vec<hpack::RawHeader>, dispatcher: EventDispatch, limit: u32, tracing_policy: TracingPolicy) -> Self {
        let headers = Self::make_headers(headers, request.trace);

        let mut res = Self {
            attempts: 1,
            limit: limit,
            request: Arc::new(request),
            dispatcher: dispatcher,
            headers: headers,
            current: None,
            verbose: Arc::new(AtomicBool::new(false)),
            tracing_policy: tracing_policy,
        };

        res.current = Some(res.make_future());

        res
    }

    fn make_headers(headers: Vec<hpack::RawHeader>, trace: u64) -> Vec<hpack::RawHeader> {
        let mut headers = if headers.is_empty() {
            Vec::with_capacity(4)
        } else {
            headers
        };

        let span = rand::random::<u64>();
        headers.push(hpack::TraceId(trace).into_raw());
        headers.push(hpack::SpanId(span).into_raw());
        headers.push(hpack::ParentId(trace).into_raw());

        headers
    }

    fn make_future(&self) -> Box<dyn Future<Item=Option<(Response, u64)>, Error=Error> + Send> {
        let (tx, rx) = oneshot::channel();

        let request = self.request.clone();
        let verbose = self.verbose.clone();
        let attempt = self.attempts;
        let headers = self.headers.clone();

        let manual_verbose = match self.tracing_policy {
            TracingPolicy::Auto => None,
            TracingPolicy::Manual(v) => Some(rand::random::<f64>() <= v),
        };

        let ev = Event::Service {
            name: request.service.clone(),
            func: Box::new(move |service: &Service, mut settings: Settings| {
                let mut headers = headers.clone();
                if let Some(true) = manual_verbose {
                    settings.verbose = true;
                }

                if settings.verbose && attempt == 1 {
                    verbose.store(true, Ordering::Release);
                }

                if verbose.load(Ordering::Acquire) {
                    headers.push(hpack::TraceBit(true).into_raw());
                }

                if let Some(timeout) = settings.timeout {
                    headers.push(hpack::RawHeader::new(&b"request_timeout"[..], pack_u64((timeout * 1000.0) as u64)));
                }

                let req = cocaine::Request::new(0, &[request.event.clone()]).unwrap()
                    .add_headers(headers);

                let future = service.call(req, AppReadDispatch {
                    tx: tx,
                    method: request.frame.method.clone(),
                    body: None,
                    trace: request.trace,
                    response: Some(Response::new()),
                }).and_then(move |tx| {
                    let buf = serialize::to_vec(&request.frame).unwrap();
                    tx.send(cocaine::Request::new(0, &[unsafe { ::std::str::from_utf8_unchecked(&buf) }]).unwrap());
                    tx.send(cocaine::Request::new(2, &[0; 0]).unwrap());
                    Ok(())
                }).then(|_| {
                    // TODO: Consider if it is okay to always finish the future with OK. May be log?
                    Ok(())
                });

                // box future as Box<Future<Item = (), Error = ()> + Send>
                Box::new(future)
            }),
        };

        self.dispatcher.send(ev);

        let future = rx.map_err(|futures::Canceled| Error::Canceled);
        Box::new(future)
    }
}

impl Future for AppWithSafeRetry {
    type Item = (Response, u64);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut future = self.current.take().unwrap();

        match future.poll() {
            Ok(Async::Ready(Some((res, bytes)))) => return Ok(Async::Ready((res, bytes))),
            Ok(Async::Ready(None)) => {
                if self.attempts < self.limit {
                    self.current = Some(self.make_future());
                    self.attempts += 1;
                    return self.poll();
                } else {
                    let body = "Retry limit exceeded: queue is full";
                    let bytes = body.len() as u64;
                    let resp = Response::new()
                        .with_status(StatusCode::InternalServerError)
                        .with_header(XRequestId(self.request.trace))
                        .with_body(body);
                    return Ok(Async::Ready((resp, bytes)));
                }
            }
            Ok(Async::NotReady) => {}
            Err(err) => {
                return Err(err);
            }
        }

        self.current = Some(future);
        Ok(Async::NotReady)
    }
}

#[derive(Debug)]
enum Error {
    /// Either none or both `X-Cocaine-Service` and `X-Cocaine-Event` headers
    /// must be specified.
    IncompleteHeadersMatch,
    /// Failed to parse special tracing header, by default `X-Request-Id`.
    InvalidRequestIdHeader(Cow<'static, str>),
//    RetryLimitExceeded(u32),
//    Service(cocaine::Error),
    InvalidBodyRead(hyper::Error),
    Canceled,
}

impl Error {
    fn code(&self) -> StatusCode {
        match *self {
            Error::IncompleteHeadersMatch |
            Error::InvalidRequestIdHeader(..) => StatusCode::BadRequest,
            Error::InvalidBodyRead(..) |
            Error::Canceled => StatusCode::InternalServerError,
        }
    }
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::IncompleteHeadersMatch => fmt.write_str(error::Error::description(self)),
            Error::InvalidRequestIdHeader(ref name) => {
                write!(fmt, "Invalid `{}` header value", name)
            }
            Error::InvalidBodyRead(ref err) => write!(fmt, "{}", err),
            Error::Canceled => fmt.write_str("canceled"),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::IncompleteHeadersMatch => {
                "either none or both `X-Cocaine-Service` and `X-Cocaine-Event` headers must be specified"
            }
            Error::InvalidRequestIdHeader(..) => "invalid tracing header value",
            Error::InvalidBodyRead(..) => "failed to read HTTP body",
            Error::Canceled => "canceled",
        }
    }
}

struct AppReadDispatch {
    tx: oneshot::Sender<Option<(Response, u64)>>,
    method: Method,
    body: Option<Vec<u8>>,
    trace: u64,
    response: Option<Response>,
}

impl Dispatch for AppReadDispatch {
    fn process(mut self: Box<Self>, response: &cocaine::Response) -> Option<Box<dyn Dispatch>> {
        match response.deserialize::<protocol::Streaming<rmps::RawRef>>().flatten() {
            // TODO: Support chunked transfer encoding.
            Ok(Some(data)) => {
                if self.body.is_none() {
                    let meta: ResponseMeta = match rmps::from_slice(data.as_bytes()) {
                        Ok(meta) => meta,
                        Err(err) => {
                            let err = err.to_string();
                            let body_size = err.len();
                            let resp = Response::new()
                                .with_status(StatusCode::InternalServerError)
                                .with_header(XRequestId(self.trace))
                                .with_body(err);
                            drop(self.tx.send(Some((resp, body_size as u64))));
                            return None
                        }
                    };

                    let status = StatusCode::try_from(meta.code as u16)
                        .unwrap_or(StatusCode::InternalServerError);

                    let mut resp = self.response.take().unwrap();
                    resp.set_status(status);
                    resp.headers_mut().set(XRequestId(self.trace));
                    for (name, value) in meta.headers {
                        // TODO: Filter headers - https://tools.ietf.org/html/draft-ietf-httpbis-p1-messaging-14#section-7.1.3
                        resp.headers_mut().set_raw(name, value);
                    }
                    self.response = Some(resp);
                    self.body = Some(Vec::with_capacity(64));
                } else {
                    // TODO: If TE: chunked - feed parser. Consume chunks until None and send.
                    // TODO: Otherwise - just send.
                    self.body.as_mut().unwrap().extend(data.as_bytes());
                }
                Some(self)
            }
            Ok(None) => {
                let (resp, size) = match self.body.take() {
                    Some(body) => {
                        use hyper::header::ContentLength;
                        let mut resp = self.response.take().unwrap();

                        // Special handling for responses with no body.
                        // See https://www.w3.org/Protocols/rfc2616/rfc2616-sec10.html for more.
                        let size = if self.method == Method::Head {
                            // We shouldn't remove Content-Length header here, because according to
                            // RFC: the Content-Length entity-header field indicates the size of the
                            // entity-body, in decimal number of OCTETs, sent to the recipient or,
                            // in the case of the HEAD method, the size of the entity-body that
                            // would have been sent had the request been a GET.
                            0
                        } else {
                            match resp.status() {
                                StatusCode::NoContent |
                                StatusCode::NotModified => {
                                    0
                                }
                                _ => {
                                    let mut has_body = !body.is_empty();
                                    has_body = has_body & resp.headers().get::<ContentLength>().map(|head| {
                                        match *head {
                                            ContentLength(len) => return len > 0,
                                        }
                                    }).unwrap_or(true);

                                    if has_body {
                                        let size = body.len();
                                        resp.set_body(body);
                                        size
                                    } else {
                                        0
                                    }
                                }
                            }
                        };

                        (resp, size)
                    }
                    None => {
                        let err = "received `close` event without prior meta info";
                        let size = err.len();
                        let resp = Response::new()
                            .with_status(StatusCode::InternalServerError)
                            .with_header(XRequestId(self.trace))
                            .with_body(err);

                        (resp, size)
                    }
                };

                drop(self.tx.send(Some((resp, size as u64))));
                None
            }
            // TODO: Make names for category and code.
            Err(cocaine::Error::Service(ref err)) if err.category() == 0x52ff && err.code() == 1 => {
                drop(self.tx.send(None));
                None
            }
            Err(err) => {
                let body = err.to_string();
                let body_len = body.len() as u64;

                let mut resp = Response::new()
                    .with_status(StatusCode::InternalServerError)
                    .with_header(XRequestId(self.trace))
                    .with_body(body);

                if let cocaine::Error::Service(ref err) = err {
                    if err.category() == 0x54ff {
                        resp.headers_mut().set(XErrorGeneratedBy("vicodyn".to_string()));
                    }
                }

                drop(self.tx.send(Some((resp, body_len))));
                None
            }
        }
    }

    fn discard(self: Box<Self>, err: &cocaine::Error) {
        let body = err.to_string();
        let body_len = body.as_bytes().len() as u64;

        let status = if let cocaine::Error::Service(ref err) = *err {
            if err.category() == 10 && err.code() == 1 {
                StatusCode::ServiceUnavailable
            } else {
                StatusCode::InternalServerError
            }
        } else {
            StatusCode::InternalServerError
        };

        let resp = Response::new()
            .with_status(status)
            .with_header(XRequestId(self.trace))
            .with_body(body);
        drop(self.tx.send(Some((resp, body_len))));
    }
}

#[cfg(test)]
mod test {
    use hyper::HttpVersion;
    use serde_json::Serializer;

    use super::serialize_version;

    #[test]
    fn test_serialize_version() {
        let mut se = Serializer::new(Vec::new());
        serialize_version(&HttpVersion::Http10, &mut se).unwrap();
        assert_eq!(&b"\"1.0\""[..], &se.into_inner()[..]);

        let mut se = Serializer::new(Vec::new());
        serialize_version(&HttpVersion::Http11, &mut se).unwrap();
        assert_eq!(&b"\"1.1\""[..], &se.into_inner()[..]);
    }
}

// TODO: Test HEAD responses with body.
// TODO: Test NoContent responses with body.
// TODO: Test NotModified responses with body.
// TODO: Test invalid UTF8 headers in request/response.
