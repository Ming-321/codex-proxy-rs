use std::sync::Arc;

use gateway_core::{
    account::OutboundProxy, engine::nested::ExecutionEffects, upstream::UpstreamSendState,
};
use gateway_plugin_sdk::{CallContext, Stage};

/// 一次回调的账号、网络与嵌套模型授权事实，由操作持有到完成。
pub(crate) struct NetworkScope {
    stage: Stage,
    account_id: Option<String>,
    credential_revision: Option<u64>,
    pub(super) proxy: Option<OutboundProxy>,
    pub(super) extension_scope: gateway_core::engine::extensions::ExtensionCallScope,
    execution_effects: Option<Arc<ExecutionEffects>>,
}

impl NetworkScope {
    pub(super) fn new(context: &CallContext, proxy: Option<OutboundProxy>) -> Self {
        Self {
            stage: context.stage,
            account_id: context.account_id.clone(),
            credential_revision: context.credential_revision,
            proxy,
            extension_scope: Default::default(),
            execution_effects: None,
        }
    }

    pub(super) fn for_call(context: &CallContext) -> Self {
        Self::new(context, None)
    }

    pub(super) fn with_execution_effects(mut self, effects: Arc<ExecutionEffects>) -> Self {
        self.execution_effects = Some(effects);
        self
    }

    pub(super) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(super) const fn credential_revision(&self) -> Option<u64> {
        self.credential_revision
    }

    pub(super) fn authorizes(&self, context: &CallContext) -> bool {
        context.stage == self.stage
            && context.account_id == self.account_id
            && context.credential_revision == self.credential_revision
    }

    pub(super) fn start_http(&self) -> HttpAttempt {
        HttpAttempt {
            effects: self.execution_effects.clone(),
            completed: false,
        }
    }
}

pub(super) struct HttpAttempt {
    effects: Option<Arc<ExecutionEffects>>,
    completed: bool,
}

impl HttpAttempt {
    pub(super) fn finish(mut self, observed: UpstreamSendState) {
        self.observe(observed);
        self.completed = true;
    }

    fn observe(&self, observed: UpstreamSendState) {
        if observed != UpstreamSendState::NotSent
            && let Some(effects) = &self.effects
        {
            effects.observe();
        }
    }
}

impl Drop for HttpAttempt {
    fn drop(&mut self) {
        // HTTP future 被取消时无法证明上游未接收；通知 Core 收紧重放判断。
        if !self.completed {
            self.observe(UpstreamSendState::Ambiguous);
        }
    }
}
