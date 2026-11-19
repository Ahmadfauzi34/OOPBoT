use super::state::MAX_SUBJECT_CHARS;
use super::*;
use serde_json::json;

#[tokio::test]
async fn test_todo_tool_lifecycle() {
    let plugin = TodoPlugin::new();
    let tool = TodoTool {
        state: Arc::clone(&plugin.state),
    };

    // Create
    let res = tool
        .execute(&json!({
            "action": "create",
            "subject": "Desain Arsitektur OOP"
        }))
        .await
        .unwrap();
    assert_eq!(res["status"], "success");
    assert_eq!(res["changed"], true);
    assert_eq!(res["task"]["id"], 1);

    // Update in_progress
    let update_res = tool
        .execute(&json!({
            "action": "update",
            "id": 1,
            "status": "in_progress",
            "activeForm": "mengoding plugin.rs"
        }))
        .await
        .unwrap();
    assert_eq!(update_res["status"], "success");
    assert_eq!(update_res["changed"], true);

    // Periksa widget lines
    let lines = plugin.get_widget_lines().expect("harus ada widget");
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("Todos (0/1)"));
    assert!(lines[1].contains("◐ #1 Desain Arsitektur OOP"));
    assert!(lines[1].contains("(mengoding plugin.rs)"));

    // Mark completed
    tool.execute(&json!({
        "action": "update",
        "id": 1,
        "status": "completed"
    }))
    .await
    .unwrap();

    let lines_done = plugin.get_widget_lines().expect("harus ada widget");
    assert!(lines_done[0].contains("Todos (1/1)"));
    assert!(lines_done[1].contains("✓ #1 Desain Arsitektur OOP"));
}

#[tokio::test]
async fn test_todo_dependencies_cycles_transitions_and_metadata() {
    let plugin = TodoPlugin::new();
    let mut reg = ToolRegistry::new();
    plugin.register_tools(&mut reg);
    let tool = reg.get("todo").unwrap();

    // Create #1, #2 blockedBy #1, #3
    let t1 = tool
        .execute(&json!({ "action": "create", "subject": "Riset" }))
        .await
        .unwrap();
    assert_eq!(t1["task"]["id"], 1);

    let t2 = tool
        .execute(&json!({
            "action": "create",
            "subject": "Implementasi",
            "blockedBy": [1],
            "owner": "coder",
            "metadata": { "priority": "high", "track": 1 }
        }))
        .await
        .unwrap();
    assert_eq!(t2["task"]["blockedBy"], json!([1]));
    assert_eq!(t2["task"]["owner"], "coder");

    // Invalid blockedBy at create (#99 missing)
    let bad_dep = tool
        .execute(&json!({
            "action": "create",
            "subject": "Rusak",
            "blockedBy": [99]
        }))
        .await;
    assert!(bad_dep.is_err());

    // Self-block on update must fail
    let self_block = tool
        .execute(&json!({
            "action": "update",
            "id": 1,
            "addBlockedBy": [1]
        }))
        .await;
    assert!(self_block.is_err());

    // Cycle 1 -> 2 -> 1 must fail
    let cycle = tool
        .execute(&json!({
            "action": "update",
            "id": 1,
            "addBlockedBy": [2]
        }))
        .await;
    assert!(cycle.is_err());

    // Update metadata with null deletion + removeBlockedBy
    let upd = tool
        .execute(&json!({
            "action": "update",
            "id": 2,
            "removeBlockedBy": [1],
            "metadata": { "track": null, "stage": "dev" }
        }))
        .await
        .unwrap();
    assert_eq!(upd["changed"], true);
    assert_eq!(
        upd["task"]["metadata"],
        json!({ "priority": "high", "stage": "dev" })
    );
    assert!(upd["task"].get("blockedBy").is_none());

    // Re-sending identical update reports changed = false (no-op)
    let noop = tool
        .execute(&json!({
            "action": "update",
            "id": 2,
            "owner": "coder"
        }))
        .await
        .unwrap();
    assert_eq!(noop["changed"], false);
    assert!(noop["message"].as_str().unwrap().starts_with("No change"));

    // Update without any mutable field fails
    let empty_upd = tool.execute(&json!({ "action": "update", "id": 1 })).await;
    assert!(empty_upd.is_err());

    // Completed -> pending transition is illegal
    tool.execute(&json!({ "action": "update", "id": 1, "status": "completed" }))
        .await
        .unwrap();
    let illegal = tool
        .execute(&json!({ "action": "update", "id": 1, "status": "pending" }))
        .await;
    assert!(illegal.is_err());

    // Get includes reverse `blocks` after adding dep back
    tool.execute(&json!({ "action": "update", "id": 2, "addBlockedBy": [1] }))
        .await
        .unwrap();
    let get1 = tool
        .execute(&json!({ "action": "get", "id": 1 }))
        .await
        .unwrap();
    assert_eq!(get1["blocks"], json!([2]));
    assert!(get1["message"].as_str().unwrap().contains("blocks: #2"));

    // Delete #1 -> cannot delete twice, cannot block on deleted #1
    tool.execute(&json!({ "action": "delete", "id": 1 }))
        .await
        .unwrap();
    assert!(tool
        .execute(&json!({ "action": "delete", "id": 1 }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "create", "subject": "X", "blockedBy": [1] }))
        .await
        .is_err());
}

