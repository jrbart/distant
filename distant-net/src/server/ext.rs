use crate::{
    GenericServerRef, Listener, Request, Response, Server, ServerConnection, ServerCtx, ServerRef,
    ServerReply, ServerState, TypedAsyncRead, TypedAsyncWrite,
};
use log::*;
use serde::{de::DeserializeOwned, Serialize};
use std::{io, sync::Arc};
use tokio::sync::mpsc;

mod tcp;
pub use tcp::*;

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::*;

/// Extension trait to provide a reference implementation of starting a server
/// that will listen for new connections (exposed as [`TypedAsyncWrite`] and [`TypedAsyncRead`])
/// and process them using the [`Server`] implementation
pub trait ServerExt {
    type Request;
    type Response;

    /// Start a new server using the provided listener
    fn start<L, R, W>(self, listener: L) -> io::Result<Box<dyn ServerRef>>
    where
        L: Listener<Output = (W, R)> + 'static,
        R: TypedAsyncRead<Request<Self::Request>> + Send + 'static,
        W: TypedAsyncWrite<Response<Self::Response>> + Send + 'static;
}

impl<S, Req, Res, Data> ServerExt for S
where
    S: Server<Request = Req, Response = Res, LocalData = Data> + Sync + 'static,
    Req: DeserializeOwned + Send + Sync + 'static,
    Res: Serialize + Send + 'static,
    Data: Default + Send + Sync + 'static,
{
    type Request = Req;
    type Response = Res;

    fn start<L, R, W>(self, listener: L) -> io::Result<Box<dyn ServerRef>>
    where
        L: Listener<Output = (W, R)> + 'static,
        R: TypedAsyncRead<Request<Self::Request>> + Send + 'static,
        W: TypedAsyncWrite<Response<Self::Response>> + Send + 'static,
    {
        let server = Arc::new(self);
        let state = Arc::new(ServerState::new());

        let task = tokio::spawn(task(server, Arc::clone(&state), listener));

        Ok(Box::new(GenericServerRef { state, task }))
    }
}

async fn task<S, Req, Res, Data, L, R, W>(server: Arc<S>, state: Arc<ServerState>, mut listener: L)
where
    S: Server<Request = Req, Response = Res, LocalData = Data> + Sync + 'static,
    Req: DeserializeOwned + Send + Sync + 'static,
    Res: Serialize + Send + 'static,
    Data: Default + Send + Sync + 'static,
    L: Listener<Output = (W, R)> + 'static,
    R: TypedAsyncRead<Request<Req>> + Send + 'static,
    W: TypedAsyncWrite<Response<Res>> + Send + 'static,
{
    loop {
        let server = Arc::clone(&server);
        match listener.accept().await {
            Ok((mut writer, mut reader)) => {
                let mut connection = ServerConnection::new();
                let connection_id = connection.id;
                let state = Arc::clone(&state);

                // Create some default data for the new connection and pass it
                // to the callback prior to processing new requests
                let local_data = {
                    let mut data = Data::default();
                    server.on_accept(&mut data).await;
                    Arc::new(data)
                };

                // Start a writer task that reads from a channel and forwards all
                // data through the writer
                let (tx, mut rx) = mpsc::channel::<Response<Res>>(1);
                connection.writer_task = Some(tokio::spawn(async move {
                    while let Some(data) = rx.recv().await {
                        // trace!("[Conn {}] Sending {:?}", connection_id, data.payload);
                        if let Err(x) = writer.write(data).await {
                            error!("[Conn {}] Failed to send {:?}", connection_id, x);
                            break;
                        }
                    }
                }));

                // Start a reader task that reads requests and processes them
                // using the provided handler
                connection.reader_task = Some(tokio::spawn(async move {
                    loop {
                        match reader.read().await {
                            Ok(Some(request)) => {
                                let reply = ServerReply {
                                    origin_id: request.id.clone(),
                                    tx: tx.clone(),
                                };

                                let ctx = ServerCtx {
                                    connection_id,
                                    request,
                                    reply: reply.clone(),
                                    local_data: Arc::clone(&local_data),
                                };

                                server.on_request(ctx).await;
                            }
                            Ok(None) => {
                                debug!("[Conn {}] Connection closed", connection_id);
                                break;
                            }
                            Err(x) => {
                                // NOTE: We do NOT break out of the loop, as this could happen
                                //       if someone sends bad data at any point, but does not
                                //       mean that the reader itself has failed. This can
                                //       happen from getting non-compliant typed data
                                error!("[Conn {}] {}", connection_id, x);
                            }
                        }
                    }
                }));

                state
                    .connections
                    .write()
                    .await
                    .insert(connection_id, connection);
            }
            Err(x) => {
                error!("Server no longer accepting connections: {}", x);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IntoSplit, MpscListener, MpscTransport};
    use async_trait::async_trait;

    pub struct TestServer;

    #[async_trait]
    impl Server for TestServer {
        type Request = u16;
        type Response = String;
        type LocalData = ();

        async fn on_request(&self, ctx: ServerCtx<Self::Request, Self::Response, Self::LocalData>) {
            // Always send back "hello"
            ctx.reply.send("hello".to_string()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn should_invoke_handler_upon_receiving_a_request() {
        // Create a test listener where we will forward a connection
        let (tx, listener) = MpscListener::channel(100);

        // Make bounded transport pair and send off one of them to act as our connection
        let (mut transport, connection) =
            MpscTransport::<Request<u16>, Response<String>>::pair(100);
        tx.send(connection.into_split())
            .await
            .expect("Failed to feed listener a connection");

        let _server = ServerExt::start(TestServer, listener).expect("Failed to start server");

        transport
            .write(Request::new(123))
            .await
            .expect("Failed to send request");

        let response: Response<String> = transport.read().await.unwrap().unwrap();
        assert_eq!(response.payload, "hello");
    }
}