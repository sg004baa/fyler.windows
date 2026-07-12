//! 診断用: フィードバックendpointへの生のTLS/HTTPエラーを表示する。
//!
//! 本体GUIはセキュリティ上transport詳細を隠す(定型文のみ表示する)ため、
//! 現地でNetwork系エラーを切り分けるときはこれを使う:
//!
//! ```text
//! cargo run -p fyler-app --example feedback_probe -- https://<endpoint>
//! ```
//!
//! `ERR: StatusCode(400)` はTLS/HTTP経路が正常でサーバーがschema違反を
//! 返しただけ(= 疎通OK)。TLS系エラーはDebug表現がそのまま出る。
//!
//! 注意: Agent設定は `src/feedback.rs` の `send_feedback` と同一に保つこと。

fn main() {
    let url = std::env::args()
        .nth(1)
        .expect("usage: feedback_probe <url>");
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    match agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send("{}")
    {
        Ok(response) => println!("TLS OK, status={}", response.status()),
        Err(error) => println!("ERR: {error:?}"),
    }
}
