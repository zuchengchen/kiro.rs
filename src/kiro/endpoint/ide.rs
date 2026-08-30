//! Kiro editor-compatible streaming endpoints.
//!
//! CodeWhisperer, Amazon Q, and Kiro Runtime use the same editor payload shape,
//! but their hosts are backed by independent rate-limit buckets.

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext, transform_streaming_payload};
use crate::kiro::kiro_version;

/// Legacy configuration alias retained for existing installations.
pub const IDE_ENDPOINT_NAME: &str = "ide";
pub const CODEWHISPERER_ENDPOINT_NAME: &str = "codewhisperer";
pub const AMAZON_Q_ENDPOINT_NAME: &str = "amazonq";
pub const RUNTIME_ENDPOINT_NAME: &str = "runtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorEndpointKind {
    CodeWhisperer,
    AmazonQ,
    KiroRuntime,
}

pub struct IdeEndpoint {
    kind: EditorEndpointKind,
}

impl IdeEndpoint {
    /// The legacy `ide` endpoint now starts with the CodeWhisperer bucket.
    pub fn new() -> Self {
        Self::codewhisperer()
    }

    pub fn codewhisperer() -> Self {
        Self {
            kind: EditorEndpointKind::CodeWhisperer,
        }
    }

    pub fn amazon_q() -> Self {
        Self {
            kind: EditorEndpointKind::AmazonQ,
        }
    }

    pub fn runtime() -> Self {
        Self {
            kind: EditorEndpointKind::KiroRuntime,
        }
    }

    fn api_region(&self, ctx: &RequestContext<'_>) -> &'static str {
        normalize_api_region(ctx.credentials.effective_api_region(ctx.config))
    }

    fn api_host(&self, ctx: &RequestContext<'_>) -> String {
        let region = self.api_region(ctx);
        match self.kind {
            EditorEndpointKind::CodeWhisperer if region == "us-east-1" => {
                "codewhisperer.us-east-1.amazonaws.com".to_string()
            }
            // codewhisperer.eu-central-1.amazonaws.com does not exist; the EU
            // editor service is carried by q.eu-central-1.amazonaws.com.
            EditorEndpointKind::CodeWhisperer | EditorEndpointKind::AmazonQ => {
                format!("q.{}.amazonaws.com", region)
            }
            EditorEndpointKind::KiroRuntime => format!("runtime.{}.kiro.dev", region),
        }
    }

    fn mcp_host(&self, ctx: &RequestContext<'_>) -> String {
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

    fn common_headers(
        &self,
        req: RequestBuilder,
        ctx: &RequestContext<'_>,
        host: String,
    ) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-kiro-agent-mode", &ctx.config.agent_mode)
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", host)
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("TokenType", token_type);
        }
        req
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        match self.kind {
            EditorEndpointKind::CodeWhisperer => CODEWHISPERER_ENDPOINT_NAME,
            EditorEndpointKind::AmazonQ => AMAZON_Q_ENDPOINT_NAME,
            EditorEndpointKind::KiroRuntime => RUNTIME_ENDPOINT_NAME,
        }
    }

    fn display_name(&self) -> &'static str {
        match self.kind {
            EditorEndpointKind::CodeWhisperer => "CodeWhisperer",
            EditorEndpointKind::AmazonQ => "AmazonQ",
            EditorEndpointKind::KiroRuntime => "KiroRuntime",
        }
    }

    fn fallback_name(&self) -> Option<&'static str> {
        match self.kind {
            EditorEndpointKind::KiroRuntime => Some(CODEWHISPERER_ENDPOINT_NAME),
            EditorEndpointKind::CodeWhisperer | EditorEndpointKind::AmazonQ => {
                Some(RUNTIME_ENDPOINT_NAME)
            }
        }
    }

    fn requires_codewhisperer_model_id(&self) -> bool {
        self.kind == EditorEndpointKind::CodeWhisperer
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.api_host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.mcp_host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = self.common_headers(req, ctx, self.api_host(ctx)).header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        );
        if self.kind == EditorEndpointKind::KiroRuntime {
            req = req.header("x-amzn-codewhisperer-optout", "true");
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = self.common_headers(req, ctx, self.mcp_host(ctx));
        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        transform_streaming_payload(
            body,
            ctx.credentials.streaming_profile_arn().as_deref(),
            "AI_EDITOR",
            false,
        )
    }
}

