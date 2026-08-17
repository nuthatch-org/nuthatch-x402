//! x402 payment boundary for Nuthatch's named-query surface.
//!
//! This crate deliberately prices `GET /q/<name>`, never arbitrary `/sql`.
//! Named queries are bounded, typed resources; free-form SQL is neither.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::SocketAddr, path::Path as StdPath, sync::Arc};

pub const PAYMENT_REQUIRED: &str = "payment-required";
pub const PAYMENT_SIGNATURE: &str = "payment-signature";

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub provider: Provider,
    #[serde(rename = "query")]
    pub queries: Vec<PricedQuery>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Provider {
    pub upstream: String,
    pub network: String,
    pub asset: String,
    pub pay_to: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PricedQuery {
    pub name: String,
    pub price_atomic: String,
    pub description: String,
}

impl Config {
    pub fn load(path: impl AsRef<StdPath>) -> Result<Self> {
        let source = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("read {}", path.as_ref().display()))?;
        let config: Self = toml::from_str(&source).context("parse x402 configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        for (label, value) in [
            ("provider.upstream", &self.provider.upstream),
            ("provider.network", &self.provider.network),
            ("provider.asset", &self.provider.asset),
            ("provider.pay_to", &self.provider.pay_to),
        ] {
            if value.trim().is_empty() || value.contains("YourMerchant") {
                bail!("{label} must be configured")
            }
        }
        if self.queries.is_empty() {
            bail!("at least one [[query]] is required")
        }
        for query in &self.queries {
            if query.name.is_empty()
                || !query
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                bail!("query names must use lowercase letters, digits, '-' or '_'")
            }
            if query
                .price_atomic
                .parse::<u128>()
                .ok()
                .filter(|n| *n > 0)
                .is_none()
            {
                bail!("query {} has an invalid positive price_atomic", query.name)
            }
        }
        Ok(())
    }
}

/// The x402 v2 payment-requirements envelope. It is carried in the
/// `PAYMENT-REQUIRED` header as base64 JSON, with the JSON body retained for
/// humans and clients which surface error bodies.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    pub x402_version: u8,
    pub accepts: Vec<PaymentOption>,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentOption {
    pub scheme: String,
    pub network: String,
    pub asset: String,
    pub pay_to: String,
    pub max_amount_required: String,
    pub resource: String,
    pub description: String,
    pub mime_type: String,
}

impl PaymentRequired {
    fn for_query(provider: &Provider, query: &PricedQuery) -> Self {
        Self {
            x402_version: 2,
            accepts: vec![PaymentOption {
                scheme: "exact".into(),
                network: provider.network.clone(),
                asset: provider.asset.clone(),
                pay_to: provider.pay_to.clone(),
                max_amount_required: query.price_atomic.clone(),
                resource: format!("/q/{}", query.name),
                description: query.description.clone(),
                mime_type: "application/json".into(),
            }],
            error: "payment required".into(),
        }
    }
}

/// The only authority which may turn a PAYMENT-SIGNATURE into access. A real
/// implementation must verify and settle through an x402 facilitator and
/// reject replayed identifiers. The sidecar never accepts an amount supplied
/// by its caller.
#[async_trait]
pub trait Facilitator: Send + Sync + 'static {
    async fn verify_and_settle(
        &self,
        requirements: &PaymentRequired,
        signature: &str,
    ) -> Result<()>;
}

/// Production-safe default until a concrete facilitator adapter is installed.
pub struct UnconfiguredFacilitator;

#[async_trait]
impl Facilitator for UnconfiguredFacilitator {
    async fn verify_and_settle(&self, _: &PaymentRequired, _: &str) -> Result<()> {
        bail!("no x402 facilitator is configured; refusing to accept a payment signature")
    }
}

#[async_trait]
pub trait QueryBackend: Send + Sync + 'static {
    async fn execute(&self, name: &str, args: &BTreeMap<String, String>)
    -> Result<BackendResponse>;
}

#[derive(Debug)]
pub struct BackendResponse {
    pub status: StatusCode,
    pub content_type: Option<HeaderValue>,
    pub body: Vec<u8>,
}

pub struct NuthatchHttpBackend {
    base_url: String,
    client: reqwest::Client,
}

