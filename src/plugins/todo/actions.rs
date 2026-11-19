//! Validasi argumen & eksekusi aksi `todo`: `create`, `update`, `batch`, `list`, `get`, `delete`, `clear`.

use super::format::{format_get_lines, format_ids, format_list_line, sanitize_text};
use super::state::{
    blocks_of, closes_cycle, dependency_problem, ensure_create_capacity, make_details,
    merge_metadata, unresolved_blockers, TaskItem, TaskStatus, TodoState, MAX_BATCH_OPS,
    MAX_DESCRIPTION_CHARS, MAX_LABEL_CHARS, MAX_SUBJECT_CHARS,
};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

pub(super) fn parse_task_id(val: &Value, field: &str) -> Result<u32, String> {
    if let Some(n) = val.as_u64() {
        let id =
            u32::try_from(n).map_err(|_| format!("nilai '{}' di luar jangkauan u32", field))?;
        if id == 0 {
            return Err(format!("ID pada '{}' harus >= 1", field));
        }
        return Ok(id);
    }
    if let Some(f) = val.as_f64() {
        if f.is_finite() && f >= 1.0 && f <= (u32::MAX as f64) && f.fract() == 0.0 {
            return Ok(f as u32);
        }
    }
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if let Ok(id) = trimmed.parse::<u32>() {
            if id == 0 {
                return Err(format!("ID pada '{}' harus >= 1", field));
            }
            return Ok(id);
        }
    }
    Err(format!("argumen '{}' (angka bulat >= 1) wajib diisi", field))
}

fn parse_id_list(args: &Value, field: &str) -> Result<Option<Vec<u32>>, String> {
    let Some(raw) = args.get(field) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let arr = raw
        .as_array()
        .ok_or_else(|| format!("argumen '{}' harus berupa array angka", field))?;
    let mut ids = Vec::with_capacity(arr.len());
    for item in arr {
        let id = parse_task_id(item, field)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(Some(ids))
}

fn parse_metadata_arg(args: &Value) -> Result<Option<Map<String, Value>>, String> {
    let Some(raw) = args.get("metadata") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let obj = raw
        .as_object()
        .ok_or_else(|| "argumen 'metadata' harus berupa objek JSON".to_string())?;
    Ok(Some(obj.clone()))
}

fn parse_optional_clearable_string(
    args: &Value,
    field: &str,
    trim_only_clears: bool,
    max_chars: usize,
) -> Result<Option<Option<String>>, String> {
    let Some(raw) = args.get(field) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(Some(None));
    }
    let s = raw
        .as_str()
        .ok_or_else(|| format!("argumen '{}' harus berupa string atau null", field))?;
    let should_clear = if trim_only_clears {
        s.trim().is_empty()
    } else {
        s.is_empty()
    };
    if should_clear {
        return Ok(Some(None));
    }
    validate_text_len(s, field, max_chars)?;
    Ok(Some(Some(s.to_string())))
}

fn validate_text_len(text: &str, field: &str, max_chars: usize) -> Result<(), String> {
    let count = text.chars().count();
    if count > max_chars {
        return Err(format!(
            "argumen '{}' terlalu panjang ({} karakter, maksimal {})",
            field, count, max_chars
        ));
    }
    Ok(())
}

fn validate_create_only_args(args: &Value) -> Result<(), String> {
    if let Some(st) = args.get("status").filter(|v| !v.is_null()) {
        let s = st
            .as_str()
            .ok_or_else(|| "argumen 'status' pada create harus berupa string".to_string())?;
        if s != "pending" {
            return Err(format!(
                "aksi 'create' selalu membuat tugas berstatus 'pending' (status '{}' ditolak; gunakan aksi 'update' setelah tugas dibuat)",
                s
            ));
        }
    }
    if args
        .get("addBlockedBy")
        .filter(|v| !v.is_null())
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
        || args
            .get("removeBlockedBy")
            .filter(|v| !v.is_null())
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    {
        return Err(
            "gunakan 'blockedBy' (bukan 'addBlockedBy'/'removeBlockedBy') pada aksi 'create'"
                .to_string(),
        );
    }
    Ok(())
}