#[test]
fn test_replay_branch_custom_wins_over_tool_result_and_handles_empty() {
    let result_entry = json!({
        "type": "message",
        "message": {
            "role": "toolResult",
            "toolName": "todo",
            "details": {
                "tasks": [{ "id": 1, "subject": "From result", "status": "pending" }],
                "nextId": 2
            }
        }
    });
    let custom_entry = json!({
        "type": "custom",
        "customType": TODO_STATE_ENTRY,
        "data": {
            "tasks": [
                { "id": 1, "subject": "From result", "status": "completed" },
                { "id": 2, "subject": "From custom", "status": "in_progress", "activeForm": "working" }
            ],
            "nextId": 3
        }
    });

    let only_result = replay_branch_entries(std::slice::from_ref(&result_entry));
    assert_eq!(only_result.tasks.len(), 1);
    assert_eq!(only_result.tasks[0].subject, "From result");

    let both = replay_branch_entries(&[custom_entry.clone(), result_entry]);
    assert_eq!(both.tasks.len(), 2);
    assert_eq!(both.next_id, 3);
    assert_eq!(both.tasks[1].subject, "From custom");

    let plugin = TodoPlugin::new();
    plugin.on_branch_replay(&[custom_entry]);
    assert_eq!(plugin.snapshot_state().tasks.len(), 2);

    let cmd_out = plugin.command_text().expect("command_text should exist");
    assert!(cmd_out.contains("1/2 completed · 1 in progress"));
    assert!(cmd_out.contains("◐ #2 From custom (working)"));

    // Empty branch resets state cleanly
    plugin.on_branch_replay(&[]);
    assert!(plugin.snapshot_state().tasks.is_empty());
    assert!(plugin.get_widget_lines().is_none());
    assert!(plugin.command_text().is_none());
}

#[test]
fn test_sanitize_strips_ansi_and_bidi_controls() {
    let dirty = "\u{001b}[31mMerah\u{001b}[0m\nBaris\t\u{202e}bidi\u{2066}!";
    assert_eq!(sanitize_text(dirty), "Merah Baris bidi!");
}

#[tokio::test]
async fn reg_todo_blocks_in_progress_until_dep_done_and_cleans_dangling_on_delete() {
    let plugin = TodoPlugin::new();
    let tool = TodoTool {
        state: Arc::clone(&plugin.state),
    };

    tool.execute(&json!({ "action": "create", "subject": "Langkah 1" }))
        .await
        .unwrap();
    tool.execute(&json!({
        "action": "create",
        "subject": "Langkah 2",
        "blockedBy": [1]
    }))
    .await
    .unwrap();

    // Transitioning #2 to in_progress or completed while #1 is pending must fail
    let blocked_start = tool
        .execute(&json!({ "action": "update", "id": 2, "status": "in_progress" }))
        .await
        .unwrap_err();
    assert!(blocked_start.contains("#1 (pending)"));

    let blocked_done = tool
        .execute(&json!({ "action": "update", "id": 2, "status": "completed" }))
        .await
        .unwrap_err();
    assert!(blocked_done.contains("#1 (pending)"));

    // Deleting #1 cleans #1 from #2.blocked_by and reports unblockedIds
    let del = tool
        .execute(&json!({ "action": "delete", "id": 1 }))
        .await
        .unwrap();
    assert_eq!(del["unblockedIds"], json!([2]));
    assert!(del["message"].as_str().unwrap().contains("#2"));

    // Now #2 can cleanly transition to in_progress
    let start2 = tool
        .execute(&json!({ "action": "update", "id": 2, "status": "in_progress" }))
        .await
        .unwrap();
    assert_eq!(start2["task"]["status"], "in_progress");
    assert!(start2["task"].get("blockedBy").is_none());
}

