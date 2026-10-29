//! The TLS clients that leave the crypto provider to rustls — mysql_async
//! and async-nats both build their config with `ClientConfig::builder()` —
//! reach the TLS handshake once the process default is installed, instead
//! of panicking because rustls is built with both ring and aws-lc-rs.
//!
//! Each test stands up a fake server that speaks just enough of the
//! protocol to make the client switch to TLS, then reports the first byte
//! the client sends after the switch: 0x16 is a TLS handshake record, so
//! the client built its TLS config and started the handshake. The server
//! then hangs up, so the connection itself fails — the point is that it
//! fails as an error, not a panic.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// The first byte of a TLS record carrying a handshake message.
const TLS_HANDSHAKE_RECORD: u8 = 0x16;

/// A MySQL protocol-10 greeting that offers TLS (`CLIENT_SSL`).
fn mysql_greeting() -> Vec<u8> {
    const CLIENT_LONG_PASSWORD: u32 = 0x1;
    const CLIENT_PROTOCOL_41: u32 = 0x200;
    const CLIENT_SSL: u32 = 0x800;
    const CLIENT_TRANSACTIONS: u32 = 0x2000;
    const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
    const CLIENT_PLUGIN_AUTH: u32 = 0x8_0000;
    let caps = CLIENT_LONG_PASSWORD
        | CLIENT_PROTOCOL_41
        | CLIENT_SSL
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH;

    let mut payload = vec![0x0a];
    payload.extend_from_slice(b"8.0.36\0");
    payload.extend_from_slice(&1u32.to_le_bytes()); // connection id
    payload.extend_from_slice(b"abcdefgh"); // scramble, part 1
    payload.push(0); // filler
    payload.extend_from_slice(&((caps & 0xffff) as u16).to_le_bytes());
    payload.push(0x21); // utf8_general_ci
    payload.extend_from_slice(&2u16.to_le_bytes()); // SERVER_STATUS_AUTOCOMMIT
    payload.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
    payload.push(21); // scramble length
    payload.extend_from_slice(&[0; 10]); // reserved
    payload.extend_from_slice(b"ijklmnopqrst\0"); // scramble, part 2
    payload.extend_from_slice(b"mysql_native_password\0");

    let len = payload.len() as u32;
    let mut packet = len.to_le_bytes()[..3].to_vec();
    packet.push(0); // sequence id
    packet.extend_from_slice(&payload);
    packet
}

#[tokio::test]
async fn mysql_over_tls_reaches_the_handshake_instead_of_panicking() {
    workforce_common::tls::install_default_crypto_provider();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.write_all(&mysql_greeting()).await.unwrap();
        // The client's SSLRequest: a 4-byte header and a 32-byte body.
        let mut ssl_request = [0u8; 36];
        socket.read_exact(&mut ssl_request).await.unwrap();
        let mut first = [0u8; 1];
        socket.read_exact(&mut first).await.unwrap();
        let _ = seen_tx.send(first[0]);
        // Dropping the socket fails the handshake.
    });

    let opts =
        mysql_async::Opts::from_url(&format!("mysql://user:pw@127.0.0.1:{port}/db")).unwrap();
    let opts = mysql_async::OptsBuilder::from_opts(opts).ssl_opts(
        mysql_async::SslOpts::default()
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true),
    );
    let result = tokio::time::timeout(Duration::from_secs(10), mysql_async::Conn::new(opts))
        .await
        .expect("the connection attempt finishes");
    assert!(
        result.is_err(),
        "the fake server cannot complete a handshake"
    );

    let first = tokio::time::timeout(Duration::from_secs(5), seen_rx)
        .await
        .expect("the client switched to TLS")
        .unwrap();
    assert_eq!(first, TLS_HANDSHAKE_RECORD);
}

#[tokio::test]
async fn nats_over_tls_reaches_the_handshake_instead_of_panicking() {
    workforce_common::tls::install_default_crypto_provider();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let info = format!(
            "INFO {{\"server_id\":\"fake\",\"server_name\":\"fake\",\"version\":\"2.10.0\",\
             \"proto\":1,\"host\":\"127.0.0.1\",\"port\":{port},\"max_payload\":1048576,\
             \"tls_required\":true}}\r\n"
        );
        socket.write_all(info.as_bytes()).await.unwrap();
        let mut first = [0u8; 1];
        socket.read_exact(&mut first).await.unwrap();
        let _ = seen_tx.send(first[0]);
    });

    let config = workforce_nats::client::NatsConfig {
        url: format!("nats://127.0.0.1:{port}"),
        connect_timeout_secs: 2,
        max_reconnects: Some(0),
        require_tls: true,
        ..Default::default()
    };
    let result = workforce_nats::client::connect(&config).await;
    assert!(
        result.is_err(),
        "the fake server cannot complete a handshake"
    );

    let first = tokio::time::timeout(Duration::from_secs(5), seen_rx)
        .await
        .expect("the client switched to TLS")
        .unwrap();
    assert_eq!(first, TLS_HANDSHAKE_RECORD);
}