fn create_single_task(
    state: &mut TodoState,
    args: &Value,
    batch_ids: Option<&[u32]>,
) -> Result<TaskItem, String> {
    ensure_create_capacity(state)?;
    validate_create_only_args(args)?;

    let subject = args["subject"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "argumen 'subject' wajib diisi untuk aksi create".to_string())?;
    validate_text_len(subject, "subject", MAX_SUBJECT_CHARS)?;

    let mut blocked_by = parse_id_list(args, "blockedBy")?.unwrap_or_default();
    if let Some(raw_idx) = args.get("dependsOnBatchIndex").filter(|v| !v.is_null()) {
        let Some(created_ids) = batch_ids else {
            return Err(
                "argumen 'dependsOnBatchIndex' hanya dapat dipakai di dalam daftar 'tasks' pada batch create"
                    .to_string(),
            );
        };
        let arr = raw_idx.as_array().ok_or_else(|| {
            "argumen 'dependsOnBatchIndex' harus berupa array indeks >= 0".to_string()
        })?;
        for item in arr {
            let idx_u64 = item.as_u64().ok_or_else(|| {
                "setiap elemen 'dependsOnBatchIndex' harus integer >= 0".to_string()
            })?;
            let idx = usize::try_from(idx_u64)
                .map_err(|_| "indeks 'dependsOnBatchIndex' di luar jangkauan".to_string())?;
            let dep_id = created_ids.get(idx).copied().ok_or_else(|| {
                format!(
                    "dependsOnBatchIndex {} tidak valid (hanya boleh merujuk item sebelum indeks saat ini: 0..{})",
                    idx,
                    created_ids.len()
                )
            })?;
            if !blocked_by.contains(&dep_id) {
                blocked_by.push(dep_id);
            }
        }
    }

    for &dep in &blocked_by {
        if let Some(problem) = dependency_problem(&state.tasks, dep) {
            return Err(format!("blockedBy: {}", problem));
        }
    }

    let id = state.next_id.max(1);
    let next_id = id
        .checked_add(1)
        .ok_or_else(|| "batas maksimum ID tugas (u32::MAX) tercapai".to_string())?;
    state.next_id = next_id;

    let description =
        parse_optional_clearable_string(args, "description", false, MAX_DESCRIPTION_CHARS)?
            .flatten();
    let active_form =
        parse_optional_clearable_string(args, "activeForm", false, MAX_LABEL_CHARS)?.flatten();
    let owner = parse_optional_clearable_string(args, "owner", true, MAX_LABEL_CHARS)?.flatten();
    let metadata = match parse_metadata_arg(args)? {
        Some(patch) => merge_metadata(None, &patch)?,
        None => None,
    };

    let task = TaskItem {
        id,
        subject: subject.to_string(),
        description,
        active_form,
        status: TaskStatus::Pending,
        blocked_by,
        owner,
        metadata,
    };
    state.tasks.push(task.clone());
    Ok(task)
}

pub(super) fn execute_create(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    if let Some(raw_tasks) = args.get("tasks").filter(|v| !v.is_null()) {
        return execute_create_batch(state, args, raw_tasks);
    }

    let task = create_single_task(state, args, None)?;
    let details = make_details("create", args, state);

    Ok(json!({
        "status": "success",
        "action": "create",
        "changed": true,
        "task": task,
        "details": details,
        "message": format!("Tugas #{} '{}' berhasil dibuat (pending)", task.id, sanitize_text(&task.subject))
    }))
}

fn execute_create_batch(
    state: &mut TodoState,
    args: &Value,
    raw_tasks: &Value,
) -> Result<Value, String> {
    let items = raw_tasks
        .as_array()
        .ok_or_else(|| "argumen 'tasks' harus berupa array objek tugas".to_string())?;
    if items.is_empty() {
        return Err("argumen 'tasks' tidak boleh array kosong".to_string());
    }
    if items.len() > MAX_BATCH_OPS {
        return Err(format!(
            "jumlah item pada batch create melebihi batas maksimal ({})",
            MAX_BATCH_OPS
        ));
    }

    let mut draft = state.clone();
    let mut created = Vec::with_capacity(items.len());
    let mut created_ids = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err(format!("elemen 'tasks[{}]' harus berupa objek JSON", idx));
        }
        let task = create_single_task(&mut draft, item, Some(&created_ids))
            .map_err(|e| format!("tasks[{}]: {}", idx, e))?;
        created_ids.push(task.id);
        created.push(task);
    }

    *state = draft;
    let details = make_details("create", args, state);
    let summary_lines: Vec<String> = created
        .iter()
        .map(|t| format!("#{} '{}'", t.id, sanitize_text(&t.subject)))
        .collect();

    Ok(json!({
        "status": "success",
        "action": "create",
        "changed": true,
        "tasks": created,
        "details": details,
        "message": format!("Berhasil membuat {} tugas (pending): {}", summary_lines.len(), summary_lines.join(", "))
    }))
}