#[tokio::test]
async fn reg_todo_warns_on_multiple_in_progress_and_keeps_monotonic_id_after_clear() {
    let plugin = TodoPlugin::new();
    let tool = TodoTool {
        state: Arc::clone(&plugin.state),
    };

    tool.execute(&json!({ "action": "create", "subject": "T1" }))
        .await
        .unwrap();
    tool.execute(&json!({ "action": "create", "subject": "T2" }))
        .await
        .unwrap();

    let first_start = tool
        .execute(&json!({ "action": "update", "id": 1, "status": "in_progress" }))
        .await
        .unwrap();
    assert!(first_start.get("warnings").is_none());

    // Starting #2 while #1 is still in_progress emits explicit warning
    let second_start = tool
        .execute(&json!({ "action": "update", "id": 2, "status": "in_progress" }))
        .await
        .unwrap();
    assert_eq!(second_start["warnings"].as_array().unwrap().len(), 1);
    assert!(second_start["message"]
        .as_str()
        .unwrap()
        .contains("Peringatan:"));

    // clear empties tasks but preserves monotonic next_id (3)
    let cleared = tool.execute(&json!({ "action": "clear" })).await.unwrap();
    assert_eq!(cleared["changed"], true);
    assert_eq!(cleared["nextId"], 3);

    // Second clear is a no-op (changed = false)
    let cleared_again = tool.execute(&json!({ "action": "clear" })).await.unwrap();
    assert_eq!(cleared_again["changed"], false);

    // Next created task gets #3, never #1
    let t3 = tool
        .execute(&json!({ "action": "create", "subject": "T3" }))
        .await
        .unwrap();
    assert_eq!(t3["task"]["id"], 3);
}

#[tokio::test]
async fn reg_todo_atomic_create_tasks_and_batch_operations_rollback_on_error() {
    let plugin = TodoPlugin::new();
    let tool = TodoTool {
        state: Arc::clone(&plugin.state),
    };

    // Atomic batch create with invalid second item must roll back next_id & tasks
    let failed_create = tool
        .execute(&json!({
            "action": "create",
            "tasks": [
                { "subject": "Valid" },
                { "subject": "   " }
            ]
        }))
        .await;
    assert!(failed_create.is_err());
    assert!(plugin.snapshot_state().tasks.is_empty());
    assert_eq!(plugin.snapshot_state().next_id, 1);

    // Valid batch create with dependsOnBatchIndex
    let ok_create = tool
        .execute(&json!({
            "action": "create",
            "tasks": [
                { "subject": "Setup DB" },
                { "subject": "Build API", "dependsOnBatchIndex": [0] },
                { "subject": "Run Tests", "dependsOnBatchIndex": [1] }
            ]
        }))
        .await
        .unwrap();
    assert_eq!(ok_create["tasks"].as_array().unwrap().len(), 3);
    assert_eq!(ok_create["tasks"][1]["blockedBy"], json!([1]));
    assert_eq!(ok_create["tasks"][2]["blockedBy"], json!([2]));

    // Atomic batch where op #2 fails rolls back op #1 as well
    let failed_batch = tool
        .execute(&json!({
            "action": "batch",
            "operations": [
                { "action": "update", "id": 1, "status": "in_progress" },
                { "action": "update", "id": 2, "status": "in_progress" }
            ]
        }))
        .await;
    assert!(failed_batch.is_err());
    assert_eq!(plugin.snapshot_state().tasks[0].status, TaskStatus::Pending);

    // Valid atomic batch completing #1 then starting #2
    let ok_batch = tool
        .execute(&json!({
            "action": "batch",
            "operations": [
                { "action": "update", "id": 1, "status": "completed" },
                { "action": "update", "id": 2, "status": "in_progress", "activeForm": "koding" }
            ]
        }))
        .await
        .unwrap();
    assert_eq!(ok_batch["changed"], true);
    assert_eq!(ok_batch["count"], 2);
    assert_eq!(
        plugin.snapshot_state().tasks[0].status,
        TaskStatus::Completed
    );
    assert_eq!(
        plugin.snapshot_state().tasks[1].status,
        TaskStatus::InProgress
    );
}

