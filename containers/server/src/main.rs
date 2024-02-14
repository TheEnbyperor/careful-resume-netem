#[macro_use]
extern crate log;

use tokio::io::AsyncReadExt;

const FILE_PATH_1_MB: &'static str = "/files/1MB.bin";
const FILE_PATH_10_MB: &'static str = "/files/10MB.bin";
const FILE_PATH_100_MB: &'static str = "/files/100MB.bin";
const FILE_PATH_1_GB: &'static str = "/files/1GB.bin";

const CERT_PATH: &'static str = "/certs/cert.crt";
const CERT_KEY_PATH: &'static str = "/certs/cert.key";

const MAX_DATAGRAM_SIZE: usize = 1350;
const BIND_ADDR: std::net::SocketAddr = std::net::SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 443);

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_cc_algorithm(quiche::CongestionControlAlgorithm::CUBIC);
    config.load_cert_chain_from_pem_file(CERT_PATH).unwrap();
    config.load_priv_key_from_pem_file(CERT_KEY_PATH).unwrap();
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
    config.set_bdp_tokens(true);
    config.enable_pacing(false);
    config.enable_resume(true);

    info!("Accepting QUIC connections on {}", BIND_ADDR);
    let mut connections =
        quiche_tokio::Connection::accept(BIND_ADDR, config)
            .await
            .unwrap();

    let done = std::sync::Arc::new(tokio::sync::Notify::new());

    loop {
        tokio::select! {
            _ = done.notified() => {
                break;
            }
            c = connections.next() => {
                let connection = match c {
                    Some(c) => c,
                    None => break,
                };

                let done = done.clone();
                tokio::task::spawn(async move {
                    info!("New connection");

                    let scid = connection.scid();
                    let (qlog, qlog_handle) = quiche_tokio::QLog::new(format!("/qlog/connection-{:?}.qlog", scid)).await.unwrap();
                    let qlog_conf = quiche_tokio::QLogConfig {
                        qlog,
                        title: format!("{:?}", scid),
                        description: String::new(),
                        level: quiche::QlogLevel::Extra,
                    };
                    connection.set_qlog(qlog_conf).await.unwrap();

                    connection.established().await.unwrap();

                    if let (Ok(rtt), Ok(cwnd)) = (std::env::var("CR_RTT_MS"), std::env::var("CR_CWND")) {
                        let rtt = u64::from_str_radix(&rtt, 10).unwrap();
                        let rtt = std::time::Duration::from_millis(rtt);
                        let cwnd = usize::from_str_radix(&cwnd, 10).unwrap();
                        connection.setup_careful_resume(rtt, cwnd).await.unwrap()
                    }

                    let alpn = connection.application_protocol().await;
                    info!("New connection established, alpn={}", String::from_utf8_lossy(&alpn));

                    if alpn != b"h3" {
                        warn!("Non HTTP/3 connection negotiated");
                        return;
                    }

                    let mut h3_connection = quiver_h3::Connection::new(connection, true);
                    h3_connection.setup().await.unwrap();
                    info!("HTTP/3 connection open");

                    while let Some(mut request) = h3_connection.next_request().await.unwrap() {
                        tokio::task::spawn(async move {
                            info!("New request");

                            let headers = request.headers();
                            match headers.get_header_one(b":method") {
                                Some(m) => {
                                    if m.value.as_ref() != b"GET" {
                                        let mut headers = quiver_h3::Headers::new();
                                        headers.add(b":status", b"405");
                                        request.send_headers(&headers).await.unwrap();
                                        request.done().await.unwrap();
                                        return;
                                    }
                                },
                                None => {
                                    let mut headers = quiver_h3::Headers::new();
                                    headers.add(b":status", b"400");
                                    request.send_headers(&headers).await.unwrap();
                                    request.done().await.unwrap();
                                    return;
                                }
                            };
                            let scheme = headers.get_header_one(b":scheme");
                            let path = headers.get_header_one(b":path");

                            if scheme.is_none() || path.is_none() {
                                let mut headers = quiver_h3::Headers::new();
                                headers.add(b":status", b"400");
                                request.send_headers(&headers).await.unwrap();
                                request.done().await.unwrap();
                                return;
                            }

                            let scheme = scheme.unwrap();
                            let path = path.unwrap();

                            if scheme.value.as_ref() != b"https" {
                                let mut headers = quiver_h3::Headers::new();
                                headers.add(b":status", b"400");
                                request.send_headers(&headers).await.unwrap();
                                request.done().await.unwrap();
                                return;
                            }

                            match path.value.as_ref() {
                                b"/" => {
                                    let mut headers = quiver_h3::Headers::new();
                                    headers.add(b":status", b"200");
                                    request.send_headers(&headers).await.unwrap();
                                    request.send_data(b"hello world").await.unwrap();
                                    request.done().await.unwrap();
                                }
                                b"/1MB.bin" => {
                                    send_file(&mut request, FILE_PATH_1_MB).await;
                                }
                                b"/10MB.bin" => {
                                    send_file(&mut request, FILE_PATH_10_MB).await;
                                }
                                b"/100MB.bin" => {
                                    send_file(&mut request, FILE_PATH_100_MB).await;
                                }
                                b"/1GB.bin" => {
                                    send_file(&mut request, FILE_PATH_1_GB).await;
                                }
                                _ => {
                                    let mut headers = quiver_h3::Headers::new();
                                    headers.add(b":status", b"400");
                                    request.send_headers(&headers).await.unwrap();
                                    request.done().await.unwrap();
                                }
                            }

                            info!("Request done");
                        });
                    }
                    info!("HTTP/3 connection closed");
                    qlog_handle.await.unwrap();
                    done.notify_one();
                });
            }
        }
    }
}

async fn send_file<S: AsRef<std::path::Path>>(request: &mut quiver_h3::Message, path: S) {
    let mut headers = quiver_h3::Headers::new();
    headers.add(b":status", b"200");
    request.send_headers(&headers).await.unwrap();
    let file = tokio::fs::File::open(path).await.unwrap();
    let mut buffer = [0; 8192];
    let mut reader = tokio::io::BufReader::new(file);
    loop {
        let n = reader.read(&mut buffer).await.unwrap();
        if n == 0 {
            break;
        }
        request.send_data(&buffer[..n]).await.unwrap();
    }
    request.done().await.unwrap();
}