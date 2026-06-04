use crate::approval::ApprovalProvider;
use crate::process_manager::execute_process_action;
use crate::session::SessionManager;
use anyhow::Result;
use serde_json::{Map, Value};
use std::sync::Arc;

pub fn start(
    session_manager: &SessionManager,
    session_id: &str,
    args: Value,
    approval_provider: Arc<dyn ApprovalProvider>,
) -> Result<String> {
    execute_process_action(
        session_manager,
        session_id,
        with_action(args, "start"),
        Some(approval_provider),
    )
}

pub fn read(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    execute_process_action(session_manager, session_id, with_action(args, "read"), None)
}

pub fn list(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    execute_process_action(session_manager, session_id, with_action(args, "list"), None)
}

pub fn stop(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    execute_process_action(session_manager, session_id, with_action(args, "stop"), None)
}

pub fn write(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    execute_process_action(
        session_manager,
        session_id,
        with_action(args, "write"),
        None,
    )
}

pub fn watch(session_manager: &SessionManager, session_id: &str, args: Value) -> Result<String> {
    execute_process_action(
        session_manager,
        session_id,
        with_action(args, "watch"),
        None,
    )
}

fn with_action(args: Value, action: &str) -> Value {
    let mut object = match args {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    object.insert("action".to_string(), Value::String(action.to_string()));
    Value::Object(object)
}
