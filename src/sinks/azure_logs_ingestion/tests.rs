use std::time::Duration;
use futures::stream;
use http::Response;
use hyper::body;
use tokio::time::timeout;
use vector_lib::config::log_schema;

use azure_identity::{ClientSecretCredential, ClientSecretCredentialOptions, TokenCredentialOptions};

use super::config::AzureLogsIngestionConfig;

use crate::{
    event::LogEvent,
    sinks::prelude::*,
    test_util::{
        components::{run_and_assert_sink_compliance, SINK_TAGS},
        http::spawn_blackhole_http_server,
    },
};


#[test]
fn generate_config() {
    crate::test_util::test_generate_config::<AzureLogsIngestionConfig>();
}

#[test]
fn basic_config() {
    let config = toml::from_str::<AzureLogsIngestionConfig>(r#"
            endpoint = "https://my-dce-5kyl.eastus-1.ingest.monitor.azure.com"
            dcr_immutable_id = "dcr-00000000000000000000000000000000"
            stream_name = "Custom-UnitTest"
        "#)
        .expect("Config parsing failed without custom host");
    assert_eq!(config.endpoint, "https://my-dce-5kyl.eastus-1.ingest.monitor.azure.com");
    assert_eq!(config.dcr_immutable_id, "dcr-00000000000000000000000000000000");
    assert_eq!(config.stream_name, "Custom-UnitTest");
    assert_eq!(config.token_scope, "https://monitor.azure.com/.default");
    assert_eq!(config.timestamp_field, "TimeGenerated");
}

fn insert_timestamp_kv(log: &mut LogEvent) -> (String, String) {
    let now = chrono::Utc::now();

    let timestamp_value = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    log.insert(log_schema().timestamp_key_target_path().unwrap(), now);

    (
        log_schema().timestamp_key().unwrap().to_string(),
        timestamp_value,
    )
}

#[tokio::test]
async fn correct_request() {

    // Because we use the azure_identity::DefaultAzureCredential, we need to run our own mock OAuth endpoint as well
    let (authority_tx, mut _authority_rx) = tokio::sync::mpsc::channel(1);
    let mock_token_authority = spawn_blackhole_http_server(move |request| {
        let authority_tx = authority_tx.clone();
        async move {
            authority_tx.send(request).await.unwrap();
            let body = serde_json::json!({
                "access_token": "mock-access-token",
                "token_type": "Bearer",
                "expires_in": 3600
            }).to_string();

            Ok(Response::builder()
            .header("Content-Type", "application/json")
            .body(body.into())
            .unwrap())
        }
    })
    .await;

    let mut credential_options = TokenCredentialOptions::default();
    //credential_options.set_authority_host("http://127.0.0.1:9001".into());
    credential_options.set_authority_host(mock_token_authority.to_string());

    let credential = ClientSecretCredential::new(
        "00000000-0000-0000-0000-000000000000",
        "mock-client-id".into(),
        "mock-client-secret".into(),
        Some(ClientSecretCredentialOptions {
            credential_options: credential_options,
        }),
    );

    let config: AzureLogsIngestionConfig = toml::from_str(r#"
            endpoint = "http://localhost:9001"
            dcr_immutable_id = "dcr-00000000000000000000000000000000"
            stream_name = "Custom-UnitTest"
        "#)
        .unwrap();

    let mut log1 = [("message", "hello")].iter().copied().collect::<LogEvent>();
    let (_timestamp_key1, timestamp_value1) = insert_timestamp_kv(&mut log1);

    let mut log2 = [("message", "world")].iter().copied().collect::<LogEvent>();
    let (_timestamp_key2, timestamp_value2) = insert_timestamp_kv(&mut log2);

    let (endpoint_tx, mut endpoint_rx) = tokio::sync::mpsc::channel(1);
    let mock_endpoint = spawn_blackhole_http_server(move |request| {
        let endpoint_tx = endpoint_tx.clone();
        async move {
            endpoint_tx.send(request).await.unwrap();
            Ok(Response::new(hyper::Body::empty()))
        }
    })
    .await;

    let context = SinkContext::default();

    let (sink, _healthcheck) = config
        .build_inner(
            context,
            mock_endpoint.into(),
            config.dcr_immutable_id.clone(),
            config.stream_name.clone(),
            config.token_scope.clone(),
            config.timestamp_field.clone(),
            Some(credential.unwrap()),
        )
        .await
        .unwrap();

    run_and_assert_sink_compliance(sink, stream::iter(vec![log1, log2]), &SINK_TAGS).await;

    let request = timeout(Duration::from_millis(500), endpoint_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let (parts, body) = request.into_parts();
    assert_eq!(&parts.method.to_string(), "POST");

    let body = body::to_bytes(body).await.unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body[..]).unwrap();
    let expected_json = serde_json::json!([
        {
            "TimeGenerated": timestamp_value1,
            "message": "hello"
        },
        {
            "TimeGenerated": timestamp_value2,
            "message": "world"
        }
    ]);
    assert_eq!(body_json, expected_json);

    let headers = parts.headers;
    let authorization = headers.get("Authorization").unwrap();
    assert_eq!(authorization.to_str().unwrap(), "Bearer mock-access-token");

    assert_eq!(
        &parts.uri.path_and_query().unwrap().to_string(),
        "/dataCollectionRules/dcr-00000000000000000000000000000000/streams/Custom-UnitTest?api-version=2023-01-01"
    );

}
