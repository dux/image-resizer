use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{Action, ApiError, ApiResult, Auth, Ctx, Out};
use crate::referers::DomainStat;

#[derive(Deserialize, JsonSchema)]
pub struct List {}

impl Action for List {
    const PATH: &'static str = "referer/list";
    const DOC: &'static str =
        "Request totals per referer base domain, busiest first, with the disabled flag.";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = false;
    type Output = Vec<DomainStat>;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Vec<DomainStat>>> {
        Ok(ctx
            .blocking(|app| app.referers.stats(&app.store))
            .await?
            .into())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Toggle {
    /// Referer base domain, e.g. `example.com`.
    pub domain: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ToggleOut {
    pub domain: String,
    pub disabled: bool,
}

impl Action for Toggle {
    const PATH: &'static str = "referer/toggle";
    const DOC: &'static str = "Disables or re-enables resizing for pages on a referer domain (`direct` and `hidden` cannot be toggled).";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = true;
    type Output = ToggleOut;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<ToggleOut>> {
        let domain = self.domain.trim().to_lowercase();
        if domain.is_empty() {
            return Err(ApiError::invalid("Domain is required."));
        }
        if domain == "direct" || domain == "hidden" {
            return Err(ApiError::invalid(
                "Cannot toggle status for special domains.",
            ));
        }
        let d = domain.clone();
        let disabled = ctx
            .blocking(move |app| app.referers.toggle(&app.store, &d))
            .await?;
        tracing::info!(%domain, disabled, by = %ctx.email(), "referer domain toggled");
        Ok(ToggleOut { domain, disabled }.into())
    }
}
