//! A tonic test server for `tests/protos/helloworld/greeter.proto`, written
//! the way generated code would be (hand-written prost structs, a manual
//! service router), with optional server reflection and TLS.

#![allow(dead_code)]

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::StreamExt;
use tonic::metadata::{BinaryMetadataKey, KeyAndValueRef, MetadataKey, MetadataMap};
use tonic::{Request, Response, Status, Streaming};
use tonic_prost::ProstCodec;

pub fn protos_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("protos")
}

/// Client-certificate fixtures (copied from merpati-http's
/// `tests/fixtures/certs`, see its `generate.sh`; password "merpati").
pub fn cert_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("certs").join(name)
}

// Only the fields the server looks at; prost skips the others.
#[derive(Clone, PartialEq, prost::Message)]
pub struct HelloRequest {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(int32, tag = "2")]
    pub times: i32,
    #[prost(int32, tag = "3")]
    pub mood: i32,
    #[prost(message, optional, tag = "5")]
    pub address: Option<Address>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Address {
    #[prost(string, tag = "1")]
    pub city: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HelloReply {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(int32, tag = "2")]
    pub index: i32,
    #[prost(int32, tag = "3")]
    pub mood: i32,
}

/// Request metadata `x-*` comes back as response header `echo-x-*`.
fn echo_metadata(incoming: &MetadataMap) -> MetadataMap {
    let mut out = MetadataMap::new();
    for kv in incoming.iter() {
        match kv {
            KeyAndValueRef::Ascii(k, v) if k.as_str().starts_with("x-") => {
                let key: MetadataKey<_> = MetadataKey::from_bytes(format!("echo-{}", k.as_str()).as_bytes()).unwrap();
                out.insert(key, v.clone());
            }
            KeyAndValueRef::Binary(k, v) if k.as_str().starts_with("x-") => {
                let key = BinaryMetadataKey::from_bytes(format!("echo-{}", k.as_str()).as_bytes()).unwrap();
                out.insert_bin(key, v.clone());
            }
            _ => {}
        }
    }
    out
}

fn greeting(req: &HelloRequest) -> String {
    match req.address.as_ref().filter(|a| !a.city.is_empty()) {
        Some(a) => format!("Hello, {} from {}!", req.name, a.city),
        None => format!("Hello, {}!", req.name),
    }
}

async fn say_hello(req: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
    let echoed = echo_metadata(req.metadata());
    let msg = req.into_inner();
    match msg.name.as_str() {
        "missing" => {
            let mut md = MetadataMap::new();
            md.insert("x-reason", "unknown-person".parse().unwrap());
            return Err(Status::with_metadata(tonic::Code::NotFound, "no greeting for missing", md));
        }
        "slow" => tokio::time::sleep(Duration::from_secs(5)).await,
        _ => {}
    }
    let mut resp = Response::new(HelloReply { message: greeting(&msg), index: msg.times, mood: msg.mood });
    *resp.metadata_mut() = echoed;
    Ok(resp)
}

type ReplyStream = Pin<Box<dyn futures_util::Stream<Item = Result<HelloReply, Status>> + Send>>;

async fn lots_of_replies(req: Request<HelloRequest>) -> Result<Response<ReplyStream>, Status> {
    let msg = req.into_inner();
    let n = if msg.times > 0 { msg.times } else { 3 };
    let mut items: Vec<Result<HelloReply, Status>> = (1..=n)
        .map(|i| Ok(HelloReply { message: format!("Hello, {} #{i}", msg.name), index: i, mood: msg.mood }))
        .collect();
    if msg.name == "fail-midway" {
        items.truncate(1);
        items.push(Err(Status::resource_exhausted("quota used up")));
    }
    Ok(Response::new(Box::pin(tokio_stream::iter(items))))
}

async fn lots_of_greetings(req: Request<Streaming<HelloRequest>>) -> Result<Response<HelloReply>, Status> {
    let mut stream = req.into_inner();
    let mut names = Vec::new();
    while let Some(m) = stream.message().await? {
        names.push(m.name);
    }
    Ok(Response::new(HelloReply {
        message: format!("Hello, {}!", names.join(", ")),
        index: names.len() as i32,
        mood: 0,
    }))
}

