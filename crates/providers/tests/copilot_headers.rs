use agent_shim_providers::github_copilot::{
    credential_store::StoredCredentials,
    headers::{
        COPILOT_INTEGRATION_ID, EDITOR_PLUGIN_VERSION, EDITOR_VERSION, OPENAI_INTENT,
        USER_AGENT as COPILOT_USER_AGENT,
    },
    token_manager::CopilotToken,
    CopilotProvider,
};

fn make_token() -> CopilotToken {
    CopilotToken {
        token: "tid_headertest".to_string(),
        api_base: "https://api.githubcopilot.com".to_string(),
        expires_at_unix: 9_999_999_999,
    }
}

fn make_provider() -> CopilotProvider {
    let creds = StoredCredentials {
        github_oauth_token: "gho_test".to_string(),
        created_at_unix: 0,
    };
    CopilotProvider::spawn_with_creds(creds, "https://api.githubcopilot.com".to_string())
        .expect("provider should build")
}

#[tokio::test]
async fn required_headers_present() {
    let provider = make_provider();
    let token = make_token();
    let body = serde_json::json!({"model": "gpt-4o", "messages": []});
    let request_id = "test-req-id-1234";

    let req = provider
        .build_request_for_test(&token, &body, request_id, false)
        .await
        .expect("request should build");

    let headers = req.headers();

    // Authorization header should contain the token
    let auth = headers
        .get("authorization")
        .expect("authorization header must be present")
        .to_str()
        .unwrap();
    assert!(auth.starts_with("Bearer "), "auth should be Bearer: {auth}");
    assert!(auth.contains("tid_headertest"));

    // User-Agent
    let ua = headers
        .get("user-agent")
        .expect("user-agent must be present")
        .to_str()
        .unwrap();
    assert_eq!(ua, COPILOT_USER_AGENT);

    // editor-version
    let ev = headers
        .get("editor-version")
        .expect("editor-version must be present")
        .to_str()
        .unwrap();
    assert_eq!(ev, EDITOR_VERSION);

    // editor-plugin-version
    let epv = headers
        .get("editor-plugin-version")
        .expect("editor-plugin-version must be present")
        .to_str()
        .unwrap();
    assert_eq!(epv, EDITOR_PLUGIN_VERSION);

    // copilot-integration-id
    let cid = headers
        .get("copilot-integration-id")
        .expect("copilot-integration-id must be present")
        .to_str()
        .unwrap();
    assert_eq!(cid, COPILOT_INTEGRATION_ID);

    // openai-intent
    let intent = headers
        .get("openai-intent")
        .expect("openai-intent must be present")
        .to_str()
        .unwrap();
    assert_eq!(intent, OPENAI_INTENT);

    // x-request-id
    let rid = headers
        .get("x-request-id")
        .expect("x-request-id must be present")
        .to_str()
        .unwrap();
    assert_eq!(rid, request_id);
}

#[tokio::test]
async fn stream_request_has_accept_header() {
    let provider = make_provider();
    let token = make_token();
    let body = serde_json::json!({"model": "gpt-4o", "messages": [], "stream": true});

    let req = provider
        .build_request_for_test(&token, &body, "req-stream", true)
        .await
        .expect("request should build");

    let accept = req
        .headers()
        .get("accept")
        .expect("accept header must be present for stream")
        .to_str()
        .unwrap();
    assert_eq!(accept, "text/event-stream");
}
