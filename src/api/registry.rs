// The table of every API action; dispatch, `sys/schema` and the docs test all read it.

use std::collections::HashMap;
use std::sync::LazyLock;

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;

use super::actions::{auth, cache, demo, key, referer, stats, sys};
use super::{Action, ApiResult, Auth, Ctx, Out, Params};

pub type CallFn = for<'a> fn(&'a Ctx, Params) -> BoxFuture<'a, ApiResult<Out<Value>>>;

pub struct Entry {
    pub path: &'static str,
    pub doc: &'static str,
    pub auth: Auth,
    pub write: bool,
    pub call: CallFn,
    pub params_schema: fn() -> schemars::Schema,
    pub output_schema: fn() -> schemars::Schema,
}

fn call<A: Action>(ctx: &Ctx, params: Params) -> BoxFuture<'_, ApiResult<Out<Value>>> {
    Box::pin(async move {
        let action: A = params.parse()?;
        let out = action.run(ctx).await?;
        let data = serde_json::to_value(&out.data).map_err(super::ApiError::internal)?;
        Ok(Out {
            data,
            message: out.message,
            meta: out.meta,
            cookies: out.cookies,
        })
    })
}

fn entry<A: Action>() -> Entry {
    Entry {
        path: A::PATH,
        doc: A::DOC,
        auth: A::AUTH,
        write: A::WRITE,
        call: call::<A>,
        params_schema: || schemars::schema_for!(A),
        output_schema: || schemars::schema_for!(A::Output),
    }
}

pub static ENTRIES: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    vec![
        entry::<sys::Health>(),
        entry::<sys::Schema>(),
        entry::<auth::Me>(),
        entry::<auth::Logout>(),
        entry::<demo::Random>(),
        entry::<stats::Show>(),
        entry::<referer::List>(),
        entry::<referer::Toggle>(),
        entry::<cache::List>(),
        entry::<cache::Delete>(),
        entry::<cache::Clear>(),
        entry::<cache::Purge>(),
        entry::<key::List>(),
        entry::<key::Create>(),
        entry::<key::Delete>(),
    ]
});

static BY_PATH: LazyLock<HashMap<&'static str, &'static Entry>> =
    LazyLock::new(|| ENTRIES.iter().map(|e| (e.path, e)).collect());

pub fn find(path: &str) -> Option<&'static Entry> {
    BY_PATH.get(path).copied()
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ActionDoc {
    pub path: &'static str,
    pub doc: &'static str,
    pub auth: Auth,
    pub write: bool,
    pub params: Value,
    pub output: Value,
}

pub fn docs() -> Vec<ActionDoc> {
    ENTRIES
        .iter()
        .map(|e| ActionDoc {
            path: e.path,
            doc: e.doc,
            auth: e.auth,
            write: e.write,
            params: (e.params_schema)().to_value(),
            output: (e.output_schema)().to_value(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_documented() {
        let doc = std::fs::read_to_string("doc/api.md").unwrap();
        for e in ENTRIES.iter() {
            assert!(
                doc.contains(&format!("`{}`", e.path)),
                "{} missing from doc/api.md",
                e.path
            );
        }
        assert_eq!(BY_PATH.len(), ENTRIES.len(), "duplicate action path");
    }

    #[test]
    fn schemas_render() {
        for d in docs() {
            assert!(d.params.is_object() && d.output.is_object(), "{}", d.path);
        }
    }
}
