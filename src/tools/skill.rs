//! The `use_skill` tool: load one skill's body on demand.
//!
//! The system prompt only carries the catalog (name + description); the body
//! is fetched here when the task actually matches. Loading a skill is a read:
//! no approval, no side effects.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{arg_str, Tool, ToolCtx, ToolError};
use crate::agent::skills::Skill;

pub struct UseSkill {
    pub skills: Arc<Vec<Skill>>,
}

#[async_trait]
impl Tool for UseSkill {
    fn name(&self) -> &str {
        "use_skill"
    }

    fn description(&self) -> &str {
        "Load a project skill's full procedure. The system prompt lists available skills by name and description; call this when the task clearly matches one. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name from the catalog"}
            },
            "required": ["name"]
        })
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        let want = arg_str(args, "name")?.trim().to_ascii_lowercase();
        let skill = self.skills.iter().find(|s| s.name == want).ok_or_else(|| {
            let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
            ToolError::new(format!(
                "no skill named {want:?}; available: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            ))
        })?;
        Ok(format!(
            "# skill: {}\n{}{}\n\nsource: {}\n\n{}",
            skill.name,
            if skill.description.is_empty() {
                String::new()
            } else {
                format!("{}\n", skill.description)
            },
            if skill.truncated {
                " (body truncated)"
            } else {
                ""
            },
            skill.path.display(),
            skill.body
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skills::Scope;
    use std::path::PathBuf;

    fn tool() -> UseSkill {
        UseSkill {
            skills: Arc::new(vec![Skill {
                name: "release".into(),
                description: "Cut a release".into(),
                body: "1. bump\n2. tag".into(),
                path: PathBuf::from("/repo/.kazi/skills/release/SKILL.md"),
                scope: Scope::Project,
                truncated: false,
            }]),
        }
    }

    fn ctx() -> ToolCtx {
        crate::tools::tool_ctx(
            &crate::config::ToolsCfg::default(),
            std::path::Path::new("."),
            Arc::new(crate::tools::AutoApprover),
            crate::tools::PermissionMode::Edit,
        )
    }

    #[tokio::test]
    async fn loads_a_skill_by_name_case_insensitively() {
        let out = tool()
            .run(&json!({"name": "RELEASE"}), &ctx())
            .await
            .unwrap();
        assert!(out.contains("# skill: release"));
        assert!(out.contains("Cut a release"));
        assert!(out.contains("2. tag"));
        assert!(out.contains("SKILL.md"));
    }

    #[tokio::test]
    async fn unknown_names_list_what_exists() {
        let err = tool()
            .run(&json!({"name": "nope"}), &ctx())
            .await
            .unwrap_err();
        assert!(err.0.contains("available: release"), "{}", err.0);
    }
}
