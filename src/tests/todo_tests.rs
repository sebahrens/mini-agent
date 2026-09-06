use crate::agent::tools::todo::{TodoItem, TodoReadArgs, TodoWriteArgs};
use crate::agent::tools::{ReadTodoList, TodoStore, WriteTodoList};
use compact_str::CompactString;
use rig::tool::Tool;

#[tokio::test]
async fn definition_name() {
    let tool = WriteTodoList::new(None, None);
    assert_eq!(tool.name(), "todo_write");
}

#[tokio::test]
async fn read_definition_name() {
    let tool = ReadTodoList::new(TodoStore::default());
    assert_eq!(tool.name(), "todo_read");
    assert_eq!(tool.parameters()["additionalProperties"], false);
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

#[tokio::test]
async fn write_and_read_share_one_session_store() {
    let store = TodoStore::default();
    let writer = WriteTodoList::new_with_store(None, None, store.clone());
    let reader = ReadTodoList::new(store);

    writer
        .call(TodoWriteArgs {
            todos: vec![TodoItem {
                content: "Persist this plan".to_string(),
                status: CompactString::new("in_progress"),
                priority: CompactString::new("high"),
            }],
        })
        .await
        .unwrap();

    let output = reader.call(TodoReadArgs {}).await.unwrap();
    assert!(output.contains("Persist this plan"));
    assert!(output.contains("[>]"));
}

#[test]
fn todo_store_round_trips_with_session_and_survives_compaction() {
    use crate::session::{MessageRole, Session};

    let session = Session::new("provider", "model", 10_000, "todos");
    session.todos.replace(vec![
        TodoItem {
            content: "Open work".to_string(),
            status: CompactString::new("pending"),
            priority: CompactString::new("high"),
        },
        TodoItem {
            content: "Finished work".to_string(),
            status: CompactString::new("completed"),
            priority: CompactString::new("low"),
        },
    ]);
    let encoded = serde_json::to_string(&session).unwrap();
    let mut restored: Session = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored.todos.snapshot(), session.todos.snapshot());

    restored.add_message(MessageRole::User, "old context");
    restored.add_message(MessageRole::Assistant, "old response");
    restored.compress("Goal: continue".to_string(), 1, 1);
    let summary = restored.compactions.last().unwrap().summary.as_str();
    assert!(summary.contains("Critical Context"));
    assert!(summary.contains("Open work"));
    assert!(!summary.contains("Finished work"));
}
