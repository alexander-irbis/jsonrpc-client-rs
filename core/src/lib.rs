// Copyright 2017 Amagicom AB.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


//! A crate for generating transport agnostic, auto serializing, strongly typed JSON-RPC 2.0
//! clients.
//!
//! This crate mainly provides a macro, `jsonrpc_client`. The macro generates structs that can be
//! used for calling JSON-RPC 2.0 APIs. The macro lets you list methods on the struct with
//! arguments and a return type. The macro then generates a struct which will automatically
//! serialize the arguments, send the request and deserialize the response into the target type.
//!
//! # Transports
//!
//! The `jsonrpc-client-core` crate itself and the structs generated by the `jsonrpc_client` macro
//! are transport agnostic. They can use any type implementing the `Transport` trait.
//!
//! The main (and so far only) transport implementation is the Hyper based HTTP implementation
//! in the [`jsonrpc-client-http`](../jsonrpc_client_http/index.html) crate.
//!
//! # Example
//!
//! ```rust,ignore
//! #[macro_use]
//! extern crate jsonrpc_client_core;
//! extern crate jsonrpc_client_http;
//!
//! use jsonrpc_client_http::HttpTransport;
//!
//! jsonrpc_client!(pub struct FizzBuzzClient {
//!     /// Returns the fizz-buzz string for the given number.
//!     pub fn fizz_buzz(&mut self, number: u64) -> Future<String>;
//! });
//!
//! fn main() {
//!     let transport = HttpTransport::new().standalone().unwrap();
//!     let transport_handle = transport
//!         .handle("http://api.fizzbuzzexample.org/rpc/")
//!         .unwrap();
//!     let mut client = FizzBuzzClient::new(transport_handle);
//!     let result1 = client.fizz_buzz(3).wait().unwrap();
//!     let result2 = client.fizz_buzz(4).wait().unwrap();
//!     let result3 = client.fizz_buzz(5).wait().unwrap();
//!
//!     // Should print "fizz 4 buzz" if the server implemented the service correctly
//!     println!("{} {} {}", result1, result2, result3);
//! }
//! ```

#![deny(missing_docs)]

#[macro_use]
pub extern crate error_chain;
extern crate futures;
extern crate jsonrpc_client_utils;
extern crate jsonrpc_core;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde;
extern crate serde_json;

use futures::future;
use futures::sync::mpsc;
pub use futures::sync::oneshot;
pub use futures::Future;
use futures::{Async, AsyncSink};
use futures::{Sink, Stream};
use jsonrpc_core::types::{
    Failure as RpcFailure, Id, MethodCall, Notification, Output, Params, Request, Response,
    Success as RpcSuccess, Version,
};
use serde_json::Value as JsonValue;


use std::collections::HashMap;

/// Contains the main macro of this crate, `jsonrpc_client`.
#[macro_use]
mod macros;

mod id_generator;
use id_generator::IdGenerator;

use jsonrpc_client_utils::select_weak::{self, SelectWithWeakExt};

/// Module containing the _server_ part of the client, allowing the user to set callbacks for
/// various method and notification requests coming in from the server. Does not work with HTTP.
pub mod server;

/// Module containing an example client. To show in the docs what a generated struct look like.
pub mod example;

error_chain! {
    errors {
        /// Error in the underlying transport layer.
        TransportError {
            description("Unable to send the JSON-RPC 2.0 request")
        }
        /// Error while serializing method parameters.
        SerializeError {
            description("Unable to serialize the method parameters")
        }
        /// Error when deserializing server response
        DeserializeError {
            description("Unable to deserialize response")
        }
        /// Error while deserializing or parsing the response data.
        ResponseError(msg: &'static str) {
            description("Unable to deserialize the response into the desired type")
            display("Unable to deserialize the response: {}", msg)
        }
        /// The server returned a response with an incorrect version
        InvalidVersion {
            description("Method call returned a response that was not specified as JSON-RPC 2.0")
        }
        /// Error when trying to send a new message to the server because the client is already
        /// shut down.
        Shutdown {
            description("RPC Client already shut down")
        }
        /// The request was replied to, but with a JSON-RPC 2.0 error.
        JsonRpcError(error: jsonrpc_core::Error) {
            description("Method call returned JSON-RPC 2.0 error")
            display("JSON-RPC 2.0 Error: {} ({})", error.code.description(), error.message)
        }
    }
}


