//! Contain a route that is used primarily for performance measuring.
//!
//! Currently all requests are transformed into a Geobase requests.

use std::io::{self, ErrorKind};

use futures::Future;
use futures::sync::oneshot;

use hyper::{self, StatusCode};
use hyper::header::ContentLength;
use hyper::server::{Response, Request};

use cocaine::{self, Dispatch, Error, Service};
use cocaine::logging::Logger;
use cocaine::protocol::{Primitive, Flatten};

use crate::logging::AccessLogger;
use crate::pool::{Event, EventDispatch, Settings};
use crate::route::{Match, Route};

pub struct PerfRoute {
    dispatcher: EventDispatch,
    log: Logger,
}

impl PerfRoute {
    pub fn new(dispatcher: EventDispatch, log: Logger) -> Self {
        Self {
            dispatcher: dispatcher,
            log: log,
        }
    }
}

impl Route for PerfRoute {
    type Future = Box<dyn Future<Item = Response, Error = hyper::Error>>;

    fn process(&self, req: Request) -> Match<Self::Future> {
        // let (tx, rx) = oneshot::channel();

        todo!()
        // let ev = Event::Service {
        //     name: "geobase".to_owned(),
            // func: move |service: &Service, _settings: Settings| {
            //     let future = service.call(cocaine::Request::new(0, &["8.8.8.8"]).unwrap(), SingleChunkReadDispatch { tx: tx })
            //         .then(|tx| {
            //             drop(tx);
            //             Ok(())
            //         });
            //     Box::new(future) as Box<dyn Future<Item = (), Error = ()> + Send>
            // },
        // };

        // self.dispatcher.send(ev);

        // let log = AccessLogger::new(self.log.clone(), &req, "geobase".to_owned(), "ip".to_owned(), 0);
        // let future = rx.and_then(move |(mut res, bytes_sent)| {
        //     res.headers_mut().set_raw("X-Powered-By", "Cocaine");
        //     log.commit(res.status().into(), bytes_sent, None);
        //     Ok(res)
        // }).map_err(|err| hyper::Error::Io(io::Error::new(ErrorKind::Other, format!("{}", err))));

        // Match::Some(Box::new(future))
    }
}

pub struct SingleChunkReadDispatch {
    tx: oneshot::Sender<(Response, u64)>,
}

impl Dispatch for SingleChunkReadDispatch {
    fn process(self: Box<Self>, response: &cocaine::Response) -> Option<Box<dyn Dispatch>> {
        let (code, body) = match response.deserialize::<Primitive<i64>>().flatten() {
            Ok(v) => (StatusCode::Ok, format!("[{}]", v)),
            Err(err) => (StatusCode::InternalServerError, format!("{:?}", err)),
        };

        let body_len = body.as_bytes().len() as u64;

        let res = Response::new()
            .with_status(code)
            .with_header(ContentLength(body_len))
            .with_body(body);

        drop(self.tx.send((res, body_len)));

        None
    }

    fn discard(self: Box<Self>, err: &Error) {
        let body = format!("{}", err);
        let body_len = body.as_bytes().len() as u64;

        let mut res = Response::new();
        res.set_status(StatusCode::InternalServerError);
        res.set_body(body);

        drop(self.tx.send((res, body_len)));
    }
}
