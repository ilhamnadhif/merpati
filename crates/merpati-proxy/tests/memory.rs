//! Peak memory stays bounded while huge bodies stream through the proxy.
//! Its own test binary so other tests do not disturb the process RSS.
//! Unix only: peak RSS comes from getrusage.
#![cfg(unix)]

mod common;

use common::*;
use futures_util::StreamExt;

fn peak_rss_bytes() -> u64 {
    // SAFETY: getrusage only writes into the zeroed struct we pass.
    let usage = unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        u
    };
    let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    usage.ru_maxrss as u64 * unit
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_stays_bounded_for_huge_bodies() {
    const SIZE: u64 = 256 * 1024 * 1024;
    let upstream = spawn_http().await;
    let cert = ca_signed_cert();
    let tls_upstream = spawn_https(&cert, &[b"h2", b"http/1.1"]).await;
    let (proxy, ca) = start_proxy(|c| c.extra_upstream_roots_pem = Some(cert.ca_pem.clone())).await;
    let http = client(&proxy, Some(ca.cert_pem()));

    // Warm up every path (TLS, pools, buffers) before taking the baseline.
    let warm = [
        format!("http://{upstream}/stream/4000000"),
        format!("https://127.0.0.1:{}/stream/4000000", tls_upstream.port()),
    ];
    for url in &warm {
        http.get(url).send().await.unwrap().bytes().await.unwrap();
    }
    let baseline = peak_rss_bytes();

    let urls = [
        format!("http://{upstream}/stream/{SIZE}"),
        format!("https://127.0.0.1:{}/stream/{SIZE}", tls_upstream.port()),
    ];
    for url in &urls {
        let mut stream = http.get(url).send().await.unwrap().bytes_stream();
        let mut n = 0u64;
        while let Some(chunk) = stream.next().await {
            n += chunk.unwrap().len() as u64;
        }
        assert_eq!(n, SIZE);
    }
    let chunks = futures_util::stream::iter((0..SIZE / 65536).map(|_| Ok::<_, std::io::Error>(vec![b'u'; 65536])));
    let r =
        http.post(format!("http://{upstream}/count")).body(reqwest::Body::wrap_stream(chunks)).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), SIZE.to_string());

    let entries = finished(&proxy, 5).await;
    let big: Vec<_> = entries.iter().filter(|e| e.response_body.size == SIZE || e.request_body.size == SIZE).collect();
    assert_eq!(big.len(), 3);
    for e in big {
        assert!(e.response_body.data.len() <= 1024 * 1024 && e.request_body.data.len() <= 1024 * 1024);
    }
    let peak = peak_rss_bytes();
    let growth = peak.saturating_sub(baseline);
    eprintln!("peak RSS {} MiB, growth during 768 MiB of transfers: {} MiB", peak >> 20, growth >> 20);
    assert!(growth < 64 * 1024 * 1024, "memory grew by {} MiB", growth >> 20);
    proxy.stop().await;
}
