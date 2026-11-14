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
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&self.req_id);
            }
        }
    }
}

/// Approver berbasis RPC yang berinteraksi secara asinkron dengan UI Pipkin Android
/// melalui protokol standar Pi `extension_ui_request` (`method: "confirm"`) dan `extension_ui_response`.
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
            let mut pending = self.pending.lock().unwrap();
            pending.insert(req_id.clone(), tx);
        }
        let mut _guard = PendingApprovalGuard {
            pending: Arc::clone(&self.pending),
            req_id: req_id.clone(),
            active: true,
        };

        let timeout_ms = self.timeout.as_millis() as u64;
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

