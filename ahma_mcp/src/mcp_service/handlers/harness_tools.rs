use super::common::{mcp_internal, mcp_invalid_params, text_result};
use crate::AhmaMcpService;
use rmcp::model::{CallToolResult, ErrorData as McpError};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

impl AhmaMcpService {
    pub async fn handle_read_file(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'path' is required"))?;
        let start_line = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let end_line = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let result = ahma_harness_tools::read_file(&scopes, Path::new(path), start_line, end_line)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        Ok(text_result(result))
    }

    pub async fn handle_list_dir(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let scopes = self.adapter.sandbox().scopes().to_vec();

        let entries = ahma_harness_tools::list_dir(&scopes, Path::new(path))
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;
        let body = serde_json::to_string_pretty(&entries)
            .map_err(|e| mcp_internal(format!("Failed to serialize list_dir result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_file_search(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'pattern' is required"))?;

        let base_dir = args
            .get("base_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.adapter.sandbox().scopes().first().cloned())
            .unwrap_or_else(|| PathBuf::from("."));

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let matches = ahma_harness_tools::file_search(&scopes, &base_dir, pattern)
            .map_err(|e| mcp_internal(e.to_string()))?;

        let body = serde_json::to_string_pretty(&matches)
            .map_err(|e| mcp_internal(format!("Failed to serialize file_search result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_grep_search(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'query' is required"))?;
        let is_regex = args
            .get("is_regex")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let include_pattern = args.get("include_pattern").and_then(Value::as_str);
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let base_dir = args
            .get("base_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.adapter.sandbox().scopes().first().cloned())
            .unwrap_or_else(|| PathBuf::from("."));

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let matches = ahma_harness_tools::grep_search(
            &scopes,
            &base_dir,
            query,
            is_regex,
            include_pattern,
            max_results,
        )
        .map_err(|e| mcp_internal(e.to_string()))?;

        let body = serde_json::to_string_pretty(&matches)
            .map_err(|e| mcp_internal(format!("Failed to serialize grep_search result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_fetch_webpage(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'url' is required"))?;
        let query = args.get("query").and_then(Value::as_str);

        let result = ahma_harness_tools::fetch_webpage(url, query)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        let body = serde_json::to_string_pretty(&result)
            .map_err(|e| mcp_internal(format!("Failed to serialize fetch_webpage result: {e}")))?;
        Ok(text_result(body))
    }

    pub async fn handle_write_file(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'path' is required"))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'content' is required"))?;

        let scopes = self.adapter.sandbox().scopes().to_vec();
        ahma_harness_tools::write_file(&scopes, Path::new(path), content)
            .await
            .map_err(|e| mcp_internal(e.to_string()))?;

        Ok(text_result("File written"))
    }

    pub async fn handle_replace_in_file(
        &self,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'path' is required"))?;
        let old_str = args
            .get("old_str")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'old_str' is required"))?;
        let new_str = args
            .get("new_str")
            .and_then(Value::as_str)
            .ok_or_else(|| mcp_invalid_params("'new_str' is required"))?;

        let scopes = self.adapter.sandbox().scopes().to_vec();
        let replaced =
            ahma_harness_tools::replace_in_file(&scopes, Path::new(path), old_str, new_str)
                .await
                .map_err(|e| mcp_internal(e.to_string()))?;

        Ok(text_result(format!("Replaced {replaced} occurrence(s)")))
    }
}

use crate::mcp_service::schema;
use std::sync::Arc;

pub fn read_file_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Absolute or scoped-relative file path."}),
    );
    props.insert(
        "start_line".to_string(),
        json!({"type": "integer", "description": "1-based inclusive start line."}),
    );
    props.insert(
        "end_line".to_string(),
        json!({"type": "integer", "description": "1-based inclusive end line."}),
    );
    schema::object_input_schema(props, &["path"])
}

pub fn list_dir_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Directory path. Defaults to current scope root."}),
    );
    schema::object_input_schema(props, &[])
}

pub fn file_search_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "pattern".to_string(),
        json!({"type": "string", "description": "Glob pattern, e.g. '**/*.rs'."}),
    );
    props.insert(
        "base_dir".to_string(),
        json!({"type": "string", "description": "Base directory for glob search."}),
    );
    schema::object_input_schema(props, &["pattern"])
}

pub fn grep_search_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "query".to_string(),
        json!({"type": "string", "description": "Search query (regex or plain text)."}),
    );
    props.insert(
        "is_regex".to_string(),
        json!({"type": "boolean", "description": "Interpret query as regex.", "default": false}),
    );
    props.insert(
        "base_dir".to_string(),
        json!({"type": "string", "description": "Directory root to search from."}),
    );
    props.insert(
        "include_pattern".to_string(),
        json!({"type": "string", "description": "Optional glob filter for files."}),
    );
    props.insert(
        "max_results".to_string(),
        json!({"type": "integer", "description": "Maximum number of matches to return."}),
    );
    schema::object_input_schema(props, &["query"])
}

pub fn fetch_webpage_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "url".to_string(),
        json!({"type": "string", "description": "HTTP/HTTPS URL to fetch."}),
    );
    props.insert(
        "query".to_string(),
        json!({"type": "string", "description": "Optional query to filter extracted text."}),
    );
    schema::object_input_schema(props, &["url"])
}

pub fn write_file_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Absolute or scoped-relative file path."}),
    );
    props.insert(
        "content".to_string(),
        json!({"type": "string", "description": "UTF-8 content to write."}),
    );
    schema::object_input_schema(props, &["path", "content"])
}

pub fn replace_in_file_schema() -> Arc<Map<String, Value>> {
    let mut props = Map::new();
    props.insert(
        "path".to_string(),
        json!({"type": "string", "description": "Absolute or scoped-relative file path."}),
    );
    props.insert(
        "old_str".to_string(),
        json!({"type": "string", "description": "Exact string to replace."}),
    );
    props.insert(
        "new_str".to_string(),
        json!({"type": "string", "description": "Replacement string."}),
    );
    schema::object_input_schema(props, &["path", "old_str", "new_str"])
}
