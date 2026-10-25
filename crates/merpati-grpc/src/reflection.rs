//! Schema discovery through gRPC server reflection (`grpc.reflection.v1`,
//! falling back to `v1alpha`; both share the same wire format).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use prost::Message as _;
use prost_reflect::DescriptorPool;
use prost_types::FileDescriptorProto;
use protox::file::FileResolver as _;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::codec::Streaming;
use tonic::metadata::MetadataMap;
use tonic::transport::Channel;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
use tonic_reflection::pb::v1::{ServerReflectionRequest, ServerReflectionResponse};

use crate::error::{GrpcError, error_chain, status_code_name, transport_failure};
use crate::schema::GrpcSchema;
use crate::target::{Target, build_metadata, parse_address};
use crate::transport::{TlsOptions, client_cert_hint, connect, format_duration};

const V1_PATH: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
const V1ALPHA_PATH: &str = "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";
const REFLECTION_SERVICES: [&str; 2] =
    ["grpc.reflection.v1.ServerReflection", "grpc.reflection.v1alpha.ServerReflection"];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

enum ReflectError {
    /// This reflection version is not served.
    Unimplemented,
    /// The server answered a request with an `ErrorResponse`.
    Answer(String),
    Fatal(GrpcError),
}

impl From<GrpcError> for ReflectError {
    fn from(e: GrpcError) -> Self {
        ReflectError::Fatal(e)
    }
}

fn status_to_error(status: tonic::Status, tls: bool) -> ReflectError {
    if status.code() == tonic::Code::Unimplemented {
        return ReflectError::Unimplemented;
    }
    match std::error::Error::source(&status) {
        Some(src) => ReflectError::Fatal(GrpcError::Connect(transport_failure(src, tls))),
        None => {
            let code = status.code() as i32;
            ReflectError::Fatal(GrpcError::Reflection(format!(
                "{} ({code}): {}",
                status_code_name(code),
                status.message()
            )))
        }
    }
}

struct Session {
    tx: mpsc::UnboundedSender<ServerReflectionRequest>,
    stream: Streaming<ServerReflectionResponse>,
    tls: bool,
}

impl Session {
    /// Opens the reflection stream and lists the services.
    async fn start(
        channel: Channel,
        path: &'static str,
        metadata: &MetadataMap,
        tls: bool,
    ) -> Result<(Session, Vec<String>), ReflectError> {
        let (tx, rx) = mpsc::unbounded_channel();
        // Queue the first request before waiting for response headers: some
        // servers only send headers along with the first response.
        let _ = tx.send(request(MessageRequest::ListServices(String::new())));
        let mut req = tonic::Request::new(UnboundedReceiverStream::new(rx));
        *req.metadata_mut() = metadata.clone();
        let mut grpc = tonic::client::Grpc::new(channel).max_decoding_message_size(64 * 1024 * 1024);
        grpc.ready().await.map_err(|e| GrpcError::Connect(error_chain(&e)))?;
        let codec = tonic_prost::ProstCodec::<ServerReflectionRequest, ServerReflectionResponse>::default();
        let path = http::uri::PathAndQuery::from_static(path);
        let stream = grpc.streaming(req, path, codec).await.map_err(|s| status_to_error(s, tls))?.into_inner();
        let mut session = Session { tx, stream, tls };
        let services = match session.next().await? {
            MessageResponse::ListServicesResponse(list) => list.service.into_iter().map(|s| s.name).collect(),
            _ => return Err(GrpcError::Reflection("unexpected answer to list_services".into()).into()),
        };
        Ok((session, services))
    }

    async fn ask(&mut self, req: MessageRequest) -> Result<MessageResponse, ReflectError> {
        self.tx.send(request(req)).map_err(|_| GrpcError::Reflection("the reflection stream closed".into()))?;
        self.next().await
    }

    async fn next(&mut self) -> Result<MessageResponse, ReflectError> {
        match self.stream.message().await {
            Ok(Some(resp)) => match resp.message_response {
                Some(MessageResponse::ErrorResponse(e)) => Err(ReflectError::Answer(format!(
                    "{} ({}): {}",
                    status_code_name(e.error_code),
                    e.error_code,
                    e.error_message
                ))),
                Some(other) => Ok(other),
                None => Err(GrpcError::Reflection("empty reflection response".into()).into()),
            },
            Ok(None) => Err(GrpcError::Reflection("the server closed the reflection stream".into()).into()),
            Err(status) => Err(status_to_error(status, self.tls)),
        }
    }
}

fn request(req: MessageRequest) -> ServerReflectionRequest {
    ServerReflectionRequest { host: String::new(), message_request: Some(req) }
}