#[tokio::test]
async fn reg_todo_tolerant_id_parsing_clearable_fields_bounds_and_create_guards() {
    let plugin = TodoPlugin::new();
    let tool = TodoTool {
        state: Arc::clone(&plugin.state),
    };

    // create rejects non-pending status & addBlockedBy
    assert!(tool
        .execute(&json!({ "action": "create", "subject": "A", "status": "in_progress" }))
        .await
        .is_err());
    assert!(tool
        .execute(&json!({ "action": "create", "subject": "A", "addBlockedBy": [1] }))
        .await
        .is_err());

    // Overlong subject is rejected
    let huge = "x".repeat(MAX_SUBJECT_CHARS + 1);
    assert!(tool
        .execute(&json!({ "action": "create", "subject": huge }))
        .await
        .is_err());

    tool.execute(&json!({
        "action": "create",
        "subject": "Task",
        "description": "desc",
        "owner": "agent"
    }))
    .await
    .unwrap();

    // Stringified id "1" and null clearing on description/owner work
    let upd = tool
        .execute(&json!({
            "action": "update",
            "id": "1",
            "description": null,
            "owner": null
        }))
        .await
        .unwrap();
    assert_eq!(upd["changed"], true);
    assert!(upd["task"].get("description").is_none());
    assert!(upd["task"].get("owner").is_none());

    // ID 0 is rejected cleanly
    assert!(tool
        .execute(&json!({ "action": "get", "id": 0 }))
        .await
        .is_err());
}

#[tokio::test]
async fn reg_todo_replay_defensive_parsing_metadata_sanitization_and_poison_recovery() {
    // Defensive replay: keeps valid tasks, drops corrupt item, strips deleted/missing blockedBy
    let entry = json!({
        "type": "custom",
        "customType": TODO_STATE_ENTRY,
        "data": {
            "nextId": 4,
            "tasks": [
                { "id": 1, "subject": "Deleted", "status": "deleted" },
                { "id": 2, "subject": "Valid", "status": "pending", "blockedBy": [1, 2, 99] },
                { "bad_schema": true }
            ]
        }
    });
    let replayed = replay_branch_entries(&[entry]);
    assert_eq!(replayed.tasks.len(), 2);
    assert!(replayed.tasks[1].blocked_by.is_empty());

    // Poisoned Mutex recovers on replace_state, snapshot_state, and execute
    let plugin = TodoPlugin::new();
    let st_clone = Arc::clone(&plugin.state);
    let jh = std::thread::spawn(move || {
        let _g = st_clone.lock().unwrap();
        panic!("poison todo mutex");
    });
    assert!(jh.join().is_err());

    plugin.replace_state(TodoState::new());
    let tool = TodoTool {
        state: Arc::clone(&plugin.state),
    };
    let created = tool
        .execute(&json!({
            "action": "create",
            "subject": "Aman",
            "metadata": { "note": "\u{001b}[31mred\u{001b}[0m\nline" }
        }))
        .await
        .unwrap();
    assert_eq!(created["task"]["id"], 1);
    let got = tool
        .execute(&json!({ "action": "get", "id": 1 }))
        .await
        .unwrap();
    let msg = got["message"].as_str().unwrap();
    assert!(!msg.contains('\u{001b}'));
    assert!(msg.contains("red line"));
}