/// This handle allows one to create futures for RPC invocations. For the requests to ever be
/// resolved, the Client future has to be driven.
#[must_use]
#[derive(Debug, Clone)]
pub struct ClientHandle {
    client_handle_tx: mpsc::Sender<OutgoingMessage>,
}

impl ClientHandle {
    /// Invokes an RPC and creates a future representing the RPC's result.
    pub fn call_method<T>(
        &self,
        method: impl Into<String> + 'static,
        parameters: &impl serde::Serialize,
    ) -> impl Future<Item = T, Error = Error> + 'static
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let client = self.clone();

        future::result(serialize_parameters(parameters)).and_then(move |params| {
            client.send_client_call(Ok(OutgoingMessage::RpcCall(method.into(), params, tx)), rx)
        })
    }

    /// Send arbitrary RPC call to Client. Primarily intended to be used from macro
    /// `jsonrpc_client!`.
    #[doc(hidden)]
    pub fn send_client_call<T: serde::de::DeserializeOwned + Send + Sized>(
        &self,
        client_call: Result<OutgoingMessage>,
        rx: oneshot::Receiver<Result<JsonValue>>,
    ) -> impl Future<Item = T, Error = Error> {
        let rpc_chan = self.client_handle_tx.clone();

        future::result(client_call)
            .and_then(|call| rpc_chan.send(call).map_err(|_| ErrorKind::Shutdown.into()))
            .and_then(|_| rx.map_err(|_| ErrorKind::Shutdown).flatten())
            .and_then(|r| serde_json::from_value(r).chain_err(|| ErrorKind::DeserializeError))
    }


    /// Sends a notificaiton to the Server.
    pub fn send_notification(
        &self,
        method: String,
        parameters: &impl serde::Serialize,
    ) -> impl Future<Item = (), Error = Error> {
        let (tx, rx) = oneshot::channel();

        let rpc_chan = self.client_handle_tx.clone();

        future::result(serialize_parameters(parameters))
            .and_then(|params| {
                rpc_chan
                    .send(OutgoingMessage::Notification(method, params, tx))
                    .map_err(|_| ErrorKind::Shutdown.into())
            }).and_then(|_| rx.map_err(|_| Error::from(ErrorKind::Shutdown)))
            .flatten()
    }
}


/// A Transport allows one to send and receive JSON objects to a JSON-RPC server.
pub trait Transport: Sized + Send{
    /// A transport specific error
    type Error: ::std::error::Error + Send + 'static;
    /// A stream of strings, each of which represent a single JSON value that is either an array or
    /// an object used to receive messages from a JSON-RPC server.
    type Stream: Stream<Item = String, Error = Self::Error> + Send;
    /// A sink of strings, each of which represent a single JSON value that is either an array or an
    /// object used to send messages to a JSON-RPC server.
    type Sink: Sink<SinkItem = String, SinkError = Self::Error> + Send;

    /// Transforms the transport implementation into a sink and a stream.
    fn io_pair(self) -> (Self::Sink, Self::Stream);

    /// Creates a Client and a ClientHandle from a transport implementation.
    fn into_client(self) -> (Client<Self, server::Server>, ClientHandle) {
        Client::new(self)
    }
}

/// A transport trait that should be implemented only for transports that support full duplex
/// communication between the client and the server. DuplexTransport implementors allow the user of
/// the library to specify a server handler.
pub trait DuplexTransport: Transport {
    /// Constructs a new client with the provided server handler.
    fn with_server<S: server::ServerHandler>(self, s: S) -> (Client<Self, S>, ClientHandle) {
        Client::new_with_server(self, s)
    }
}

