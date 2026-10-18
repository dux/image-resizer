use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::registry::{ActionDoc, docs};
use crate::api::{Action, ApiResult, Auth, Ctx, Out};

/// Liveness and version.
#[derive(Deserialize, JsonSchema)]
pub struct Health {}

#[derive(Serialize, JsonSchema)]
pub struct HealthOut {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
}

impl Action for Health {
    const PATH: &'static str = "sys/health";
    const DOC: &'static str = "Liveness, version and uptime.";
    const AUTH: Auth = Auth::Public;
    const WRITE: bool = false;
    type Output = HealthOut;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<HealthOut>> {
        Ok(HealthOut {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
            uptime_secs: ctx.app.started.elapsed().as_secs(),
        }
        .into())
    }
}

/// The action registry with params and output schemas, for the API explorer.
#[derive(Deserialize, JsonSchema)]
pub struct Schema {}

impl Action for Schema {
    const PATH: &'static str = "sys/schema";
    const DOC: &'static str =
        "Every action with its auth, write flag, params and output JSON schemas.";
    const AUTH: Auth = Auth::Public;
    const WRITE: bool = false;
    type Output = Vec<ActionDoc>;

    async fn run(self, _ctx: &Ctx) -> ApiResult<Out<Vec<ActionDoc>>> {
        Ok(docs().into())
    }
}