/// The data plane has only US and EU buckets. The account-level apiRegion wins
/// before this function is called; all EU regions normalize to eu-central-1.
pub(super) fn normalize_api_region(region: &str) -> &'static str {
    if region.eq_ignore_ascii_case("eu-central-1") || region.to_ascii_lowercase().starts_with("eu-")
    {
        "eu-central-1"
    } else {
        "us-east-1"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    #[test]
    fn normalizes_supported_api_regions() {
        assert_eq!(normalize_api_region("us-east-1"), "us-east-1");
        assert_eq!(normalize_api_region("ap-southeast-1"), "us-east-1");
        assert_eq!(normalize_api_region("eu-west-1"), "eu-central-1");
    }

    #[test]
    fn endpoint_fallbacks_form_the_expected_ring() {
        let codewhisperer = IdeEndpoint::codewhisperer();
        let amazon_q = IdeEndpoint::amazon_q();
        let runtime = IdeEndpoint::runtime();
        assert_eq!(codewhisperer.fallback_name(), Some(RUNTIME_ENDPOINT_NAME));
        assert_eq!(amazon_q.fallback_name(), Some(RUNTIME_ENDPOINT_NAME));
        assert_eq!(runtime.fallback_name(), Some(CODEWHISPERER_ENDPOINT_NAME));
        assert!(codewhisperer.requires_codewhisperer_model_id());
        assert!(!runtime.requires_codewhisperer_model_id());
    }

    #[test]
    fn endpoint_urls_follow_us_and_eu_routing_rules() {
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };
        assert_eq!(
            IdeEndpoint::codewhisperer().api_url(&ctx),
            "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            IdeEndpoint::amazon_q().api_url(&ctx),
            "https://q.us-east-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            IdeEndpoint::runtime().api_url(&ctx),
            "https://runtime.us-east-1.kiro.dev/generateAssistantResponse"
        );

        let mut eu_credentials = KiroCredentials::default();
        eu_credentials.region = Some("eu-west-1".to_string());
        let eu_ctx = RequestContext {
            credentials: &eu_credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };
        assert_eq!(
            IdeEndpoint::codewhisperer().api_url(&eu_ctx),
            "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
        );
        assert_eq!(
            IdeEndpoint::runtime().api_url(&eu_ctx),
            "https://runtime.eu-central-1.kiro.dev/generateAssistantResponse"
        );
    }

    #[test]
    fn runtime_adds_only_its_required_optout_header() {
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "secret-token",
            machine_id: "machine",
            config: &config,
        };
        let client = reqwest::Client::new();

        let codewhisperer = IdeEndpoint::codewhisperer();
        let codewhisperer_request = codewhisperer
            .decorate_api(client.post(codewhisperer.api_url(&ctx)), &ctx)
            .build()
            .unwrap();
        assert_eq!(
            codewhisperer_request.headers()["x-amz-target"],
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse"
        );
        assert!(
            !codewhisperer_request
                .headers()
                .contains_key("x-amzn-codewhisperer-optout")
        );

        let runtime = IdeEndpoint::runtime();
        let runtime_request = runtime
            .decorate_api(client.post(runtime.api_url(&ctx)), &ctx)
            .build()
            .unwrap();
        assert_eq!(
            runtime_request.headers()["x-amzn-codewhisperer-optout"],
            "true"
        );
        assert_eq!(
            runtime_request.headers()["authorization"],
            "Bearer secret-token"
        );
    }
}