/// Client is a future that takes an arbitrary transport sink and stream pair and handles JSON-RPC
/// 2.0 messages with a server. This future has to be driven for the messages to be passed around.
/// To send and receive messages, one should use the ClientHandle.
#[derive(Debug)]
#[must_use]
pub struct Client<T: Transport, S: server::ServerHandler> {
    // request channel, selecting between client calls from the client handle
    // and the server, when no more client handles exist, the stream will close down.
    outgoing_payload_rx: select_weak::SelectWithWeak<
        futures::sync::mpsc::Receiver<OutgoingMessage>,
        futures::sync::mpsc::Receiver<OutgoingMessage>,
    >,

    // state
    id_generator: IdGenerator,
    shutting_down: bool,
    pending_client_requests: HashMap<Id, oneshot::Sender<Result<JsonValue>>>,
    pending_payload: Option<String>,
    fatal_error: Option<Error>,

    server_handler: S,
    server_response_tx: mpsc::Sender<OutgoingMessage>,

    // transport
    transport_tx: T::Sink,
    transport_rx: T::Stream,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IncomingMessage {
    // take care, ordering here is important. Serde won't match a response struct if the request
    // comes first.
    Response(Output),
    Request(Request),
}

impl<T: Transport> Client<T, server::Server> {
    /// To create a new Client, one must provide a transport sink and stream pair. The transport
    /// sinks are expected to send and receive strings which should hold exactly one JSON
    /// object. If any error is returned by either the sink or the stream, this future will fail,
    /// and all pending requests will be dropped. If the transport stream finishes, this future
    /// will resolve without an error. The client will resolve once all of it's handles and
    /// corresponding futures get resolved.
    pub fn new(transport: T) -> (Self, ClientHandle) {
        let (server, _) = server::Server::new();
        Self::new_with_server(transport, server)
    }
}

impl<T: DuplexTransport, S: server::ServerHandler> Client<T, S> {
    /// Creates a new client from the provided transport and server implementations.
    pub fn with_server(transport: T, server: S) -> (Self, ClientHandle) {
        Self::new_with_server(transport, server)
    }
}

impl<T: Transport, S: server::ServerHandler> Client<T, S> {
    fn new_with_server(transport: T, server_handler: S) -> (Self, ClientHandle) {
        let (transport_tx, transport_rx) = transport.io_pair();
        let (client_handle_tx, client_handle_rx) = mpsc::channel(0);
        let (server_response_tx, server_response_rx) = mpsc::channel(0);

        let outgoing_payload_rx = client_handle_rx.select_with_weak(server_response_rx);


        (
            Client {
                // request channel
                outgoing_payload_rx,

                // state
                id_generator: IdGenerator::new(),
                pending_payload: None,
                shutting_down: false,
                fatal_error: None,
                pending_client_requests: HashMap::new(),

                // server handlers
                server_handler,
                server_response_tx,

                // transport
                transport_tx,
                transport_rx,
            },
            ClientHandle { client_handle_tx },
        )
    }

    fn should_shut_down(&mut self) -> bool {
        self.fatal_error.is_some() || self.shutting_down
    }

    /// Handles incoming RPC requests from handles, drains incoming responses from the transport
    /// stream and drives the transport sink.
    fn handle_messages(&mut self) -> Result<()> {
        // try send a leftover payload
        if let Some(payload) = self.pending_payload.take() {
            self.send_payload(payload)?;
        }
        // drive server futures
        self.poll_server()?;
        // drain incoming payload
        self.poll_transport_rx()?;
        // drain incoming rpc requests, only if the writing pipe is ready
        self.poll_outgoing_messages()?;
        // poll transport tx to drive sending
        self.poll_transport_tx()?;
        Ok(())
    }

