cargo_component_bindings::generate!();

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Headers, IncomingBody, IncomingRequest, InputStream, OutgoingBody, OutgoingResponse,
    ResponseOutparam,
};
use bindings::wasi::io;
use bindings::wasi::io::streams::{OutputStream, StreamError};

use futures::{stream, AsyncWriteExt, Stream, StreamExt};

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

const READ_SIZE: u64 = 16 * 1024;

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        run(handle(request, response_out)).unwrap();
    }
}

async fn handle(request: IncomingRequest, response_out: ResponseOutparam) -> anyhow::Result<()> {
    // Extend incoming body with an "!"
    let mut incoming_body = collect_body(request).await?;
    incoming_body.extend(b"!");

    // Set the response
    let response = OutgoingResponse::new(
        Headers::from_list(&[("content-type".to_string(), b"text/plain".to_vec())]).expect("TODO"),
    );
    let mut body = outgoing_body(response.body().expect("response body was already taken"));
    ResponseOutparam::set(response_out, Ok(response));
    body.write_all(&incoming_body).await?;

    Ok(())
}

async fn collect_body(request: IncomingRequest) -> anyhow::Result<Vec<u8>> {
    let mut body = incoming_body(request.consume().expect("TODO"));
    let mut b = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        b.extend(&chunk);
    }
    Ok(b)
}

static WAKERS: Mutex<Vec<(io::poll::Pollable, Waker)>> = Mutex::new(Vec::new());

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

fn outgoing_body(body: OutgoingBody) -> impl futures::AsyncWrite {
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
                Ok(()) => {
                    WAKERS
                        .lock()
                        .unwrap()
                        .push((stream.subscribe(), cx.waker().clone()));

                    Poll::Pending
                }
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

fn incoming_body(body: IncomingBody) -> impl Stream<Item = Result<Vec<u8>, io::streams::Error>> {
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
