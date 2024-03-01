#[macro_use]
extern crate log;

use rand::seq::SliceRandom;

const MAX_DATAGRAM_SIZE: usize = 1350;

const MAX_STREAM_DATA_CAP: u64 = 10 * 1024u64.pow(2);

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Default)]
struct TokenDatabase {
    tokens: std::collections::HashMap<String, Token>
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
struct Token {
    address_validation_data: Vec<u8>,
    bdp_token: Option<quiver_bdp_tokens::BDPToken>,
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    let bdp_db_path = std::path::PathBuf::from("/data/bdp.db");
    let bdp_db = std::sync::Arc::new(rustbreak::PathDatabase::<
        TokenDatabase, rustbreak::deser::Yaml
    >::load_from_path_or_default(bdp_db_path).unwrap());

    let url = url::Url::parse(&std::env::var("SERVER_URL").unwrap()).unwrap();
    let url_host = url.host_str().unwrap();
    let url_port = url.port_or_known_default().unwrap();
    let url_authority = format!("{}:{}", url_host, url_port);
    let peer_addrs = tokio::net::lookup_host(&url_authority)
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

    let (token, default_stream_window) = bdp_db.read(|db| {
        match db.tokens.get(&url_authority).map(|t| {
            let t = t.clone();

            let mut ex_token = quiver_bdp_tokens::ExToken::default();
            ex_token.address_validation_data = t.address_validation_data;

            let mut saved_capacity = None;

            if let Some(mut bdp_token) = t.bdp_token {
                bdp_token.requested_capacity = bdp_token.saved_capacity;

                let mut token_buf = Vec::new();
                bdp_token.encode(&mut token_buf).unwrap();

                ex_token.extensions.insert(quiver_bdp_tokens::ExtensionType::BDPToken, token_buf);
                saved_capacity = Some(bdp_token.saved_capacity);
            }

            let mut ex_token_buf = Vec::new();
            ex_token.encode(&mut ex_token_buf).unwrap();

            (ex_token_buf, saved_capacity)
        }) {
            Some(r) => (Some(r.0), r.1),
            None => (None, None)
        }
    }).unwrap();

    info!("Setting up QUIC connection to {} - {}", url, peer_addr);
    let mut connection = quiche_tokio::Connection::connect(
        peer_addr, config, Some(url_host), None, token.as_deref(), None,
    ).await.unwrap();
    connection.established().await.unwrap();
    info!("QUIC connection open");

    if let Some(default_stream_window) = default_stream_window {
        connection.setup_default_stream_window(default_stream_window).await.unwrap();
    }

    let trans_params = connection.transport_parameters().await.unwrap();
    let server_bdp_tokens = trans_params.bdp_tokens;

    if !server_bdp_tokens {
        warn!("Server not using extensible tokens");
    } else {
        info!("Server using extensible tokens");
        let mut new_token_recv = connection.new_tokens();
        let bdp_db = bdp_db.clone();
        tokio::task::spawn(async move {
            while let Some(token) = match new_token_recv.next().await {
                Ok(r) => r,
                Err(err) => {
                    // H3_NO_ERROR
                    if err.to_id() == 0x100 {
                        return
                    }
                    panic!("Error receiving tokens: {:?}", err);
                }
            } {
                trace!("New token received: {:02x?}", token);
                let mut token_buf = std::io::Cursor::new(token);
                let ex_token = quiver_bdp_tokens::ExToken::decode(&mut token_buf).unwrap();
                info!("Extensible token: {:02x?}", ex_token);

                let token = Token {
                    bdp_token: ex_token.get_extension(quiver_bdp_tokens::ExtensionType::BDPToken)
                        .map(|bdp_token_bytes| {
                            let mut bdp_token_buf = std::io::Cursor::new(bdp_token_bytes);
                            let bdp_token = quiver_bdp_tokens::BDPToken::decode(&mut bdp_token_buf).unwrap();
                            info!("BDP token: {:?}", bdp_token);
                            bdp_token
                        }),
                    address_validation_data: ex_token.address_validation_data,
                };

                bdp_db.write(|db| {
                    db.tokens.insert(url_authority.clone(), token);
                }).unwrap();
            }
        });
    }

    if let Ok(cwnd) = std::env::var("CR_CWND") {
        let cwnd = u64::from_str_radix(&cwnd, 10).unwrap();
        connection.setup_default_stream_window(cwnd / 2).await.unwrap()
    }

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

    if let Some(max_data) = response.headers().get_header_one(b"content-length")
        .and_then(|h| String::from_utf8(h.value.to_vec()).ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|cl|  std::cmp::min(MAX_STREAM_DATA_CAP, cl)) {
        response.set_max_data(max_data).await.unwrap();
    }

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

    bdp_db.save().unwrap();

    println!(
        "\x1e{{\"dcid\": \"{}\", \"total\": {}, \"time\": {}, \"rate\": {}}}",
        dcid, total, elapsed.as_secs_f64(), mbit_per_sec
    );
}