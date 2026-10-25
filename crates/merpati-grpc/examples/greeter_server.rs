//! The Greeter test server (with v1 reflection), for trying the client by
//! hand: `cargo run -p merpati-grpc --example greeter_server`.

#[path = "../tests/common/mod.rs"]
mod common;

#[tokio::main]
async fn main() {
    let server = common::spawn(common::Reflection::V1, None).await;
    println!("{}", server.url());
    let _ = tokio::signal::ctrl_c().await;
}
