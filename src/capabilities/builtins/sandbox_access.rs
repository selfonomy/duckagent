use super::{BuiltinToolSpec, schema_value};
use crate::approval::ApprovalProvider;
use anyhow::Result;
use schemars::schema_for;
use serde_json::Value;
use std::sync::Arc;

pub fn spec() -> BuiltinToolSpec {
    BuiltinToolSpec {
        name: "request_filesystem_access",
        description: "Ask the user to grant additional sandbox filesystem access after a tool result is blocked by sandbox policy. Use `ro` for read-only access and `rw` for writes.",
        input_schema: schema_value(schema_for!(
            crate::sandbox::access_request::RequestFilesystemAccessArgs
        )),
    }
}

pub fn execute(args: Value, approval_provider: Arc<dyn ApprovalProvider>) -> Result<String> {
    crate::sandbox::access_request::execute(args, approval_provider)
}