pub(super) fn execute_update(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(&args["id"], "id")?;

    let idx = state
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

    let subject_arg = match args.get("subject") {
        None => None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "argumen 'subject' harus berupa string tidak kosong".to_string())?;
            if s.trim().is_empty() {
                return Err("argumen 'subject' tidak boleh kosong pada aksi update".to_string());
            }
            validate_text_len(s, "subject", MAX_SUBJECT_CHARS)?;
            Some(s)
        }
    };
    let description_arg =
        parse_optional_clearable_string(args, "description", false, MAX_DESCRIPTION_CHARS)?;
    let active_form_arg =
        parse_optional_clearable_string(args, "activeForm", false, MAX_LABEL_CHARS)?;
    let owner_arg = parse_optional_clearable_string(args, "owner", true, MAX_LABEL_CHARS)?;
    let status_arg = match args.get("status").filter(|v| !v.is_null()) {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "argumen 'status' harus berupa string".to_string())?;
            let parsed = TaskStatus::parse(s).ok_or_else(|| {
                format!(
                    "status '{}' tidak valid (gunakan pending, in_progress, completed, atau deleted)",
                    s
                )
            })?;
            Some(parsed)
        }
        None => None,
    };
    let add_blocked_by = parse_id_list(args, "addBlockedBy")?.unwrap_or_default();
    let remove_blocked_by = parse_id_list(args, "removeBlockedBy")?.unwrap_or_default();
    let metadata_patch = parse_metadata_arg(args)?;

    let has_mutable = subject_arg.is_some()
        || description_arg.is_some()
        || active_form_arg.is_some()
        || status_arg.is_some()
        || owner_arg.is_some()
        || metadata_patch.is_some()
        || !add_blocked_by.is_empty()
        || !remove_blocked_by.is_empty();
    if !has_mutable {
        return Err(
            "update memerlukan minimal satu field mutable: subject, description, activeForm, status, owner, metadata, addBlockedBy, atau removeBlockedBy"
                .to_string(),
        );
    }

    let current = state.tasks[idx].clone();
    let target_status = status_arg.unwrap_or(current.status);
    if !current.status.can_transition_to(target_status) {
        return Err(format!(
            "transisi status tidak diizinkan: {} → {}",
            current.status.as_str(),
            target_status.as_str()
        ));
    }

    let remove_set: HashSet<u32> = remove_blocked_by.into_iter().collect();
    let mut next_blocked_by: Vec<u32> = current
        .blocked_by
        .iter()
        .copied()
        .filter(|dep| !remove_set.contains(dep))
        .collect();

    for dep in &add_blocked_by {
        if *dep == current.id {
            return Err(format!(
                "tidak dapat memblokir tugas #{} oleh dirinya sendiri",
                current.id
            ));
        }
        if let Some(problem) = dependency_problem(&state.tasks, *dep) {
            return Err(format!("addBlockedBy: {}", problem));
        }
        if !next_blocked_by.contains(dep) {
            next_blocked_by.push(*dep);
        }
    }

    if !add_blocked_by.is_empty() && closes_cycle(&state.tasks, current.id, &next_blocked_by) {
        return Err(
            "addBlockedBy ditolak karena akan membentuk siklus pada graf blockedBy".to_string(),
        );
    }

    if matches!(target_status, TaskStatus::InProgress | TaskStatus::Completed)
        && current.status != target_status
    {
        let blockers = unresolved_blockers(&state.tasks, &next_blocked_by);
        if !blockers.is_empty() {
            return Err(format!(
                "tidak dapat mengubah tugas #{} ke '{}' karena masih diblokir oleh: {} (selesaikan tugas penghambat atau hapus dengan removeBlockedBy terlebih dahulu)",
                current.id,
                target_status.as_str(),
                blockers.join(", ")
            ));
        }
    }

    let mut updated = current.clone();
    updated.status = target_status;
    if let Some(s) = subject_arg {
        updated.subject = s.to_string();
    }
    if let Some(d) = description_arg {
        updated.description = d;
    }
    if let Some(af) = active_form_arg {
        updated.active_form = af;
    }
    if let Some(o) = owner_arg {
        updated.owner = o;
    }
    updated.blocked_by = next_blocked_by;
    if let Some(patch) = &metadata_patch {
        updated.metadata = merge_metadata(current.metadata.as_ref(), patch)?;
    }

    let mut warnings = Vec::new();
    if target_status == TaskStatus::InProgress && current.status != TaskStatus::InProgress {
        let other_active: Vec<String> = state
            .tasks
            .iter()
            .filter(|t| {
                t.id != id
                    && t.status == TaskStatus::InProgress
                    && (updated.owner.is_none() || t.owner.is_none() || t.owner == updated.owner)
            })
            .map(|t| format!("#{} '{}'", t.id, sanitize_text(&t.subject)))
            .collect();
        if !other_active.is_empty() {
            warnings.push(format!(
                "Masih ada tugas lain berstatus in_progress: {} — pastikan untuk menandai 'completed' bila sudah selesai.",
                other_active.join(", ")
            ));
        }
    }

    let changed = updated != current;
    if changed {
        state.tasks[idx] = updated.clone();
    }

    let mut message = if !changed {
        format!(
            "No change: tugas #{} sudah sesuai nilai yang diminta (status: {})",
            id,
            target_status.as_str()
        )
    } else if current.status == target_status {
        format!("Tugas #{} berhasil diperbarui", id)
    } else {
        format!(
            "Tugas #{} diperbarui ({} → {})",
            id,
            current.status.as_str(),
            target_status.as_str()
        )
    };
    if !warnings.is_empty() {
        message.push_str(&format!("\nPeringatan: {}", warnings.join(" ")));
    }

    let details = make_details("update", args, state);
    let mut resp = json!({
        "status": "success",
        "action": "update",
        "changed": changed,
        "task": updated,
        "details": details,
        "message": message
    });
    if !warnings.is_empty() {
        resp["warnings"] = json!(warnings);
    }
    Ok(resp)
}

