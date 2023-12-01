cargo_component_bindings::generate!();

use anyhow::{anyhow, Context as _};
use async_compression::futures::write::GzipEncoder;
use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::outgoing_handler as downstream;
use bindings::wasi::http::types::{
    ErrorCode, FutureIncomingResponse, IncomingBody, IncomingRequest, IncomingResponse,
    OutgoingBody, OutgoingRequest, OutgoingResponse, ResponseOutparam, Scheme,
};
use bindings::wasi::io;
use bindings::wasi::io::streams::{InputStream, OutputStream, StreamError};
use futures::{stream, AsyncWriteExt, Stream, StreamExt};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        run(handle(request, response_out)).unwrap();
    }
}

async fn handle(request: IncomingRequest, response_out: ResponseOutparam) -> anyhow::Result<()> {
    let mut incoming_body = incoming_body_stream(request.consume().expect("TODO"));
    let (request, mut outgoing_body) = {
        let new = OutgoingRequest::new(request.headers());
        new.set_path_with_query(request.path_with_query().as_deref())
            .map_err(|_| anyhow!("failed to set request path and query"))?;
        new.set_scheme(Some(&Scheme::Http))
            .map_err(|_| anyhow!("failed to set request scheme"))?;
        new.set_authority(request.authority().as_deref())
            .map_err(|_| anyhow!("failed to set request authority"))?;
        let mut body = outgoing_body_write(new.body().expect("TODO"));
        (new, body)
    };
    while let Some(chunk) = incoming_body.next().await {
        let chunk = chunk.map_err(|e| anyhow!("failed to get incoming body chunk {e:?}"))?;
        outgoing_body.write_all(&chunk).await;
    }
    // Drop the outgoing body so that the outgoing request can complete - otherwise we deadlock
    drop(outgoing_body);
    let response = incoming_response(downstream::handle(request, None)?).await?;
    let mut downstream_body = incoming_body_stream(
        response
            .consume()
            .expect("downstream body was unexpectedly consumed before"),
    );

    let response = {
        let new = OutgoingResponse::new(response.headers());
        new.set_status_code(response.status())
            .map_err(|_| anyhow!("failed to set response status"))?;
        new
    };
    let outgoing_body = outgoing_body_write(
        response
            .body()
            .expect("response body was unexpected consumed before"),
    );
    ResponseOutparam::set(response_out, Ok(response));
    let mut encoder = GzipEncoder::new(outgoing_body);
    while let Some(chunk) = downstream_body.next().await {
        let chunk = chunk.map_err(|e| anyhow!("failed to get downstream body chunk {e:?}"))?;
        encoder.write_all(&chunk).await?;
    }
    encoder.flush().await?;
    Ok(())
}

const READ_SIZE: u64 = 16 * 1024;

static WAKERS: Mutex<Vec<(io::poll::Pollable, Waker)>> = Mutex::new(Vec::new());

fn incoming_body_stream(
    body: IncomingBody,
) -> impl Stream<Item = Result<Vec<u8>, io::streams::Error>> {
    struct Incoming(Option<(InputStream, IncomingBody)>);

    impl Drop for Incoming {
        fn drop(&mut self) {
            if let Some((stream, body)) = self.0.take() {
                drop(stream);
                IncomingBody::finish(body);
            }
        }
    }

    stream::poll_fn({
        let stream = body.stream().expect("response body should be readable");
        let pair = Incoming(Some((stream, body)));

        move |context| {
            if let Some((stream, _)) = &pair.0 {
                match stream.read(READ_SIZE) {
                    Ok(buffer) => {
                        if buffer.is_empty() {
                            WAKERS
                                .lock()
                                .unwrap()
                                .push((stream.subscribe(), context.waker().clone()));
                            Poll::Pending
                        } else {
                            Poll::Ready(Some(Ok(buffer)))
                        }
                    }
                    Err(StreamError::Closed) => Poll::Ready(None),
                    Err(StreamError::LastOperationFailed(error)) => Poll::Ready(Some(Err(error))),
                }
            } else {
                Poll::Ready(None)
            }
        }
    })
}

fn incoming_response(
    response: FutureIncomingResponse,
) -> impl std::future::Future<Output = Result<IncomingResponse, ErrorCode>> {
    futures::future::poll_fn({
        move |context| {
            if let Some(response) = response.get() {
                Poll::Ready(response.unwrap())
            } else {
                WAKERS
                    .lock()
                    .unwrap()
                    .push((response.subscribe(), context.waker().clone()));
                Poll::Pending
            }
        }
    })
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    futures::pin_mut!(future);
    struct DummyWaker;

    impl Wake for DummyWaker {
        fn wake(self: Arc<Self>) {}
    }

    let waker = Arc::new(DummyWaker).into();

    loop {
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Pending => {
                let mut new_wakers = Vec::new();

                let wakers = core::mem::take::<Vec<_>>(&mut WAKERS.lock().unwrap());

                assert!(!wakers.is_empty());

                let pollables = wakers
                    .iter()
                    .map(|(pollable, _)| pollable)
                    .collect::<Vec<_>>();

                let mut ready = vec![false; wakers.len()];

                for index in io::poll::poll(&pollables) {
                    ready[usize::try_from(index).unwrap()] = true;
                }

                for (ready, (pollable, waker)) in ready.into_iter().zip(wakers) {
                    if ready {
                        waker.wake()
                    } else {
                        new_wakers.push((pollable, waker));
                    }
                }

                *WAKERS.lock().unwrap() = new_wakers;
            }
            Poll::Ready(result) => break result,
        }
    }
}

fn outgoing_body_write(body: OutgoingBody) -> impl futures::AsyncWrite {
    struct Outgoing {
        payload: Option<(OutputStream, OutgoingBody)>,
    }

    impl Drop for Outgoing {
        fn drop(&mut self) {
            if let Some((stream, body)) = self.payload.take() {
                drop(stream);
                OutgoingBody::finish(body, None).expect("TODO");
            }
        }
    }

    let stream = body.write().expect("response body should be writable");
    let outgoing = Outgoing {
        payload: Some((stream, body)),
    };

    impl futures::AsyncWrite for Outgoing {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            let (stream, _) = &this.payload.as_ref().unwrap();
            match stream.check_write() {
                Ok(0) => {
                    WAKERS
                        .lock()
                        .unwrap()
                        .push((stream.subscribe(), cx.waker().clone()));

                    Poll::Pending
                }
                Ok(count) => {
                    let count = usize::try_from(count).unwrap().min(buf.len());

                    match stream.write(&buf[..count]) {
                        Ok(()) => Poll::Ready(Ok(count)),
                        Err(e) => {
                            Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, e)))
                        }
                    }
                }
                Err(e) => Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, e))),
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let (stream, _) = this.payload.as_ref().unwrap();
            match stream.flush() {
                Ok(()) => Poll::Ready(Ok(())),
                Err(StreamError::Closed) => Poll::Ready(Ok(())),
                Err(StreamError::LastOperationFailed(e)) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("{e:?}"),
                ))),
            }
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    outgoing
}
