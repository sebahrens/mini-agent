use crate::agent::tools::WriteTodoList;
use crate::agent::tools::todo::{TodoItem, TodoWriteArgs};
use compact_str::CompactString;
use rig::tool::Tool;

#[tokio::test]
async fn definition_name() {
    let tool = WriteTodoList::new(None, None);
    assert_eq!(tool.name(), "todo_write");
}

#[tokio::test]
async fn definition_description_non_empty() {
    let tool = WriteTodoList::new(None, None);
    assert!(!tool.description().is_empty());
}

#[tokio::test]
async fn definition_parameters_has_required_fields() {
    let tool = WriteTodoList::new(None, None);
    let binding = tool.parameters();
    let params = binding.as_object().unwrap();
    assert!(params.contains_key("properties"));
    let props = params["properties"].as_object().unwrap();
    assert!(props.contains_key("todos"));
}

#[tokio::test]
async fn call_with_empty_todos() {
    let tool = WriteTodoList::new(None, None);
    let args = TodoWriteArgs { todos: vec![] };
    let result = tool.call(args).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.contains("cleared"), "got: {}", output);
}

#[tokio::test]
async fn call_formats_todo_items_with_icons() {
    let tool = WriteTodoList::new(None, None);
    let args = TodoWriteArgs {
        todos: vec![
            TodoItem {
                content: "High priority task".to_string(),
                status: CompactString::new("high"),
                priority: CompactString::new("high"),
            },
            TodoItem {
                content: "Completed task".to_string(),
                status: CompactString::new("completed"),
                priority: CompactString::new("medium"),
            },
            TodoItem {
                content: "In progress task".to_string(),
                status: CompactString::new("in_progress"),
                priority: CompactString::new("medium"),
            },
            TodoItem {
                content: "Cancelled task".to_string(),
                status: CompactString::new("cancelled"),
                priority: CompactString::new("low"),
            },
            TodoItem {
                content: "Low priority task".to_string(),
                status: CompactString::new("low"),
                priority: CompactString::new("low"),
            },
        ],
    };
    let result = tool.call(args).await;
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.contains("[x]"));
    assert!(output.contains("[>]"));
    assert!(output.contains("[-]"));
    assert!(output.contains("[ ]"));
    assert!(output.contains("High priority task"));
    assert!(output.contains("Completed task"));
    assert!(output.contains("In progress task"));
    assert!(output.contains("Cancelled task"));
    assert!(output.contains("Low priority task"));
    assert!(output.contains("5 items"));
}

#[tokio::test]
async fn independent_todo_tools_do_not_share_process_global_state() {
    let first = WriteTodoList::new(None, None)
        .call(TodoWriteArgs {
            todos: vec![TodoItem {
                content: "First session".to_string(),
                status: CompactString::new("pending"),
                priority: CompactString::new("high"),
            }],
        })
        .await
        .unwrap();
    let second = WriteTodoList::new(None, None)
        .call(TodoWriteArgs { todos: vec![] })
        .await
        .unwrap();

    assert!(first.contains("First session"));
    assert!(second.contains("cleared"));
    assert!(!second.contains("First session"));
}
