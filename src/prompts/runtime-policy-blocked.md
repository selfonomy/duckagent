Tool error: policy_blocked
blocked_by: {{blocked_by}}
retryable: false
capability: {{capability}}
requested_access: {{requested_access}}
target: {{target}}
sandbox_preset: {{sandbox_preset}}
effective_access: {{effective_access}}

{{detail}}

The active sandbox blocked this action. Do not retry the same {{requested_access}} access through an equivalent tool or command.

Do not infer a special path class from command text or process stderr. If access is still needed, use `request_filesystem_access` only when the user provides or confirms a concrete file or directory path and the previous result was not itself from `request_filesystem_access`.
