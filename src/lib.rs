use futures::prelude::*;

use std::collections::HashMap;
use std::pin::Pin;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::{HeaderMap, StatusCode, Uri};
use failure::Fail;
use mightybadger::payload::RequestInfo;
use pin_project::pin_project;

use futures::future::{self, Ready};
use futures::task::{Context, Poll};

#[derive(Debug)]
pub struct HoneybadgerMiddleware(());

impl HoneybadgerMiddleware {
    pub fn new() -> Self {
        HoneybadgerMiddleware(())
    }
}

#[derive(Debug, Fail)]
#[fail(display = "Unknown Error Response: {}", _0)]
pub struct ErrorStatus(StatusCode);

impl<S> Transform<S> for HoneybadgerMiddleware
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse, Error = actix_web::Error>,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse;
    type Error = actix_web::Error;
    type Transform = HoneybadgerHandler<S>;
    type InitError = ();
    type Future = Ready<Result<HoneybadgerHandler<S>, ()>>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ready(Ok(HoneybadgerHandler(service)))
    }
}

#[derive(Debug)]
pub struct HoneybadgerHandler<S>(S);

impl<S> Service for HoneybadgerHandler<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse, Error = actix_web::Error>,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse;
    type Error = actix_web::Error;
    type Future = HoneybadgerHandlerFuture<S::Future>;

    fn poll_ready(&mut self, _ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let uri = req.head().uri.clone();
        let headers = {
            let mut headers = HeaderMap::with_capacity(req.head().headers().len());
            for (name, value) in req.head().headers() {
                headers.append(name.clone(), value.clone());
            }
            headers
        };
        HoneybadgerHandlerFuture {
            inner: self.0.call(req),
            reporter: Reporter { uri, headers },
        }
    }
}

#[pin_project]
#[derive(Debug)]
pub struct HoneybadgerHandlerFuture<F> {
    #[pin]
    inner: F,
    reporter: Reporter,
}

impl<F> Future for HoneybadgerHandlerFuture<F>
where
    F: Future<Output = Result<ServiceResponse, actix_web::Error>>,
{
    type Output = Result<ServiceResponse, actix_web::Error>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(ctx) {
            Poll::Ready(Ok(resp)) => {
                this.reporter.report(Ok(&resp));
                Poll::Ready(Ok(resp))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                this.reporter.report(Err(&e));
                Poll::Ready(Err(e))
            }
        }
    }
}

#[derive(Debug)]
struct Reporter {
    uri: Uri,
    headers: HeaderMap,
}

impl Reporter {
    fn report(&self, resp: Result<&ServiceResponse, &actix_web::Error>) {
        let status = match resp {
            Ok(resp) => resp.status(),
            Err(e) => e.as_response_error().error_response().status(),
        };
        if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
            return;
        }
        let request_info = {
            let mut cgi_data: HashMap<String, String> = HashMap::new();
            for (name, value) in self.headers.iter() {
                let name = "HTTP_"
                    .chars()
                    .chain(name.as_str().chars())
                    .map(|ch| {
                        if ch == '-' {
                            '_'
                        } else {
                            ch.to_ascii_uppercase()
                        }
                    })
                    .collect::<String>();
                cgi_data.insert(name, String::from_utf8_lossy(value.as_bytes()).to_string());
            }
            let url = format!("http://localhost/{}", self.uri.path());
            let params: HashMap<String, String> = self
                .uri
                .query()
                .and_then(|query| serde_urlencoded::from_str(query).ok())
                .unwrap_or_else(HashMap::new);
            RequestInfo {
                url: url,
                cgi_data: cgi_data,
                params: params,
                ..Default::default()
            }
        };
        mightybadger::context::with(&request_info, || {
            if let Err(error) = resp {
                mightybadger::notify_std_error(error);
            } else {
                let error = ErrorStatus(status);
                mightybadger::notify(&error);
            }
        });
    }
}