async fn bidi_hello(req: Request<Streaming<HelloRequest>>) -> Result<Response<ReplyStream>, Status> {
    let mut index = 0;
    let stream = req.into_inner().map(move |r| {
        r.map(|m| {
            index += 1;
            HelloReply { message: greeting(&m), index, mood: m.mood }
        })
    });
    Ok(Response::new(Box::pin(stream)))
}

#[derive(Clone, Default)]
pub struct Greeter;

impl tonic::server::NamedService for Greeter {
    const NAME: &'static str = "helloworld.Greeter";
}

type BoxFuture = Pin<Box<dyn Future<Output = Result<http::Response<tonic::body::Body>, Infallible>> + Send>>;

impl tower::Service<http::Request<tonic::body::Body>> for Greeter {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<tonic::body::Body>) -> BoxFuture {
        let codec = ProstCodec::<HelloReply, HelloRequest>::default();
        let mut grpc = tonic::server::Grpc::new(codec);
        match req.uri().path() {
            "/helloworld.Greeter/SayHello" => {
                Box::pin(async move { Ok(grpc.unary(tower::service_fn(say_hello), req).await) })
            }
            "/helloworld.Greeter/LotsOfReplies" => {
                Box::pin(async move { Ok(grpc.server_streaming(tower::service_fn(lots_of_replies), req).await) })
            }
            "/helloworld.Greeter/LotsOfGreetings" => {
                Box::pin(async move { Ok(grpc.client_streaming(tower::service_fn(lots_of_greetings), req).await) })
            }
            "/helloworld.Greeter/BidiHello" => {
                Box::pin(async move { Ok(grpc.streaming(tower::service_fn(bidi_hello), req).await) })
            }
            _ => Box::pin(async move { Ok(Status::unimplemented("no such method").into_http()) }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reflection {
    None,
    V1,
    V1Alpha,
}

pub struct Server {
    pub addr: SocketAddr,
}

impl Server {
    pub fn url(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }
}

/// Starts the server on a random local port.
pub async fn spawn(reflection: Reflection, tls: Option<(String, String)>) -> Server {
    spawn_with(reflection, tls, None).await
}

/// Starts a TLS server that requires a client certificate issued by
/// `client_ca` (PEM).
pub async fn spawn_mtls(reflection: Reflection, tls: (String, String), client_ca: String) -> Server {
    spawn_with(reflection, Some(tls), Some(client_ca)).await
}

async fn spawn_with(reflection: Reflection, tls: Option<(String, String)>, client_ca: Option<String>) -> Server {
    merpati_http::tls::install_provider();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tonic::transport::server::TcpIncoming::from(listener);
    let fds = protox::compile(["helloworld/greeter.proto"], [protos_dir()]).unwrap();
    let builder = tonic_reflection::server::Builder::configure().register_file_descriptor_set(fds);
    let (v1, v1alpha) = match reflection {
        Reflection::None => (None, None),
        Reflection::V1 => (Some(builder.build_v1().unwrap()), None),
        Reflection::V1Alpha => (None, Some(builder.build_v1alpha().unwrap())),
    };
    let mut server = tonic::transport::Server::builder();
    if let Some((cert, key)) = tls {
        let mut config =
            tonic::transport::ServerTlsConfig::new().identity(tonic::transport::Identity::from_pem(cert, key));
        if let Some(ca) = client_ca {
            config = config.client_ca_root(tonic::transport::Certificate::from_pem(ca));
        }
        server = server.tls_config(config).unwrap();
    }
    let router = server.add_service(Greeter).add_optional_service(v1).add_optional_service(v1alpha);
    tokio::spawn(router.serve_with_incoming(incoming));
    Server { addr }
}

/// A self-signed certificate for `localhost` (cert PEM, key PEM).
pub fn self_signed() -> (String, String) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    (ck.cert.pem(), ck.signing_key.serialize_pem())
}
