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
    pub(super) upstream: Option<Arc<super::upstream::ManagedUpstream>>,
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
            upstream: None,
        }
    }

    pub(super) fn for_call(context: &CallContext) -> Self {
        Self::new(context, None)
    }

    pub(super) fn with_execution_effects(mut self, effects: Arc<ExecutionEffects>) -> Self {
        self.execution_effects = Some(effects);
        self
    }

    pub(super) fn with_upstream(mut self, managed: Arc<super::upstream::ManagedUpstream>) -> Self {
        self.execution_effects = managed.effects.clone();
        self.upstream = Some(managed);
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

    pub(super) fn start_upstream(
        &self,
        purpose: Option<gateway_plugin_sdk::call::upstream_adapter::UpstreamPathPurpose>,
    ) -> HttpAttempt {
        HttpAttempt {
            effects: if purpose
                == Some(gateway_plugin_sdk::call::upstream_adapter::UpstreamPathPurpose::Inference)
            {
                None
            } else {
                self.execution_effects.clone()
            },
            upstream: self
                .upstream
                .as_ref()
                .map(|managed| Arc::clone(&managed.send_state)),
            completed: false,
        }
    }
}

pub(super) struct HttpAttempt {
    effects: Option<Arc<ExecutionEffects>>,
    completed: bool,
    upstream: Option<Arc<super::upstream::SendWatermark>>,
}

impl HttpAttempt {
    pub(super) fn finish(mut self, observed: UpstreamSendState) {
        self.observe(observed);
        self.completed = true;
    }

    fn observe(&self, observed: UpstreamSendState) {
        if let Some(upstream) = &self.upstream {
            upstream.observe(observed);
        }
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
