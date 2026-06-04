use crate::tools::CallCapabilityInput;
use anyhow::{Result, bail};

pub const CAPABILITIES: &[&str] = &["request_memory_review"];

pub trait MainAgentBuiltinExecutor {
    fn request_memory_review(&self, purpose: String) -> Result<String>;
}

pub fn execute_main_agent_builtin(
    input: CallCapabilityInput,
    executor: &impl MainAgentBuiltinExecutor,
) -> Result<String> {
    match input.capability.trim() {
        "request_memory_review" => {
            let purpose = parse_request_memory_review_purpose(&input)?;
            executor.request_memory_review(purpose)
        }
        "" => Ok(unavailable_main_agent_capability_result(
            "(missing capability)",
        )),
        other => Ok(unavailable_main_agent_capability_result(other)),
    }
}

fn parse_request_memory_review_purpose(input: &CallCapabilityInput) -> Result<String> {
    let purpose = input.purpose.trim();
    if purpose.is_empty() {
        bail!("request_memory_review requires non-empty call_capability.purpose");
    }
    Ok(purpose.to_string())
}

fn unavailable_main_agent_capability_result(capability: &str) -> String {
    serde_json::json!({
        "status": "unavailable",
        "agent_mode": "MainAgent",
        "capability": capability,
        "allowed_capabilities": CAPABILITIES,
        "message": format!("Capability `{capability}` is not a MainAgent-only capability. Use the runtime capabilities listed under [AVAILABLE CAPABILITIES], or request_memory_review for durable memory review.")
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeMainAgentExecutor {
        memory_review_purposes: Mutex<Vec<String>>,
    }

    impl MainAgentBuiltinExecutor for FakeMainAgentExecutor {
        fn request_memory_review(&self, purpose: String) -> Result<String> {
            self.memory_review_purposes
                .lock()
                .expect("memory review purposes mutex poisoned")
                .push(purpose.clone());
            Ok(purpose)
        }
    }

    fn request_memory_review_call(args: Value, purpose: &str) -> CallCapabilityInput {
        CallCapabilityInput {
            capability: "request_memory_review".to_string(),
            args,
            purpose: purpose.to_string(),
        }
    }

    #[test]
    fn request_memory_review_uses_call_capability_purpose_without_args() -> Result<()> {
        let executor = FakeMainAgentExecutor::default();

        let output = execute_main_agent_builtin(
            request_memory_review_call(Value::Null, "Update the user preferred name from x to n."),
            &executor,
        )?;

        assert_eq!(output, "Update the user preferred name from x to n.");
        assert_eq!(
            executor
                .memory_review_purposes
                .lock()
                .expect("memory review purposes mutex poisoned")
                .as_slice(),
            ["Update the user preferred name from x to n."]
        );
        Ok(())
    }

    #[test]
    fn request_memory_review_ignores_args_purpose() -> Result<()> {
        let executor = FakeMainAgentExecutor::default();

        let output = execute_main_agent_builtin(
            request_memory_review_call(
                json!({"purpose": "Wrong nested purpose; it should not be used."}),
                "Correct call_capability purpose.",
            ),
            &executor,
        )?;

        assert_eq!(output, "Correct call_capability purpose.");
        assert_eq!(
            executor
                .memory_review_purposes
                .lock()
                .expect("memory review purposes mutex poisoned")
                .as_slice(),
            ["Correct call_capability purpose."]
        );
        Ok(())
    }

    #[test]
    fn request_memory_review_ignores_malformed_args() -> Result<()> {
        let executor = FakeMainAgentExecutor::default();

        let output = execute_main_agent_builtin(
            request_memory_review_call(
                Value::String("not an object".to_string()),
                "User asked to update the preferred name to n.",
            ),
            &executor,
        )?;

        assert_eq!(output, "User asked to update the preferred name to n.");
        Ok(())
    }

    #[test]
    fn request_memory_review_rejects_empty_call_capability_purpose() {
        let executor = FakeMainAgentExecutor::default();

        let err = execute_main_agent_builtin(
            request_memory_review_call(
                json!({"purpose": "Nested args no longer take effect"}),
                "   ",
            ),
            &executor,
        )
        .expect_err("empty request_memory_review purpose should fail");

        assert!(
            err.to_string()
                .contains("request_memory_review requires non-empty call_capability.purpose")
        );
    }

    #[test]
    fn main_agent_direct_memory_capability_is_unavailable() -> Result<()> {
        let executor = FakeMainAgentExecutor::default();

        let output = execute_main_agent_builtin(
            CallCapabilityInput {
                capability: "patch_memory".to_string(),
                args: json!({
                    "scope": "global",
                    "title": "User preferred name",
                    "summary": "User wants to be called n.",
                    "patch": "--- memory\n+++ memory\n@@ -1 +1 @@\n-User wants to be called x.\n+User wants to be called n."
                }),
                purpose: "MainAgent should not write memory directly".to_string(),
            },
            &executor,
        )?;
        let value: Value = serde_json::from_str(&output)?;

        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["agent_mode"], "MainAgent");
        assert_eq!(value["capability"], "patch_memory");
        assert!(
            value["allowed_capabilities"]
                .as_array()
                .expect("allowed capabilities should be an array")
                .iter()
                .all(|item| item != "patch_memory")
        );
        assert!(
            executor
                .memory_review_purposes
                .lock()
                .expect("memory review purposes mutex poisoned")
                .is_empty()
        );

        Ok(())
    }
}
