use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{Action, ApiError, ApiResult, Auth, Ctx, Out};

const MAX_TRIES: usize = 10;

#[derive(Deserialize, JsonSchema)]
pub struct Random {}

#[derive(Serialize, JsonSchema)]
pub struct RandomOut {
    pub url: String,
}

impl Action for Random {
    const PATH: &'static str = "demo/random";
    const DOC: &'static str =
        "A random reachable image URL from static/random-images.txt, for the demo page.";
    const AUTH: Auth = Auth::Public;
    const WRITE: bool = false;
    type Output = RandomOut;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<RandomOut>> {
        let list = tokio::fs::read_to_string("static/random-images.txt")
            .await
            .map_err(ApiError::internal)?;
        let mut urls: Vec<&str> = list
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if urls.is_empty() {
            return Err(ApiError::not_found("No images in random-images.txt."));
        }
        rand::seq::SliceRandom::shuffle(urls.as_mut_slice(), &mut rand::rng());
        for url in urls.iter().take(MAX_TRIES) {
            if ctx.app.fetcher.reachable(url).await {
                return Ok(RandomOut {
                    url: (*url).to_string(),
                }
                .into());
            }
            tracing::info!(%url, "demo image not reachable");
        }
        Ok(RandomOut {
            url: urls[0].to_string(),
        }
        .into())
    }
}
