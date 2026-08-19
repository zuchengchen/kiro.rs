//! Amazon Q Developer CLI-compatible streaming endpoint.

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::ide::{RUNTIME_ENDPOINT_NAME, normalize_api_region};
use super::{KiroEndpoint, RequestContext, transform_streaming_payload};
use crate::kiro::kiro_version;

/// Legacy configuration alias retained for existing API-key credentials.
pub const CLI_ENDPOINT_NAME: &str = "cli";
pub const AMAZON_Q_CLI_ENDPOINT_NAME: &str = "amazonq-cli";

pub struct CliEndpoint;

impl CliEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region(&self, ctx: &RequestContext<'_>) -> &'static str {
        normalize_api_region(ctx.credentials.effective_api_region(ctx.config))
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }

    fn common_headers(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("TokenType", token_type);
        }
        req
    }
}

impl Default for CliEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CliEndpoint {
    fn name(&self) -> &'static str {
        AMAZON_Q_CLI_ENDPOINT_NAME
    }

    fn display_name(&self) -> &'static str {
        "AmazonQCLI"
    }

    fn fallback_name(&self) -> Option<&'static str> {
        Some(RUNTIME_ENDPOINT_NAME)
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/SendMessageStreaming", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.common_headers(req, ctx).header(
            "x-amz-target",
            "AmazonQDeveloperStreamingService.SendMessage",
        )
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = self.common_headers(req, ctx);
        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        transform_streaming_payload(
            body,
            ctx.credentials.streaming_profile_arn().as_deref(),
            "CLI",
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_payload_sets_origin_and_strips_unsupported_state() {
        let body = r#"{
            "conversationState": {
                "agentContinuationId": "continue",
                "agentTaskType": "vibe",
                "currentMessage": {"userInputMessage": {"origin": "AI_EDITOR"}},
                "history": [{"userInputMessage": {"origin": "AI_EDITOR", "modelId": "friendly"}}]
            }
        }"#;
        let result = transform_streaming_payload(body, Some("arn:test"), "CLI", true);
        let value: serde_json::Value = serde_json::from_str(&result).unwrap();
        let state = &value["conversationState"];
        assert!(state.get("agentContinuationId").is_none());
        assert!(state.get("agentTaskType").is_none());
        assert_eq!(state["currentMessage"]["userInputMessage"]["origin"], "CLI");
        assert_eq!(state["history"][0]["userInputMessage"]["origin"], "CLI");
        assert_eq!(
            state["history"][0]["userInputMessage"]["modelId"],
            "friendly"
        );
        assert_eq!(value["profileArn"], "arn:test");
    }
}
