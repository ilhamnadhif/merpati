//! Core of Merpati: the data model shared by every other crate, variable
//! resolution, the on-disk workspace format, and Postman format support.
//!
//! Nothing in this crate performs network I/O. Protocol clients live in
//! `merpati-http`, auth signing in `merpati-auth`, and the JavaScript engine in
//! `merpati-script`; they all speak in terms of the types defined here.

pub mod auth;
pub mod dynamic;
pub mod environment;
pub mod error;
pub mod fork;
pub mod history;
pub mod jsonpath;
pub mod model;
pub mod postman;
pub mod prepared;
pub mod protocols;
pub mod response;
pub mod script;
pub mod secrets;
pub mod settings;
pub mod storage;
pub mod tokens;
pub mod urlutil;
pub mod util;
pub mod variables;

pub use auth::{Auth, AuthType};
pub use environment::Environment;
pub use error::{Error, Result};
pub use model::*;
pub use prepared::{MultipartPart, MultipartValue, PreparedBody, PreparedRequest};
pub use response::{HttpResponse, ResponseBody};
pub use variables::{Resolver, ScopeTypes, VarKind, VarType, Variable, VariableScopes};
