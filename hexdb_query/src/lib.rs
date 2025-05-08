use async_graphql::{Context, Object, Schema, EmptyMutation, EmptySubscription};

#[derive(Default)]
pub struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn hello(&self, _ctx: &Context<'_>) -> &str {
        "Welcome to HexDB"
    }
}

pub type HexDBSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

pub fn build_schema() -> HexDBSchema {
    Schema::build(QueryRoot::default(), EmptyMutation, EmptySubscription).finish()
}
