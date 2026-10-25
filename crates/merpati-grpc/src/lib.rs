//! Dynamic gRPC client for Merpati (PRD FR-X2).
//!
//! * Schemas come from `.proto` files compiled in-process with `protox`
//!   ([`load_proto_files`]) or from gRPC server reflection
//!   ([`load_via_reflection`], v1 with v1alpha fallback).
//! * [`GrpcSchema`] lists services and methods and produces example request
//!   JSON.
//! * [`GrpcCall`] runs unary, client-streaming, server-streaming and
//!   bidirectional calls on a tokio runtime and reports [`GrpcEvent`]s;
//!   [`invoke`] / [`invoke_unary`] are the collecting async variants.
//!
//! Messages are protobuf-JSON (the canonical proto3 JSON mapping). TLS is
//! rustls with the ring provider; everything is pure Rust.

mod call;
mod codec;
mod error;
mod reflection;
mod schema;
mod target;
mod transport;

pub use call::{CallResult, Direction, GrpcCall, GrpcEvent, UnaryResponse, invoke, invoke_unary};
pub use error::{GrpcError, status_code_name};
pub use prost_reflect;
pub use reflection::load_via_reflection;
pub use schema::{GrpcSchema, MethodInfo, MethodKind, ServiceInfo, load_proto_files};
pub use target::{Target, auth_metadata};
