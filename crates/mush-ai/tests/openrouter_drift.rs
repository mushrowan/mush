#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use mush_ai::openrouter_coverage::{FieldStatus, assert_no_drift};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

async fn fetch_raw_get(path: &str) -> serde_json::Value {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{BASE_URL}{path}"))
        .send()
        .await
        .expect("openrouter request failed");
    let status = resp.status();
    let body = resp.text().await.expect("body read failed");
    assert!(
        status.is_success(),
        "GET {path}: status {status}: {}",
        body.chars().take(500).collect::<String>(),
    );
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path}: parse failed: {e}: {body}"))
}

const ENDPOINTS_TOP_LEVEL: &[FieldStatus] = &[FieldStatus::Consumed("data")];

const ENDPOINTS_DATA_FIELDS: &[FieldStatus] = &[
    FieldStatus::Consumed("id"),
    FieldStatus::Consumed("name"),
    FieldStatus::Consumed("endpoints"),
    FieldStatus::Ignored("created"),
    FieldStatus::Ignored("description"),
    FieldStatus::Ignored("architecture"),
];

const ARCHITECTURE_FIELDS: &[FieldStatus] = &[
    FieldStatus::Ignored("tokenizer"),
    FieldStatus::Ignored("instruct_type"),
    FieldStatus::Ignored("modality"),
    FieldStatus::Ignored("input_modalities"),
    FieldStatus::Ignored("output_modalities"),
];

const ENDPOINT_ITEM_FIELDS: &[FieldStatus] = &[
    FieldStatus::Consumed("provider_name"),
    FieldStatus::Consumed("context_length"),
    FieldStatus::Consumed("supported_parameters"),
    FieldStatus::Consumed("pricing"),
    FieldStatus::Consumed("supports_implicit_caching"),
    FieldStatus::Ignored("name"),
    FieldStatus::Ignored("model_id"),
    FieldStatus::Ignored("model_name"),
    FieldStatus::Ignored("tag"),
    FieldStatus::Ignored("quantization"),
    FieldStatus::Ignored("max_completion_tokens"),
    FieldStatus::Ignored("max_prompt_tokens"),
    FieldStatus::Ignored("status"),
    FieldStatus::Ignored("uptime_last_30m"),
    FieldStatus::Ignored("uptime_last_5m"),
    FieldStatus::Ignored("uptime_last_1d"),
    FieldStatus::Ignored("latency_last_30m"),
    FieldStatus::Ignored("throughput_last_30m"),
];

const ENDPOINT_PRICING_FIELDS: &[FieldStatus] = &[
    FieldStatus::Consumed("prompt"),
    FieldStatus::Consumed("completion"),
    FieldStatus::Consumed("input_cache_read"),
    FieldStatus::Consumed("input_cache_write"),
    FieldStatus::Ignored("web_search"),
    FieldStatus::Ignored("discount"),
    FieldStatus::Ignored("request"),
    FieldStatus::Ignored("image"),
];

#[tokio::test]
#[ignore = "hits openrouter; run with --run-ignored=only"]
async fn endpoints_response_matches_manifest() {
    let raw = fetch_raw_get("/models/anthropic/claude-sonnet-4.5/endpoints").await;
    assert_no_drift("/models/.../endpoints root", &raw, ENDPOINTS_TOP_LEVEL);
    let data = &raw["data"];
    assert_no_drift("/models/.../endpoints .data", data, ENDPOINTS_DATA_FIELDS);
    assert_no_drift(
        "/models/.../endpoints .data.architecture",
        &data["architecture"],
        ARCHITECTURE_FIELDS,
    );
    let endpoints = data["endpoints"]
        .as_array()
        .expect(".data.endpoints should be an array");
    assert!(!endpoints.is_empty(), "expected at least one endpoint");
    for (idx, ep) in endpoints.iter().enumerate() {
        assert_no_drift(
            &format!("/models/.../endpoints[{idx}]"),
            ep,
            ENDPOINT_ITEM_FIELDS,
        );
        assert_no_drift(
            &format!("/models/.../endpoints[{idx}].pricing"),
            &ep["pricing"],
            ENDPOINT_PRICING_FIELDS,
        );
    }
}

const VENDORED_OPENAPI: &str = include_str!("fixtures/openrouter-openapi.json");

#[tokio::test]
#[ignore = "hits openrouter; run with --run-ignored=only"]
async fn vendored_openapi_matches_live() {
    use std::collections::BTreeSet;

    let live_text = reqwest::get("https://openrouter.ai/openapi.json")
        .await
        .expect("openapi fetch failed")
        .text()
        .await
        .expect("openapi body read failed");

    let live: serde_json::Value = serde_json::from_str(&live_text).expect("live openapi parses");
    let vendored: serde_json::Value =
        serde_json::from_str(VENDORED_OPENAPI).expect("vendored openapi parses");

    if live == vendored {
        return;
    }

    let live_paths: BTreeSet<&str> = live["paths"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let vendored_paths: BTreeSet<&str> = vendored["paths"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let added_paths: Vec<&&str> = live_paths.difference(&vendored_paths).collect();
    let removed_paths: Vec<&&str> = vendored_paths.difference(&live_paths).collect();

    let live_schemas: BTreeSet<&str> = live["components"]["schemas"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let vendored_schemas: BTreeSet<&str> = vendored["components"]["schemas"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let added_schemas: Vec<&&str> = live_schemas.difference(&vendored_schemas).collect();
    let removed_schemas: Vec<&&str> = vendored_schemas.difference(&live_schemas).collect();

    let mut report = String::from("openrouter spec drift detected");
    if !added_paths.is_empty() {
        report.push_str(&format!("\n  new paths:        {added_paths:?}"));
    }
    if !removed_paths.is_empty() {
        report.push_str(&format!("\n  removed paths:    {removed_paths:?}"));
    }
    if !added_schemas.is_empty() {
        report.push_str(&format!("\n  new schemas:      {added_schemas:?}"));
    }
    if !removed_schemas.is_empty() {
        report.push_str(&format!("\n  removed schemas:  {removed_schemas:?}"));
    }
    if added_paths.is_empty()
        && removed_paths.is_empty()
        && added_schemas.is_empty()
        && removed_schemas.is_empty()
    {
        report.push_str("\n  (no path/schema additions or removals; field-level changes within existing schemas)");
    }
    report.push_str(
        "\n  refresh: curl -s https://openrouter.ai/openapi.json \\
        > crates/mush-ai/tests/fixtures/openrouter-openapi.json",
    );
    panic!("{report}");
}
