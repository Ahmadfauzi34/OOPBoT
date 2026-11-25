use super::runner::strip_internal_tool_fields_for_llm;
use super::*;
use serde_json::json;

#[test]
fn chat_input_text_only_constructs_correctly() {
    let input = ChatInput::text_only("halo dunia");
    assert_eq!(input.text, "halo dunia");
    assert!(input.images.is_empty());
}

#[test]
fn chat_input_with_images_constructs_correctly() {
    let images = vec![("image/png".to_string(), "base64data".to_string())];
    let input = ChatInput::with_images("analisis gambar ini", images.clone());
    assert_eq!(input.text, "analisis gambar ini");
    assert_eq!(input.images.len(), 1);
    assert_eq!(input.images[0].0, "image/png");
    assert_eq!(input.images[0].1, "base64data");
}

#[test]
fn turn_outcome_aggregates_usage_and_steps() {
    let outcome = TurnOutcome {
        text: "Jawaban asisten".to_string(),
        events: vec![],
        steps: 2,
        outcome: StepsOutcome::Done,
        usage: Some(TokenUsage {
            input_tokens: 150,
            output_tokens: 80,
        }),
        last_usage: None,
        aborted: false,
    };

    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.outcome, StepsOutcome::Done);
    assert_eq!(outcome.usage.unwrap().input_tokens, 150);
    assert_eq!(outcome.usage.unwrap().output_tokens, 80);
    assert!(!outcome.aborted);
}

#[test]
fn reg_todo_strips_internal_details_from_llm_payload_only() {
    let raw = json!({
        "status": "success",
        "action": "create",
        "changed": true,
        "task": { "id": 1, "subject": "A", "status": "pending" },
        "details": { "action": "create", "tasks": [{ "id": 1 }], "nextId": 2 },
        "message": "ok"
    });
    let for_llm = strip_internal_tool_fields_for_llm("todo", raw.clone());
    assert!(for_llm.get("details").is_none());
    assert_eq!(for_llm["task"]["id"], 1);
    assert_eq!(for_llm["message"], "ok");

    let other = strip_internal_tool_fields_for_llm("read_file", raw.clone());
    assert!(other.get("details").is_some());
}

#[test]
fn reg_edit_file_strips_internal_details_from_llm_payload_only() {
    let raw = json!({
        "path": "src/main.rs",
        "edits_applied": 2,
        "lines_added": 5,
        "lines_removed": 3,
        "first_changed_line": 42,
        "bytes_written": 1840,
        "details": {
            "diff": "  41 fn main() {\n- 42     old();\n+ 42     new();\n  43 }",
            "firstChangedLine": 42
        }
    });
    let for_llm = strip_internal_tool_fields_for_llm("edit_file", raw.clone());
    assert!(for_llm.get("details").is_none());
    assert_eq!(for_llm["path"], "src/main.rs");
    assert_eq!(for_llm["edits_applied"], 2);
    assert_eq!(for_llm["lines_added"], 5);
    assert_eq!(for_llm["lines_removed"], 3);
    assert_eq!(for_llm["first_changed_line"], 42);
    assert_eq!(for_llm["bytes_written"], 1840);

    let sys = default_system_instruction(&crate::tools::ToolRegistry::with_defaults());
    assert!(sys.contains("edit_file"));
}