pub(super) fn execute_batch(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let ops = args
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "argumen 'operations' (array objek) wajib diisi untuk aksi batch".to_string()
        })?;
    if ops.is_empty() {
        return Err("argumen 'operations' tidak boleh kosong".to_string());
    }
    if ops.len() > MAX_BATCH_OPS {
        return Err(format!(
            "jumlah operasi pada batch melebihi batas maksimal ({})",
            MAX_BATCH_OPS
        ));
    }

    let mut draft = state.clone();
    let mut results = Vec::with_capacity(ops.len());
    let mut summaries = Vec::with_capacity(ops.len());
    let mut warnings = Vec::new();
    let mut any_changed = false;

    for (idx, op) in ops.iter().enumerate() {
        let Some(action) = op.get("action").and_then(Value::as_str) else {
            return Err(format!("operations[{}]: argumen 'action' wajib diisi", idx));
        };
        let mut res = match action {
            "create" => execute_create(&mut draft, op),
            "update" => execute_update(&mut draft, op),
            "delete" => execute_delete(&mut draft, op),
            other => Err(format!(
                "aksi '{}' tidak didukung di dalam batch (gunakan create, update, atau delete)",
                other
            )),
        }
        .map_err(|e| format!("operations[{}] ({}): {}", idx, action, e))?;

        if res["changed"].as_bool() == Some(true) {
            any_changed = true;
        }
        if let Some(ws) = res.get("warnings").and_then(Value::as_array) {
            for w in ws {
                if let Some(s) = w.as_str() {
                    warnings.push(s.to_string());
                }
            }
        }
        if let Some(msg) = res["message"].as_str() {
            summaries.push(format!("{}. {}", idx + 1, msg));
        }
        if let Some(obj) = res.as_object_mut() {
            obj.remove("details");
        }
        results.push(res);
    }

    *state = draft;
    let details = make_details("batch", args, state);
    let mut out = json!({
        "status": "success",
        "action": "batch",
        "changed": any_changed,
        "count": results.len(),
        "results": results,
        "details": details,
        "message": summaries.join("\n")
    });
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

