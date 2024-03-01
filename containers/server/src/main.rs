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

const BDP_ERROR_CODE: u64 = 0x4143414213370002;

fn common_subnet(lhs: std::net::IpAddr, rhs: std::net::IpAddr) -> bool {
    match (lhs, rhs) {
        (std::net::IpAddr::V4(lhs), std::net::IpAddr::V4(rhs)) => {
            &lhs.octets()[..3] == &rhs.octets()[..3]
        }
        (std::net::IpAddr::V6(lhs), std::net::IpAddr::V6(rhs)) => {
            &lhs.octets()[..6] == &rhs.octets()[..6]
        }
        _ => false
    }
}

async fn setup_cr(connection: &quiche_tokio::Connection, bdp_key: Option<&[u8; 32]>) -> Result<bool, quiche_tokio::ConnectionError> {
    let peer_token_bytes = connection.peer_token().await;
    if let Some(peer_token_bytes) = peer_token_bytes {
        info!("Received token from peer: {:02x?}", peer_token_bytes);
        let mut peer_token_buf = std::io::Cursor::new(peer_token_bytes);
        if let Ok(ex_token) = quiver_bdp_tokens::ExToken::decode(&mut peer_token_buf) {
            info!("Decoded extensible token from peer: {:02x?}", ex_token);

            if let Some(peer_bdp_token_bytes) = ex_token.get_extension(quiver_bdp_tokens::ExtensionType::BDPToken) {
                info!("Received BDP token from peer: {:02x?}", peer_bdp_token_bytes);
                let mut peer_bdp_token_buf = std::io::Cursor::new(peer_bdp_token_bytes);
                if let Ok(mut peer_token) = quiver_bdp_tokens::BDPToken::decode(&mut peer_bdp_token_buf) {
                    info!("Decoded BDP token from peer: {:?}", peer_token);

                    if let Some(bdp_key) = bdp_key {
                        if let Some(private_data) = peer_token.private_data(bdp_key) {
                            info!("Decrypted private BDP token data: {:?}", private_data);

                            if peer_token.requested_capacity == 0 {
                                info!("Client has requested careful resume be disabled");
                            } else if private_data.expired() {
                                warn!("Peer BDP token expired");
                            } else if !common_subnet(private_data.ip(), connection.peer_addr().ip()) {
                                info!("Client on a different subnet, not using careful resume");
                            } else {
                                info!("BDP token verified, using for careful resume");
                                let capacity = std::cmp::min(
                                    peer_token.saved_capacity, peer_token.requested_capacity
                                );
                                connection.setup_careful_resume(
                                    peer_token.saved_rtt_duration(), capacity as usize
                                ).await?;
                            }
                        } else {
                            warn!("Failed to decrypt private BDP token data");
                            connection.close(false, BDP_ERROR_CODE, vec![]).await?;
                            return Ok(true);
                        }
                    } else {
                        warn!("No key available to decrypt private BDP token data");
                    }
                } else {
                    connection.close(false, BDP_ERROR_CODE, vec![]).await?;
                    return Ok(true);
                }
            }
        } else {
            connection.close(false, BDP_ERROR_CODE, vec![]).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

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

    let bdp_key = std::env::var("BDP_KEY").ok()
        .map(|k| hex::decode(k).unwrap())
        .map(|k| <[u8; 32]>::try_from(k).unwrap());

    while let Some(mut connection) = connections.next().await {
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
            if setup_cr(&connection, bdp_key.as_ref()).await.unwrap() {
                return;
            }

            let alpn = connection.application_protocol().await;
            info!("New connection established, alpn={}", String::from_utf8_lossy(&alpn));

            if alpn != b"h3" {
                warn!("Non HTTP/3 connection negotiated");
                return;
            }

            if let Some(bdp_key) = bdp_key {
                let mut cr_event_recv = connection.cr_events();
                let cr_event_connection = connection.send_half();
                tokio::task::spawn(async move {
                    while let Some(cr_event) = cr_event_recv.next().await {
                        info!("New CR event: {:?}", cr_event);

                        let peer_ip = cr_event_connection.peer_addr().ip();
                        let bdp_token = quiver_bdp_tokens::BDPToken::from_quiche_cr_event(
                            &cr_event, peer_ip, chrono::Duration::minutes(5), &bdp_key
                        );

                        let mut bdp_token_buf = std::io::Cursor::new(vec![]);
                        bdp_token.encode(&mut bdp_token_buf).unwrap();

                        let mut ex_token = quiver_bdp_tokens::ExToken::default();
                        ex_token.extensions.insert(quiver_bdp_tokens::ExtensionType::BDPToken, bdp_token_buf.into_inner());
                        let mut ex_token_buf = std::io::Cursor::new(vec![]);
                        ex_token.encode(&mut ex_token_buf).unwrap();

                        cr_event_connection.send_new_token(ex_token_buf.into_inner()).await.unwrap();
                    }
                });
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
        });
    }
}

async fn send_file<S: AsRef<std::path::Path>>(request: &mut quiver_h3::Message, path: S) {
    let file = tokio::fs::File::open(path).await.unwrap();
    let file_meta = file.metadata().await.unwrap();
    let file_len = file_meta.len().to_string();
    let mut headers = quiver_h3::Headers::new();
    headers.add(b":status", b"200");
    headers.add(b"content-length", file_len.as_bytes());
    request.send_headers(&headers).await.unwrap();
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