    fn send_payload(&mut self, json_string: String) -> Result<()> {
        ensure!(self.fatal_error.is_none(), ErrorKind::TransportError);
        match self.transport_tx.start_send(json_string) {
            Ok(AsyncSink::Ready) => Ok(()),
            Ok(AsyncSink::NotReady(payload)) => {
                self.pending_payload = Some(payload);
                Ok(())
            }
            Err(e) => Err(e).chain_err(|| ErrorKind::TransportError),
        }
    }

    fn poll_transport_rx(&mut self) -> Result<()> {
        loop {
            match self
                .transport_rx
                .poll()
                .chain_err(|| ErrorKind::TransportError)?
            {
                Async::Ready(Some(new_payload)) => {
                    self.handle_transport_rx_payload(&new_payload)?;
                    continue;
                }
                Async::Ready(None) => {
                    trace!("transport receiver shut down, shutting down as well");
                    return Err(ErrorKind::Shutdown.into());
                }
                Async::NotReady => return Ok(()),
            }
        }
    }

    fn handle_transport_rx_payload(&mut self, payload: &str) -> Result<()> {
        let msg: IncomingMessage =
            serde_json::from_str(&payload).chain_err(|| ErrorKind::DeserializeError)?;
        match msg {
            IncomingMessage::Request(req) => self
                .server_handler
                .process_request(req, self.server_response_tx.clone()),
            IncomingMessage::Response(response) => self.handle_response(response),
        }
    }

    fn handle_response(&mut self, output: Output) -> Result<()> {
        if output.version() != Some(jsonrpc_core::types::Version::V2) {
            return Err(ErrorKind::InvalidVersion.into());
        };
        let (id, result): (Id, Result<JsonValue>) = match output {
            Output::Success(RpcSuccess { result, id, .. }) => (id, Ok(result)),
            Output::Failure(RpcFailure { id, error, .. }) => {
                (id, Err(ErrorKind::JsonRpcError(error).into()))
            }
        };

        match self.pending_client_requests.remove(&id) {
            Some(completion_chan) => Self::send_rpc_response(&id, completion_chan, result),
            None => trace!("Received response with an invalid id {:?}", id),
        };
        Ok(())
    }

    fn poll_outgoing_messages(&mut self) -> Result<()> {
        // Process new client payloads if the transport is ready to send new ones
        while self.pending_payload.is_none() {
            // There's no pending payload, so new RPC requests can be processed.
            match self.outgoing_payload_rx.poll() {
                Ok(Async::NotReady) => return Ok(()),
                Ok(Async::Ready(Some(call))) => {
                    self.handle_client_payload(call)?;
                }
                Ok(Async::Ready(None)) => {
                    trace!("All client handles and futures dropped, shutting down");
                    return Err(ErrorKind::Shutdown.into());
                }
                Err(_) => {
                    unreachable!("RPC channel returned an error, should never happen");
                }
            }
        }
        Ok(())
    }

    fn handle_client_payload(&mut self, message: OutgoingMessage) -> Result<()> {
        match message {
            OutgoingMessage::RpcCall(method, parameters, completion) => {
                let new_id = self.id_generator.next();
                match serialize_method_request(new_id.clone(), method, &parameters) {
                    Ok(payload) => {
                        self.add_new_call(new_id, completion);
                        self.send_payload(payload)?;
                    }
                    Err(e) => {
                        Self::send_rpc_response(&new_id, completion, Err(e));
                    }
                };
            }
            OutgoingMessage::Notification(method, parameters, completion) => {
                match serialize_notification_request(method, &parameters) {
                    Ok(payload) => {
                        if completion.send(Ok(())).is_err() {
                            trace!("future for notification dopped already");
                        }
                        self.send_payload(payload)?;
                    }
                    Err(e) => {
                        if completion.send(Err(e)).is_err() {
                            trace!("Future for notification already dropped");
                        }
                    }
                }
            }
            OutgoingMessage::Response(response) => {
                self.send_payload(
                    serde_json::to_string(&response).chain_err(|| ErrorKind::SerializeError)?,
                )?;
            }
        };
        Ok(())
    }

