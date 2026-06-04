use super::{BuiltinToolSpec, schema_value};
use anyhow::Result;
use schemars::schema_for;
use serde_json::Value;

pub fn specs() -> Vec<BuiltinToolSpec> {
    vec![
        BuiltinToolSpec {
            name: "load_skill",
            description: "Load an Agent Skill by name from the active profile's skills directory, returning its root path, SKILL.md path, frontmatter, and full SKILL.md content.",
            input_schema: schema_value(schema_for!(crate::skills::LoadSkillArgs)),
        },
        BuiltinToolSpec {
            name: "read_skill_file",
            description: "Read a file inside a loaded global Agent Skill root, such as references or assets, without allowing path escape.",
            input_schema: schema_value(schema_for!(crate::skills::ReadSkillFileArgs)),
        },
    ]
}

pub fn execute_load_skill(args: Value) -> Result<String> {
    crate::skills::load_skill(args)
}

pub fn execute_read_skill_file(args: Value) -> Result<String> {
    crate::skills::read_skill_file(args)
}
