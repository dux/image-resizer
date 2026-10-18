use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{Action, ApiError, ApiResult, Auth, Ctx, Out};
use crate::auth::keys::KeyView;

#[derive(Deserialize, JsonSchema)]
pub struct List {}

impl Action for List {
    const PATH: &'static str = "key/list";
    const DOC: &'static str = "API keys (never their tokens).";
    const AUTH: Auth = Auth::Session;
    const WRITE: bool = false;
    type Output = Vec<KeyView>;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Vec<KeyView>>> {
        Ok(ctx.blocking(|app| app.keys.list(&app.store)).await?.into())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Create {
    pub name: String,
    /// Read-only keys may only call actions that change nothing.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct Created {
    pub key: KeyView,
    /// Send as `Authorization: Bearer <token>`; shown only once.
    pub token: String,
}

impl Action for Create {
    const PATH: &'static str = "key/create";
    const DOC: &'static str =
        "Creates an API key; the token is in the answer and never shown again.";
    const AUTH: Auth = Auth::Session;
    const WRITE: bool = true;
    type Output = Created;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Created>> {
        let name = self.name.trim().to_string();
        if name.is_empty() || name.len() > 100 {
            return Err(ApiError::invalid(
                "name is required (at most 100 characters).",
            ));
        }
        let (read_only, by) = (self.read_only, ctx.email().to_string());
        let (key, token) = ctx
            .blocking(move |app| app.keys.create(&app.store, &name, read_only, &by))
            .await?;
        tracing::info!(id = key.id, name = %key.name, read_only, by = %ctx.email(), "api key created");
        Ok(Created { key, token }.into())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Delete {
    pub id: i64,
}

#[derive(Serialize, JsonSchema)]
pub struct DeleteOut {
    pub deleted: bool,
}

impl Action for Delete {
    const PATH: &'static str = "key/delete";
    const DOC: &'static str = "Deletes an API key; it stops working at once.";
    const AUTH: Auth = Auth::Session;
    const WRITE: bool = true;
    type Output = DeleteOut;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<DeleteOut>> {
        let id = self.id;
        let deleted = ctx
            .blocking(move |app| app.keys.delete(&app.store, id))
            .await?;
        if !deleted {
            return Err(ApiError::not_found(format!("No API key {id}.")));
        }
        tracing::info!(id, by = %ctx.email(), "api key deleted");
        Ok(DeleteOut { deleted }.into())
    }
}
