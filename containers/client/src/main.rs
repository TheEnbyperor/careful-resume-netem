#[macro_use]
extern crate log;

use rand::seq::SliceRandom;

const MAX_DATAGRAM_SIZE: usize = 1350;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let url = url::Url::parse(&std::env::var("SERVER_URL").unwrap()).unwrap();
    let url_host = url.host_str().unwrap();
    let url_port = url.port_or_known_default().unwrap();
    let url_authority = format!("{}:{}", url_host, url_port);
    let url_domain = url.domain().unwrap();
    let peer_addrs = tokio::net::lookup_host(url_authority)
        .await
        .unwrap()
        .collect::<Vec<_>>();
    let mut rng = rand::thread_rng();
    let peer_addr = *peer_addrs.choose(&mut rng).unwrap();
    drop(rng);

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_cc_algorithm(quiche::CongestionControlAlgorithm::CUBIC);
    config.verify_peer(false);
    config.set_application_protos(&[b"h3"]).unwrap();
    config.set_max_idle_timeout(15000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_active_migration(true);
    config.enable_pacing(false);
    config.enable_resume(true);

    info!("Setting up QUIC connection to {} - {}", url, peer_addr);
    let connection = quiche_tokio::Connection::connect(
        peer_addr, config, Some(url_domain), None, None, None,
    ).await.unwrap();
    connection.established().await.unwrap();
    info!("QUIC connection open");

    let dcid = format!("{:?}", connection.dcid().await.unwrap());

    let mut h3_connection = quiver_h3::Connection::new(connection, false);
    h3_connection.setup().await.unwrap();
    info!("HTTP/3 connection open");

    let mut headers = quiver_h3::Headers::new();
    headers.add(b":method", b"GET");
    headers.add(b":scheme", b"https");
    headers.add(b":authority", url_host.as_bytes());
    headers.add(b":path", url.path().as_bytes());
    headers.add(b"user-agent", b"quiche-tokio");

    info!("Sending request: {:#?}", headers);
    let mut response = h3_connection.send_request(&headers).await.unwrap();
    info!("Got response: {:#?}", response);

    let mut total = 0;
    let start = std::time::Instant::now();
    while let Some(data) = response.get_next_data().await.unwrap() {
        total += data.len();
    }

    let elapsed = start.elapsed();
    let bits_per_sec = (total * 8) as f64 / elapsed.as_secs_f64();
    let mbit_per_sec = bits_per_sec / 1_000_000.0;
    info!("Receive done, got {} bytes, transfer rate {:.3} mbit/s", total, mbit_per_sec);

    h3_connection.close().await.unwrap();
    info!("HTTP/3 and QUIC connection closed");

    println!("\x1e{{\"dcid\": \"{}\", \"total\": {}, \"rate\": {}}}", dcid, total, mbit_per_sec);
}