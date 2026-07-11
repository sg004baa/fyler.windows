//! 匿名フィードバックendpoint解決とHTTP送信。

use std::time::Duration;

/// フィードバック送信結果。詳細な通信情報は保持しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackOutcome {
    Accepted,
    Invalid,
    RateLimited,
    ServerError,
    Network,
    Timeout,
}

impl FeedbackOutcome {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Accepted => "フィードバックを受け付けました。ありがとうございます。",
            Self::Invalid => "送信内容を受け付けられませんでした。内容を確認してください。",
            Self::RateLimited => "送信間隔が短すぎます。時間をおいてからもう一度お試しください。",
            Self::ServerError => {
                "サーバーで問題が発生しました。時間をおいてからもう一度お試しください。"
            }
            Self::Network => "ネットワークに接続できませんでした。接続を確認してください。",
            Self::Timeout => "送信がタイムアウトしました。時間をおいてもう一度お試しください。",
        }
    }
}

/// config値とbuild時既定値からendpointを解決する。
///
/// configで空文字列が指定された場合は、build時既定値があっても明示的に無効化する。
pub(crate) fn resolve_endpoint(
    configured: Option<&str>,
    built_in: Option<&'static str>,
) -> Option<String> {
    match configured {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(value.to_owned()),
        None => built_in
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned),
    }
}

/// JSONをendpointへPOSTする。レスポンスbodyは読み取らない。
pub(crate) fn send_feedback(url: &str, json: &str, timeout: Duration) -> FeedbackOutcome {
    // NativeTls は自動選択されない(ureqの既定はRustls)ため明示指定する。
    // Windowsでは SChannel = システム証明書ストアを使う。
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    match agent
        .post(url)
        .header("Content-Type", "application/json")
        .send(json)
    {
        Ok(_) => FeedbackOutcome::Accepted,
        Err(ureq::Error::StatusCode(429)) => FeedbackOutcome::RateLimited,
        Err(ureq::Error::StatusCode(400..=499)) => FeedbackOutcome::Invalid,
        Err(ureq::Error::StatusCode(500..=599)) => FeedbackOutcome::ServerError,
        Err(ureq::Error::Timeout(_)) => FeedbackOutcome::Timeout,
        Err(_) => FeedbackOutcome::Network,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::*;

    fn mock_response(status: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        format!("http://{address}/feedback")
    }

    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                break (header_end, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..read]);
        }
    }

    #[test]
    fn maps_success_and_http_status_classes() {
        for (status, expected) in [
            (204, FeedbackOutcome::Accepted),
            (400, FeedbackOutcome::Invalid),
            (429, FeedbackOutcome::RateLimited),
            (500, FeedbackOutcome::ServerError),
        ] {
            assert_eq!(
                send_feedback(&mock_response(status), "{}", Duration::from_secs(1)),
                expected
            );
        }
    }

    #[test]
    fn maps_response_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            thread::sleep(Duration::from_millis(200));
        });
        assert_eq!(
            send_feedback(
                &format!("http://{address}/feedback"),
                "{}",
                Duration::from_millis(30)
            ),
            FeedbackOutcome::Timeout
        );
        server.join().unwrap();
    }

    #[test]
    fn maps_offline_port_to_network_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        // WindowsのWinsockはloopbackの接続拒否でも内部で約1秒connectを
        // リトライするため、timeoutが短いと拒否確定前にTimeoutへ倒れる。
        // 拒否は両OSとも数秒以内に確定するので、余裕を持たせてNetworkを検証する。
        assert_eq!(
            send_feedback(
                &format!("http://{address}/feedback"),
                "{}",
                Duration::from_secs(10)
            ),
            FeedbackOutcome::Network
        );
    }

    #[test]
    fn endpoint_resolution_honors_config_precedence_and_disable() {
        assert_eq!(
            resolve_endpoint(Some("https://config.test"), Some("https://build.test")),
            Some("https://config.test".to_owned())
        );
        assert_eq!(resolve_endpoint(Some(""), Some("https://build.test")), None);
        assert_eq!(
            resolve_endpoint(None, Some("https://build.test")),
            Some("https://build.test".to_owned())
        );
        assert_eq!(resolve_endpoint(None, None), None);
    }

    #[test]
    fn user_messages_never_include_transport_details() {
        for outcome in [
            FeedbackOutcome::Accepted,
            FeedbackOutcome::Invalid,
            FeedbackOutcome::RateLimited,
            FeedbackOutcome::ServerError,
            FeedbackOutcome::Network,
            FeedbackOutcome::Timeout,
        ] {
            let message = outcome.message();
            assert!(!message.contains("http"));
            assert!(!message.contains("127.0.0.1"));
            assert!(!message.contains("OS error"));
        }
    }
}
