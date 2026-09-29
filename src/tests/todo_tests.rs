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

fn item(content: String, status: &str) -> TodoItem {
    TodoItem {
        content,
        status: CompactString::new(status),
        priority: CompactString::new("medium"),
    }
}

#[tokio::test]
async fn todo_write_rejects_too_many_items_and_keeps_the_existing_list() {
    use crate::agent::tools::todo::MAX_TODO_ITEMS;

    let store = TodoStore::default();
    let writer = WriteTodoList::new_with_store(None, None, store.clone());
    writer
        .call(TodoWriteArgs {
            todos: vec![item("Keep me".to_string(), "pending")],
        })
        .await
        .unwrap();

    let todos = (0..1000)
        .map(|i| item(format!("step {i}"), "pending"))
        .collect();
    let error = writer
        .call(TodoWriteArgs { todos })
        .await
        .expect_err("1000 items must be rejected")
        .to_string();
    assert!(error.contains("1000 items"), "{error}");
    assert!(
        error.contains(&format!("{MAX_TODO_ITEMS}-item limit")),
        "{error}"
    );
    assert_eq!(
        store.snapshot(),
        vec![item("Keep me".to_string(), "pending")]
    );

    let at_limit = (0..MAX_TODO_ITEMS)
        .map(|i| item(format!("step {i}"), "pending"))
        .collect();
    writer
        .call(TodoWriteArgs { todos: at_limit })
        .await
        .expect("exactly the item limit is accepted");
    assert_eq!(store.snapshot().len(), MAX_TODO_ITEMS);
}

#[tokio::test]
async fn todo_write_rejects_oversized_content_and_labels() {
    use crate::agent::tools::todo::MAX_TODO_CONTENT_CHARS;

    let store = TodoStore::default();
    let writer = WriteTodoList::new_with_store(None, None, store.clone());
    let error = writer
        .call(TodoWriteArgs {
            todos: vec![
                item("fine".to_string(), "pending"),
                item("x".repeat(100 * 1024), "pending"),
            ],
        })
        .await
        .expect_err("a 100 KB content string must be rejected")
        .to_string();
    assert!(error.contains("item 2"), "{error}");
    assert!(
        error.contains(&format!("{MAX_TODO_CONTENT_CHARS}-character limit")),
        "{error}"
    );
    assert!(error.len() < 1024, "rejection must not echo the content");
    assert!(store.is_empty());

    let error = writer
        .call(TodoWriteArgs {
            todos: vec![item("fine".to_string(), &"s".repeat(10_000))],
        })
        .await
        .expect_err("an oversized status must be rejected")
        .to_string();
    assert!(error.contains("status"), "{error}");
    assert!(store.is_empty());

    // Multi-byte content at exactly the character limit is accepted.
    writer
        .call(TodoWriteArgs {
            todos: vec![item("é".repeat(MAX_TODO_CONTENT_CHARS), "pending")],
        })
        .await
        .expect("content at the character limit is accepted");
}

#[test]
fn critical_context_is_bounded_even_for_restored_oversized_lists() {
    use crate::agent::tools::todo::{
        MAX_CRITICAL_CONTEXT_BYTES, MAX_TODO_CONTENT_CHARS, MAX_TODO_ITEMS,
    };

    // A session restored from disk bypasses todo_write validation.
    let store = TodoStore::default();
    store.replace(
        (0..1000)
            .map(|i| item(format!("{i}:{}", "é".repeat(100 * 1024)), "pending"))
            .collect(),
    );
    let context = store.critical_context().expect("open items");
    assert!(
        context.len() <= MAX_CRITICAL_CONTEXT_BYTES,
        "critical context is {} bytes",
        context.len()
    );
    assert!(context.starts_with("Critical Context"));
    assert!(
        context.contains("more open todo items omitted"),
        "{context}"
    );
    assert!(context.contains("\"0:"));
    let longest_content = context
        .split("\"content\":\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().chars().count())
        .max()
        .unwrap();
    assert!(longest_content <= MAX_TODO_CONTENT_CHARS);

    // A small list is emitted whole, with no omission note.
    let small = TodoStore::default();
    small.replace(vec![item("Open work".to_string(), "in_progress")]);
    let context = small.critical_context().unwrap();
    assert!(context.contains("Open work"));
    assert!(!context.contains("omitted"));

    // A full ASCII list at the todo_write caps survives compaction whole.
    let full = TodoStore::default();
    full.replace(
        (0..MAX_TODO_ITEMS)
            .map(|_| item("a".repeat(MAX_TODO_CONTENT_CHARS), "pending"))
            .collect(),
    );
    let context = full.critical_context().unwrap();
    assert!(context.len() <= MAX_CRITICAL_CONTEXT_BYTES);
    assert!(!context.contains("omitted"), "{}", context.len());
    let parsed: Vec<TodoItem> = serde_json::from_str(
        context
            .split_once('\n')
            .unwrap()
            .1
            .split_once('\n')
            .unwrap()
            .1,
    )
    .expect("the item block stays valid JSON");
    assert_eq!(parsed.len(), MAX_TODO_ITEMS);
}