pub(super) fn execute_list(state: &TodoState, args: &Value) -> Result<Value, String> {
    let include_deleted = args["includeDeleted"].as_bool().unwrap_or(false);
    let status_filter = match args.get("status").filter(|v| !v.is_null()) {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "argumen 'status' harus berupa string".to_string())?;
            Some(
                TaskStatus::parse(s)
                    .ok_or_else(|| format!("filter status '{}' tidak valid", s))?,
            )
        }
        None => None,
    };
    let owner_filter = args
        .get("owner")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());

    let filtered: Vec<&TaskItem> = state
        .tasks
        .iter()
        .filter(|t| {
            if !include_deleted && t.status == TaskStatus::Deleted {
                return false;
            }
            if let Some(st) = status_filter {
                if t.status != st {
                    return false;
                }
            }
            if let Some(owner) = owner_filter {
                if t.owner.as_deref() != Some(owner) {
                    return false;
                }
            }
            true
        })
        .collect();

    let message = if filtered.is_empty() {
        "No tasks".to_string()
    } else {
        filtered
            .iter()
            .map(|t| format_list_line(t))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let details = make_details("list", args, state);
    Ok(json!({
        "status": "success",
        "action": "list",
        "changed": false,
        "total": filtered.len(),
        "tasks": filtered,
        "details": details,
        "message": message
    }))
}

pub(super) fn execute_get(state: &TodoState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(&args["id"], "id")?;
    let task = state
        .tasks
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

    let blocks_map = blocks_of(&state.tasks);
    let blocks = blocks_map.get(&task.id).cloned().unwrap_or_default();
    let message = format_get_lines(task, &blocks);
    let details = make_details("get", args, state);

    Ok(json!({
        "status": "success",
        "action": "get",
        "changed": false,
        "task": task,
        "blocks": blocks,
        "details": details,
        "message": message
    }))
}

pub(super) fn execute_delete(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let id = parse_task_id(&args["id"], "id")?;
    let idx = state
        .tasks
        .iter()
        .position(|t| t.id == id)
        .ok_or_else(|| format!("Tugas dengan ID #{} tidak ditemukan", id))?;

    if state.tasks[idx].status == TaskStatus::Deleted {
        return Err(format!("Tugas #{} sudah dihapus (deleted)", id));
    }

    state.tasks[idx].status = TaskStatus::Deleted;
    let deleted_task = state.tasks[idx].clone();

    // Bersihkan referensi dangling blockedBy pada task lain agar tidak memblokir selamanya.
    let mut unblocked_ids = Vec::new();
    for task in &mut state.tasks {
        if task.id == id || task.status == TaskStatus::Deleted {
            continue;
        }
        let before = task.blocked_by.len();
        task.blocked_by.retain(|&dep| dep != id);
        if task.blocked_by.len() < before {
            unblocked_ids.push(task.id);
        }
    }

    let details = make_details("delete", args, state);
    let unblock_note = if unblocked_ids.is_empty() {
        String::new()
    } else {
        format!(
            " (membuka dependensi pada: {})",
            format_ids(&unblocked_ids, ", ")
        )
    };

    Ok(json!({
        "status": "success",
        "action": "delete",
        "changed": true,
        "id": id,
        "task": deleted_task.clone(),
        "unblockedIds": unblocked_ids,
        "details": details,
        "message": format!(
            "Tugas #{} '{}' berhasil dihapus (tombstone){}",
            id,
            sanitize_text(&deleted_task.subject),
            unblock_note
        )
    }))
}

pub(super) fn execute_clear(state: &mut TodoState, args: &Value) -> Result<Value, String> {
    let count = state.tasks.len();
    let changed = count > 0;
    state.tasks.clear();
    // Pertahankan next_id tetap monoton dalam sesi ini agar ID lama di history tidak tabrakan.
    state.next_id = state.next_id.max(1);
    let details = make_details("clear", args, state);

    Ok(json!({
        "status": "success",
        "action": "clear",
        "changed": changed,
        "cleared_count": count,
        "nextId": state.next_id,
        "details": details,
        "message": format!("Seluruh daftar tugas ({} item) telah dikosongkan", count)
    }))
}
