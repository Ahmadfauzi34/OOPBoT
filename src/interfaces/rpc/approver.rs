use super::RpcServer;
use crate::agent::Approver;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub type PendingApprovals = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>;

struct PendingApprovalGuard {
    pending: PendingApprovals,
    req_id: String,
    active: bool,
}

impl Drop for PendingApprovalGuard {
    fn drop(&mut self) {
        if self.active {
            let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&self.req_id);
        }
    }
}

/// Approver berbasis RPC yang berinteraksi secara asinkron dengan UI OOPBoT Android
/// melalui protokol RPC OOPBoT `extension_ui_request` (`method: "confirm"`) dan `extension_ui_response`.
#[derive(Clone)]
pub struct RpcApprover {
    pending: PendingApprovals,
    counter: Arc<AtomicU64>,
    timeout: std::time::Duration,
    emitter: Option<Arc<dyn Fn(Value) + Send + Sync>>,
}

impl RpcApprover {
    pub fn new(pending: PendingApprovals) -> Self {
        Self {
            pending,
            counter: Arc::new(AtomicU64::new(1)),
            timeout: std::time::Duration::from_secs(300), // Default fail-closed setelah 5 menit
            emitter: None,
        }
    }

    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_emitter<F>(mut self, emitter: F) -> Self
    where
        F: Fn(Value) + Send + Sync + 'static,
    {
        self.emitter = Some(Arc::new(emitter));
        self
    }
}

#[async_trait]
impl Approver for RpcApprover {
    async fn approve(&self, tool_name: &str, preview: &str) -> bool {
        let id_num = self.counter.fetch_add(1, Ordering::SeqCst);
        let req_id = format!("appr-{}", id_num);
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(req_id.clone(), tx);
        }
        let mut _guard = PendingApprovalGuard {
            pending: Arc::clone(&self.pending),
            req_id: req_id.clone(),
            active: true,
        };

        let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
        let event = json!({
            "type": "extension_ui_request",
            "id": req_id,
            "method": "confirm",
            "title": format!("Allow {}?", tool_name),
            "message": preview,
            "timeout": timeout_ms,
        });

        if let Some(ref em) = self.emitter {
            em(event);
        } else {
            RpcServer::send_line(&event);
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => {
                _guard.active = false;
                approved
            }
            Ok(Err(_)) | Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_approval_guard_cleans_up_when_mutex_poisoned() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = tokio::sync::oneshot::channel::<bool>();
        pending.lock().unwrap().insert("appr-poison".into(), tx);

        let p_clone = Arc::clone(&pending);
        let _ = std::panic::catch_unwind(move || {
            let _guard = p_clone.lock().unwrap();
            panic!("poison test mutex");
        });
        assert!(pending.lock().is_err());

        {
            let _guard = PendingApprovalGuard {
                pending: Arc::clone(&pending),
                req_id: "appr-poison".into(),
                active: true,
            };
        }

        let map = pending.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!map.contains_key("appr-poison"));
    }
}