    fn poll_server(&mut self) -> Result<()> {
        if !self.shutting_down {
            self.shutting_down = match self.server_handler.poll()? {
                Async::NotReady => false,
                _ => true,
            };
        };
        Ok(())
    }

    fn send_rpc_response<V>(id: &Id, chan: oneshot::Sender<Result<V>>, value: Result<V>) {
        if chan.send(value).is_err() {
            trace!("Future for RPC call {:?} dropped already", id);
        }
    }

    fn handle_shutdown(&mut self) -> futures::Poll<(), Error> {
        if let Err(e) = self.poll_transport_rx() {
            trace!(
                "Failed to drain incoming messages from transport whilst shutting down: {}",
                e.description()
            );
        }
        match self
            .transport_tx
            .close()
            .chain_err(|| ErrorKind::TransportError)
        {
            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            }
            Err(e) => {
                warn!(
                    "Encountered error whilst shutting down client: {}",
                    e.description()
                );
            }
            _ => (),
        }

        self.fatal_error
            .take()
            .map(Err)
            .unwrap_or(Ok(Async::Ready(())))
    }

    fn add_new_call(&mut self, id: Id, completion: oneshot::Sender<Result<JsonValue>>) {
        self.pending_client_requests.insert(id, completion);
    }

    fn poll_transport_tx(&mut self) -> Result<()> {
        if self.fatal_error.is_none() {
            self.transport_tx
                .poll_complete()
                .chain_err(|| ErrorKind::TransportError)?;
        }
        Ok(())
    }
}

impl<T: Transport, S: server::ServerHandler> Future for Client<T, S> {
    type Item = ();
    type Error = Error;


    fn poll(&mut self) -> Result<Async<Self::Item>> {
        if !self.should_shut_down() {
            match self.handle_messages() {
                Ok(()) => return Ok(Async::NotReady),
                Err(Error(ErrorKind::Shutdown, _)) => self.shutting_down = true,
                Err(e) => self.fatal_error = Some(e),
            }
        }
        self.handle_shutdown()
    }
}


/// Outgoing message contains data to construct a complete object will be sent to the JSON-RPC 2.0
/// server. This can be a request, a notification or a response to a previously received request.
#[derive(Debug)]
pub enum OutgoingMessage {
    /// Invoke an RPC
    RpcCall(String, Option<Params>, oneshot::Sender<Result<JsonValue>>),
    /// Send a notification
    Notification(String, Option<Params>, oneshot::Sender<Result<()>>),
    /// Send a response response
    Response(Response),
}

/// Creates a JSON-RPC 2.0 request to the given method with the given parameters.
fn serialize_method_request(
    id: Id,
    method: String,
    params: &impl serde::Serialize,
) -> Result<String> {
    let serialized_params = serialize_parameters(params)?;
    let method_call = MethodCall {
        jsonrpc: Some(Version::V2),
        method,
        params: serialized_params,
        id,
    };
    serde_json::to_string(&method_call).chain_err(|| ErrorKind::SerializeError)
}

/// Serializes parameters for JSON-RPC 2.0 methods and notifications
pub fn serialize_parameters(params: &impl serde::Serialize) -> Result<Option<Params>> {
    let parameters = match serde_json::to_value(params).chain_err(|| ErrorKind::SerializeError)? {
        JsonValue::Null => None,
        JsonValue::Array(vec) => Some(Params::Array(vec)),
        JsonValue::Object(obj) => Some(Params::Map(obj)),
        value => Some(Params::Array(vec![value])),
    };
    Ok(parameters)
}

/// Creates a JSON-RPC 2.0 notification request to the given method with the given parameters.
fn serialize_notification_request(
    method: String,
    params: &impl serde::Serialize,
) -> Result<String> {
    let serialized_params = serialize_parameters(params)?;
    let notification = Notification {
        jsonrpc: Some(Version::V2),
        method,
        params: serialized_params,
    };
    serde_json::to_string(&notification).chain_err(|| ErrorKind::SerializeError)
}
