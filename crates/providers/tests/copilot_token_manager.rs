use agent_shim_providers::github_copilot::{
    credential_store::StoredCredentials, token_manager::exchange_with_base,
};

fn make_creds() -> StoredCredentials {
    StoredCredentials {
        github_oauth_token: "gho_test".to_string(),
        created_at_unix: 0,
    }
}

fn make_client() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn exchange_parses_token_and_endpoint() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("GET", "/copilot_internal/v2/token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "token": "tid_abc123",
                "expires_at": 9999999999,
                "refresh_in": 1500,
                "endpoints": { "api": "https://api.githubcopilot.com" }
            }"#,
        )
        .create_async()
        .await;

    let result = exchange_with_base(&make_client(), &make_creds(), &server.url()).await;
    let token = result.expect("exchange should succeed");

    assert_eq!(token.token, "tid_abc123");
    assert_eq!(token.api_base, "https://api.githubcopilot.com");
    assert_eq!(token.expires_at_unix, 9_999_999_999);
}

#[tokio::test]
async fn exchange_returns_error_on_401() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("GET", "/copilot_internal/v2/token")
        .with_status(401)
        .with_body("Unauthorized")
        .create_async()
        .await;

    let result = exchange_with_base(&make_client(), &make_creds(), &server.url()).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("401") || err.contains("unauthorized") || err.contains("Unauthorized"),
        "unexpected error: {err}"
    );
}
