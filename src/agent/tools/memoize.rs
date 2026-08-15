//! Definition caching for tools registered with the agent loop.
//!
//! Rig asks every tool for owned metadata on each completion request. Tool
//! implementations still need to return owned values, but their descriptions
//! and JSON schemas only need to be constructed once per registered instance.

use rig::completion::ToolDefinition;
use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult};
use rig::wasm_compat::WasmBoxedFuture;

struct DefinitionMemoizedTool {
    inner: Box<dyn ToolDyn>,
    definition: ToolDefinition,
}

impl DefinitionMemoizedTool {
    fn new(inner: Box<dyn ToolDyn>) -> Self {
        let definition = ToolDefinition {
            name: inner.name(),
            description: inner.description(),
            parameters: inner.parameters(),
        };
        Self { inner, definition }
    }
}

impl ToolDyn for DefinitionMemoizedTool {
    fn name(&self) -> String {
        self.definition.name.clone()
    }

    fn description(&self) -> String {
        self.definition.description.clone()
    }

    fn parameters(&self) -> serde_json::Value {
        self.definition.parameters.clone()
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        self.inner.call(args)
    }

    fn call_with_extensions<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        self.inner.call_with_extensions(args, extensions)
    }

    fn call_structured<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, ToolExecutionResult> {
        self.inner.call_structured(args, extensions)
    }
}

pub(crate) fn definitions(tools: Vec<Box<dyn ToolDyn>>) -> Vec<Box<dyn ToolDyn>> {
    tools
        .into_iter()
        .map(|tool| Box::new(DefinitionMemoizedTool::new(tool)) as Box<dyn ToolDyn>)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rig::tool::Tool;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Args {
        value: String,
    }

    struct CountingTool {
        descriptions: Arc<AtomicUsize>,
        parameters: Arc<AtomicUsize>,
    }

    impl Tool for CountingTool {
        const NAME: &'static str = "counting";
        type Error = std::convert::Infallible;
        type Args = Args;
        type Output = String;

        fn description(&self) -> String {
            self.descriptions.fetch_add(1, Ordering::SeqCst);
            "Count metadata construction".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            self.parameters.fetch_add(1, Ordering::SeqCst);
            serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            })
        }

        async fn call(&self, args: Args) -> Result<String, Self::Error> {
            Ok(args.value)
        }
    }

    #[tokio::test]
    async fn constructs_definition_once_and_preserves_calls() {
        let descriptions = Arc::new(AtomicUsize::new(0));
        let parameters = Arc::new(AtomicUsize::new(0));
        let tool = DefinitionMemoizedTool::new(Box::new(CountingTool {
            descriptions: Arc::clone(&descriptions),
            parameters: Arc::clone(&parameters),
        }));

        let expected_schema = serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        });
        assert_eq!(tool.description(), "Count metadata construction");
        assert_eq!(tool.description(), "Count metadata construction");
        assert_eq!(tool.parameters(), expected_schema);
        assert_eq!(tool.parameters(), expected_schema);
        assert_eq!(descriptions.load(Ordering::SeqCst), 1);
        assert_eq!(parameters.load(Ordering::SeqCst), 1);

        let extensions = ToolCallExtensions::new();
        let result = tool
            .call_structured(r#"{"value":"forwarded"}"#.to_string(), &extensions)
            .await;
        assert_eq!(result.model_output(), "forwarded");
    }
}