/// Loads every service the server advertises (except the reflection
/// service itself), plus all files they depend on.
pub async fn load_via_reflection(target: &Target) -> Result<GrpcSchema, GrpcError> {
    let addr = parse_address(&target.url, target.tls)?;
    let metadata = build_metadata(&target.metadata)?;
    let timeout = target.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let tls = TlsOptions::for_target(target, &addr)?;
    let work = async {
        let channel = connect(&addr, &tls, timeout).await?;
        match reflect(channel.clone(), V1_PATH, &metadata, addr.tls).await {
            Err(ReflectError::Unimplemented) => match reflect(channel, V1ALPHA_PATH, &metadata, addr.tls).await {
                Err(ReflectError::Unimplemented) => Err(GrpcError::Reflection(format!(
                    "{} does not support server reflection (tried grpc.reflection.v1 and v1alpha); load .proto files instead",
                    addr.authority
                ))),
                Err(ReflectError::Answer(m)) => Err(GrpcError::Reflection(m)),
                Err(ReflectError::Fatal(e)) => Err(e),
                Ok(s) => Ok(s),
            },
            Err(ReflectError::Answer(m)) => Err(GrpcError::Reflection(m)),
            Err(ReflectError::Fatal(e)) => Err(e),
            Ok(s) => Ok(s),
        }
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(Err(GrpcError::Connect(mut msg))) if addr.tls => {
            client_cert_hint(&mut msg, &addr, &tls);
            Err(GrpcError::Connect(msg))
        }
        Ok(r) => r,
        Err(_) => Err(GrpcError::Timeout(format!("Server reflection timed out after {}", format_duration(timeout)))),
    }
}

async fn reflect(
    channel: Channel,
    path: &'static str,
    metadata: &MetadataMap,
    tls: bool,
) -> Result<GrpcSchema, ReflectError> {
    let (mut session, services) = Session::start(channel, path, metadata, tls).await?;
    let mut files: BTreeMap<String, FileDescriptorProto> = BTreeMap::new();
    let mut warnings = Vec::new();
    let wanted: Vec<String> = services.into_iter().filter(|s| !REFLECTION_SERVICES.contains(&s.as_str())).collect();
    if wanted.is_empty() {
        warnings.push("the server lists no services besides reflection".to_string());
    }
    for service in &wanted {
        if defines_service(&files, service) {
            continue;
        }
        match session.ask(MessageRequest::FileContainingSymbol(service.clone())).await {
            Ok(MessageResponse::FileDescriptorResponse(r)) => add_files(&mut files, &r.file_descriptor_proto)?,
            Ok(_) => warnings.push(format!("{service}: unexpected reflection answer")),
            Err(ReflectError::Answer(m)) => warnings.push(format!("{service}: {m}")),
            Err(e) => return Err(e),
        }
    }
    if files.is_empty() && !wanted.is_empty() {
        return Err(
            GrpcError::Reflection(format!("no service definition could be fetched: {}", warnings.join("; "))).into()
        );
    }
    // Fetch dependencies the server did not send along.
    loop {
        let missing: BTreeSet<String> = files
            .values()
            .flat_map(|f| f.dependency.iter())
            .filter(|d| !files.contains_key(d.as_str()))
            .cloned()
            .collect();
        if missing.is_empty() {
            break;
        }
        for name in missing {
            if files.contains_key(&name) {
                continue;
            }
            match session.ask(MessageRequest::FileByFilename(name.clone())).await {
                Ok(MessageResponse::FileDescriptorResponse(r)) => add_files(&mut files, &r.file_descriptor_proto)?,
                Ok(_) | Err(ReflectError::Answer(_)) => {}
                Err(e) => return Err(e),
            }
            if !files.contains_key(&name) {
                // Well-known types are bundled with protox.
                match protox::file::GoogleFileResolver::new().open_file(&name) {
                    Ok(f) => {
                        files.insert(name.clone(), f.file_descriptor_proto().clone());
                    }
                    Err(_) => {
                        return Err(GrpcError::Reflection(format!(
                            "the server did not provide the imported file \"{name}\""
                        ))
                        .into());
                    }
                }
            }
        }
    }
    drop(session);
    let mut pool = DescriptorPool::new();
    pool.add_file_descriptor_protos(files.into_values()).map_err(|e| GrpcError::Descriptor(e.to_string()))?;
    Ok(GrpcSchema::from_pool(pool).with_warnings(warnings))
}

fn add_files(files: &mut BTreeMap<String, FileDescriptorProto>, encoded: &[Vec<u8>]) -> Result<(), ReflectError> {
    for bytes in encoded {
        let file = FileDescriptorProto::decode(bytes.as_slice())
            .map_err(|e| GrpcError::Reflection(format!("invalid file descriptor from server: {e}")))?;
        files.entry(file.name().to_string()).or_insert(file);
    }
    Ok(())
}

fn defines_service(files: &BTreeMap<String, FileDescriptorProto>, full_name: &str) -> bool {
    files.values().any(|f| {
        f.service.iter().any(|s| {
            if f.package().is_empty() {
                s.name() == full_name
            } else {
                format!("{}.{}", f.package(), s.name()) == full_name
            }
        })
    })
}
