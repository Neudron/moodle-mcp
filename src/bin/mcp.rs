use anyhow::Result;
use moodle_mcp::{moodle::Moodle, state::State, sync::sync_course, sync::SyncReport};
use rmcp::schemars::JsonSchema;
use rmcp::{
    handler::server::wrapper::Parameters, model::*, tool, tool_handler, tool_router,
    transport::stdio, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone, Default)]
pub struct MoodleMcp;

#[derive(Deserialize, JsonSchema)]
pub struct CourseId {
    /// Moodle course id, as returned by list_courses
    course_id: i64,
}

#[tool_router]
impl MoodleMcp {
    pub fn new() -> Self {
        Self
    }

    fn client() -> Result<Moodle, McpError> {
        Moodle::from_env().map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    fn root() -> String {
        Moodle::moodle_root()
    }

    #[tool(
        description = "List all Moodle courses the token can see. Returns [{id, fullname, shortname, summary}]."
    )]
    async fn list_courses(&self) -> Result<CallToolResult, McpError> {
        let m = Self::client()?;
        let info = m.site_info().await.map_err(mcp)?;
        let uid = info["userid"]
            .as_i64()
            .ok_or_else(|| McpError::internal_error("no userid".to_string(), None))?;
        let courses = m.courses(uid).await.map_err(mcp)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&courses).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Get full section/module/file structure of one course (no downloads).")]
    async fn course_contents(
        &self,
        Parameters(CourseId { course_id }): Parameters<CourseId>,
    ) -> Result<CallToolResult, McpError> {
        let m = Self::client()?;
        let sections = m.contents(course_id).await.map_err(mcp)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&sections).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Extract one Moodle course into $MOODLE_ROOT/<shortname>/ organized by RA sections. Downloads all files, writes README.md per module and course index.md."
    )]
    async fn sync_course_tool(
        &self,
        Parameters(CourseId { course_id }): Parameters<CourseId>,
    ) -> Result<CallToolResult, McpError> {
        let m = Self::client()?;
        let root = Self::root();
        let info = m.site_info().await.map_err(mcp)?;
        let uid = info["userid"]
            .as_i64()
            .ok_or_else(|| McpError::internal_error("no userid".to_string(), None))?;
        let courses = m.courses(uid).await.map_err(mcp)?;
        let Some(course) = courses.iter().find(|c| c.id == course_id) else {
            return Err(McpError::invalid_params(
                format!("course {course_id} not found"),
                None,
            ));
        };
        let mut state = State::load(&root).map_err(mcp)?;
        let report = sync_course(&m, &mut state, course, &root)
            .await
            .map_err(mcp)?;
        State::save(&state, &root).map_err(mcp)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Extract ALL visible Moodle courses into $MOODLE_ROOT/.")]
    async fn sync_all(&self) -> Result<CallToolResult, McpError> {
        let m = Self::client()?;
        let root = Self::root();
        let info = m.site_info().await.map_err(mcp)?;
        let uid = info["userid"]
            .as_i64()
            .ok_or_else(|| McpError::internal_error("no userid".to_string(), None))?;
        let courses = m.courses(uid).await.map_err(mcp)?;
        let mut state = State::load(&root).map_err(mcp)?;
        let mut out = vec![];
        for course in &courses {
            match sync_course(&m, &mut state, course, &root).await {
                Ok(SyncReport {
                    course_id,
                    shortname,
                    files_new,
                    files_skipped,
                    errors,
                }) => {
                    out.push(json!({
                        "course_id": course_id, "shortname": shortname,
                        "files_new": files_new, "files_skipped": files_skipped,
                        "errors": errors,
                    }));
                }
                Err(e) => out.push(json!({ "course_id": course.id, "error": e.to_string() })),
            }
            State::save(&state, &root).ok();
        }
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }
}

fn mcp(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

#[tool_handler]
impl ServerHandler for MoodleMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(
            "Moodle course extraction server. Use list_courses first, then sync_course or sync_all. Files land under $MOODLE_ROOT/<shortname>/<RAxx>/<nnn-module>/."
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok(); // .env optional; real env always wins
    let service = MoodleMcp::new().serve(stdio()).await.inspect_err(|e| {
        eprintln!("server error: {e}");
    })?;
    service.waiting().await?;
    Ok(())
}
