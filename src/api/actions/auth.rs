use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{Action, ApiResult, Auth, Ctx, Out};
use crate::auth::Via;
use crate::config::AppEnv;

#[derive(Deserialize, JsonSchema)]
pub struct Me {}

#[derive(Serialize, JsonSchema)]
pub struct MeOut {
    /// Signed-in admin email, or null.
    pub email: Option<String>,
    pub is_admin: bool,
    /// `session` or `key`.
    pub via: Option<&'static str>,
    /// `/dev/login` is available (not production).
    pub dev_login: bool,
}

impl Action for Me {
    const PATH: &'static str = "auth/me";
    const DOC: &'static str = "Who is calling: the signed-in admin, or nobody.";
    const AUTH: Auth = Auth::Public;
    const WRITE: bool = false;
    type Output = MeOut;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<MeOut>> {
        let via = match &ctx.principal {
            crate::auth::Principal::Admin {
                via: Via::Session, ..
            } => Some("session"),
            crate::auth::Principal::Admin {
                via: Via::Key { .. },
                ..
            } => Some("key"),
            crate::auth::Principal::Anonymous => None,
        };
        Ok(MeOut {
            email: ctx.principal.email().map(String::from),
            is_admin: via.is_some(),
            via,
            dev_login: ctx.app.config.env != AppEnv::Production,
        }
        .into())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Logout {}

impl Action for Logout {
    const PATH: &'static str = "auth/logout";
    const DOC: &'static str = "Signs the browser out (clears the session cookie).";
    const AUTH: Auth = Auth::Session;
    const WRITE: bool = true;
    type Output = bool;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<bool>> {
        Ok(Out::from(true).cookie(crate::auth::authcog::logout_cookie(&ctx.app, &ctx.headers)))
    }
}