impl NuthatchHttpBackend {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl QueryBackend for NuthatchHttpBackend {
    async fn execute(
        &self,
        name: &str,
        args: &BTreeMap<String, String>,
    ) -> Result<BackendResponse> {
        let response = self
            .client
            .get(format!("{}/q/{name}", self.base_url))
            .query(args)
            .send()
            .await
            .context("call Nuthatch named-query endpoint")?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).context("invalid upstream status")?;
        let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
        let body = response
            .bytes()
            .await
            .context("read Nuthatch response")?
            .to_vec();
        Ok(BackendResponse {
            status,
            content_type,
            body,
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    provider: Provider,
    queries: Arc<BTreeMap<String, PricedQuery>>,
    facilitator: Arc<dyn Facilitator>,
    backend: Arc<dyn QueryBackend>,
}

impl AppState {
    pub fn new(
        config: Config,
        facilitator: Arc<dyn Facilitator>,
        backend: Arc<dyn QueryBackend>,
    ) -> Self {
        let queries = config
            .queries
            .into_iter()
            .map(|q| (q.name.clone(), q))
            .collect();
        Self {
            provider: config.provider,
            queries: Arc::new(queries),
            facilitator,
            backend,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/q/{name}", get(query))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

async fn query(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(args): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(priced) = state.queries.get(&name) else {
        return (StatusCode::NOT_FOUND, "unknown paid query").into_response();
    };
    let required = PaymentRequired::for_query(&state.provider, priced);
    let Some(signature) = headers
        .get(PAYMENT_SIGNATURE)
        .and_then(|value| value.to_str().ok())
    else {
        return payment_required(&required);
    };
    if let Err(error) = state
        .facilitator
        .verify_and_settle(&required, signature)
        .await
    {
        tracing::warn!(query = %name, %error, "x402 payment rejected");
        return payment_rejected(&required, error.to_string());
    }
    match state.backend.execute(&name, &args).await {
        Ok(result) => backend_response(result),
        Err(error) => {
            tracing::error!(query = %name, %error, "Nuthatch query backend failed");
            (StatusCode::BAD_GATEWAY, "Nuthatch query backend failed").into_response()
        }
    }
}

fn requirements_header(required: &PaymentRequired) -> HeaderValue {
    let json = serde_json::to_vec(required).expect("payment requirements are serializable");
    HeaderValue::from_str(&BASE64.encode(json)).expect("base64 is a valid header value")
}

fn payment_required(required: &PaymentRequired) -> Response {
    let mut response = (StatusCode::PAYMENT_REQUIRED, axum::Json(required)).into_response();
    response
        .headers_mut()
        .insert(PAYMENT_REQUIRED, requirements_header(required));
    response
}

fn payment_rejected(required: &PaymentRequired, error: String) -> Response {
    let mut rejected = required.clone();
    rejected.error = error;
    payment_required(&rejected)
}

fn backend_response(result: BackendResponse) -> Response {
    let mut response = Response::new(Body::from(result.body));
    *response.status_mut() = result.status;
    if let Some(content_type) = result.content_type {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    response
}

pub async fn serve(address: SocketAddr, state: AppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .context("bind listener")?;
    axum::serve(listener, router(state))
        .await
        .context("serve x402 sidecar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    struct TestFacilitator;
    #[async_trait]
    impl Facilitator for TestFacilitator {
        async fn verify_and_settle(&self, _: &PaymentRequired, signature: &str) -> Result<()> {
            if signature == "test-proof" {
                Ok(())
            } else {
                bail!("invalid test proof")
            }
        }
    }
    struct TestBackend;
    #[async_trait]
    impl QueryBackend for TestBackend {
        async fn execute(
            &self,
            name: &str,
            _: &BTreeMap<String, String>,
        ) -> Result<BackendResponse> {
            Ok(BackendResponse {
                status: StatusCode::OK,
                content_type: Some(HeaderValue::from_static("application/json")),
                body: format!(r#"{{"query":"{name}"}}"#).into_bytes(),
            })
        }
    }
    fn app() -> Router {
        let config: Config = toml::from_str(
            r#"
            [provider]
            upstream = "http://127.0.0.1:8288"
            network = "eip155:84532"
            asset = "0x036CbD53842c5426634e7929541eC2318f3dCF7e"
            pay_to = "0x3CB9B3bBfde8501f411bB69Ad3DC07908ED0dE20"
            [[query]]
            name = "latest_transfers"
            price_atomic = "2000"
            description = "latest transfers"
        "#,
        )
        .unwrap();
        router(AppState::new(
            config,
            Arc::new(TestFacilitator),
            Arc::new(TestBackend),
        ))
    }

    #[tokio::test]
    async fn unpaid_query_returns_x402_header_and_body() {
        let response = app()
            .oneshot(
                Request::get("/q/latest_transfers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let encoded = response
            .headers()
            .get(PAYMENT_REQUIRED)
            .unwrap()
            .to_str()
            .unwrap();
        let required: PaymentRequired =
            serde_json::from_slice(&BASE64.decode(encoded).unwrap()).unwrap();
        assert_eq!(required.x402_version, 2);
        assert_eq!(required.accepts[0].network, "eip155:84532");
        assert_eq!(required.accepts[0].max_amount_required, "2000");
    }

    #[tokio::test]
    async fn paid_query_forwards_only_after_facilitator_accepts() {
        let response = app()
            .oneshot(
                Request::get("/q/latest_transfers")
                    .header(PAYMENT_SIGNATURE, "test-proof")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }

    #[tokio::test]
    async fn bad_proof_remains_a_payment_required_response() {
        let response = app()
            .oneshot(
                Request::get("/q/latest_transfers")
                    .header(PAYMENT_SIGNATURE, "replay-me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(response.headers().contains_key(PAYMENT_REQUIRED));
    }
}
