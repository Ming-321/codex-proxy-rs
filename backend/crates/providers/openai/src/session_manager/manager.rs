//! 显式启用账号的实验性 State 轮换；运维网络与业务 transport 完全分离。

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use futures::{
    StreamExt,
    future::{BoxFuture, join_all, select_ok},
};
use gateway_admin::{
    model::accounts::{SessionModelRefresh, SessionRefreshObserver, SessionStateRefresh},
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{
    account::{CredentialState, OutboundProxy, ProviderAccount, ProviderAccountId},
    lifecycle::CancellationToken,
    provider_ports::{ProviderRuntimePolicyPort, ProviderSessionTicket, ProviderSessionTicketPort},
    task::{DaemonTask, WorkerTaskError},
};
use reqwest::Client;

use super::diagnostics;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::{
    credential::{CodexCredentialRepository, CodexRuntimeCredential},
    transport::{
        CodexBackendClient, CodexRequestContext,
        profile::CodexWireProfileState,
        protocol::responses::CodexResponsesRequest,
        request::{RequestAccountScope, scope_request_to_account},
    },
};

pub const SESSION_KEEPALIVE_MODELS: [&str; 2] = ["gpt-5.6-sol", "gpt-6-astra"];
const TTL_SECONDS: i64 = 3600;
const MAX_REFRESH_ATTEMPTS: u32 = 100;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_BEFORE_SECONDS: i64 = 600;
const WARMUP_INTERVAL_SECONDS: u64 = 6;

#[derive(Default)]
struct SessionCache {
    generation: u64,
    cancelled: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
struct AccountSessions {
    refresh: Mutex<()>,
    attempts: Mutex<HashMap<String, u32>>,
    cache: RwLock<SessionCache>,
    retry_after: Mutex<HashMap<String, tokio::time::Instant>>,
}

/// Provider 内自动与手动刷新共享的服务；也可用于显式装配 Provider。
pub struct SessionManager {
    repository: CodexCredentialRepository,
    policy: Arc<dyn ProviderRuntimePolicyPort>,
    tickets: Option<Arc<dyn ProviderSessionTicketPort>>,
    profile: CodexWireProfileState,
    base_url: String,
    accounts: RwLock<HashMap<ProviderAccountId, Arc<AccountSessions>>>,
    oam_client: Mutex<Option<(OutboundProxy, Client)>>,
    capacity: Semaphore,
}

impl SessionManager {
    #[must_use]
    pub fn new(
        repository: CodexCredentialRepository,
        policy: Arc<dyn ProviderRuntimePolicyPort>,
        profile: CodexWireProfileState,
        base_url: String,
        tickets: Option<Arc<dyn ProviderSessionTicketPort>>,
    ) -> Self {
        Self {
            repository,
            policy,
            tickets,
            profile,
            base_url,
            accounts: RwLock::new(HashMap::new()),
            oam_client: Mutex::new(None),
            capacity: Semaphore::new(2),
        }
    }

    /// 只失效缓存代次，保留账号级刷新互斥与上游冷却，避免配置编辑绕过它们。
    pub async fn invalidate(&self, account_id: &ProviderAccountId) {
        let sessions = self.account_sessions(account_id).await;
        let mut cache = sessions.cache.write().await;
        cache.generation = cache.generation.wrapping_add(1);
        cache.cancelled.cancel();
        cache.cancelled = tokio_util::sync::CancellationToken::new();
        tracing::info!(target: "session_keepalive", account_id = account_id.as_str(), reason = "account_unavailable", "Session tickets invalidated");
        if let Some(tickets) = &self.tickets
            && tickets.clear(account_id).await.is_err()
        {
            tracing::warn!(
                account_id = account_id.as_str(),
                "Session ticket invalidation failed"
            );
        }
    }

    async fn account_sessions(&self, account_id: &ProviderAccountId) -> Arc<AccountSessions> {
        self.accounts
            .write()
            .await
            .entry(account_id.clone())
            .or_default()
            .clone()
    }

    pub async fn refresh(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<SessionStateRefresh, ProviderAdminError> {
        self.refresh_with_progress(account_id, None).await
    }

    pub async fn refresh_with_progress(
        &self,
        account_id: &ProviderAccountId,
        observer: Option<SessionRefreshObserver>,
    ) -> Result<SessionStateRefresh, ProviderAdminError> {
        self.refresh_rounds(account_id, observer, true).await
    }

    async fn refresh_rounds(
        &self,
        account_id: &ProviderAccountId,
        observer: Option<SessionRefreshObserver>,
        repeat: bool,
    ) -> Result<SessionStateRefresh, ProviderAdminError> {
        let account = self
            .repository
            .store()
            .get_account(account_id)
            .await
            .map_err(|_| admin_error(ProviderAdminErrorKind::Unavailable, "账号读取失败"))?
            .ok_or_else(|| admin_error(ProviderAdminErrorKind::NotFound, "账号不存在"))?;
        if !eligible(&account) {
            return Err(admin_error(
                ProviderAdminErrorKind::Invalid,
                "账号未开启保活或凭据不可用",
            ));
        }
        let proxy = self
            .policy
            .load_session_keepalive_proxy()
            .await
            .map_err(|_| admin_error(ProviderAdminErrorKind::Unavailable, "动态代理配置读取失败"))?
            .ok_or_else(|| {
                admin_error(
                    ProviderAdminErrorKind::Invalid,
                    "请开启全局会话保活，并在代理管理中保存、测试动态代理",
                )
            })?;
        let sessions = self
            .accounts
            .write()
            .await
            .entry(account_id.clone())
            .or_default()
            .clone();
        let _refresh = sessions
            .refresh
            .try_lock()
            .map_err(|_| admin_error(ProviderAdminErrorKind::Conflict, "该账号正在刷新 State"))?;
        sessions
            .retry_after
            .lock()
            .await
            .retain(|_, deadline| *deadline > tokio::time::Instant::now());
        let (generation, cancelled) = {
            let cache = sessions.cache.read().await;
            (cache.generation, cache.cancelled.clone())
        };
        let _capacity = self.capacity.try_acquire().map_err(|_| {
            admin_error(ProviderAdminErrorKind::Conflict, "重写并发已满，请稍后重试")
        })?;
        sessions
            .attempts
            .lock()
            .await
            .retain(|model, _| account.session_keepalive_models().contains(model));
        if repeat {
            sessions.attempts.lock().await.clear();
            sessions.retry_after.lock().await.clear();
        }
        let client = self.client(&proxy).await?;
        let credential = self
            .repository
            .load_runtime_credential(&account)
            .await
            .map_err(|_| admin_error(ProviderAdminErrorKind::Conflict, "账号凭据已变化，请重试"))?;
        let authorization = credential
            .authentication
            .authorization_header()
            .map_err(|_| admin_error(ProviderAdminErrorKind::Invalid, "账号鉴权不可用"))?;
        let binding = credential_binding(&account, &credential)
            .map_err(|_| admin_error(ProviderAdminErrorKind::Invalid, "账号鉴权不可用"))?;
        gateway_core::account::validate_session_keepalive_models(
            account.session_keepalive_models(),
        )
        .map_err(|_| admin_error(ProviderAdminErrorKind::Invalid, "请配置有效的重写模型"))?;
        let mut pending = account.session_keepalive_models().to_vec();
        let mut models = Vec::new();
        while !pending.is_empty() {
            let policy = self
                .policy
                .load_session_rewrite_policy()
                .await
                .map_err(|_| {
                    admin_error(
                        ProviderAdminErrorKind::Unavailable,
                        "State 重写参数读取失败",
                    )
                })?;
            // 每轮只等待仍需刷新的模型；成功项立即落库并通知页面，随后退出。
            let round = join_all(pending.iter().map(|model| {
                let account = &account;
                let proxy = &proxy;
                let sessions = &sessions;
                let client = &client;
                let authorization = &authorization;
                let credential = &credential;
                let cancelled = &cancelled;
                let observer = &observer;
                async move {
                    let result = tokio::select! {
                        biased;
                        () = cancelled.cancelled() => Err("账号配置已变化，已停止重写".to_owned()),
                        result = self.refresh_model_round(account, proxy, sessions, generation, model,
                            client, authorization.expose_secret(), &credential.installation_id, &binding, policy.concurrency(), repeat) => result,
                    };
                    let item = match result {
                        Ok(None) => return None,
                        Ok(Some(expire_at)) => SessionModelRefresh {
                            model: model.clone(),
                            refreshed_at: chrono::DateTime::from_timestamp(expire_at - TTL_SECONDS, 0),
                            expire_at: Some(expire_at),
                            error: None,
                        },
                        Err(error) => SessionModelRefresh {
                            model: model.clone(), refreshed_at: None, expire_at: None, error: Some(error),
                        },
                    };
                    if observer.is_some() || item.error.is_some() {
                        tracing::info!(target: "session_keepalive", account_id = account_id.as_str(), model, expire_at = item.expire_at, error = item.error.as_deref(), "Session state cache update result");
                    }
                    if let Some(observer) = observer {
                        observer(item.clone());
                    }
                    Some(item)
                }
            })).await;
            for item in round.into_iter().flatten() {
                pending.retain(|model| model != &item.model);
                models.push(item);
            }
            if !repeat || pending.is_empty() {
                break;
            }
            // 前三轮固定等待；页面参数从第四轮起接管，429 仍按模型单独跳过。
            let warmup = sessions
                .attempts
                .lock()
                .await
                .iter()
                .any(|(model, attempt)| pending.contains(model) && *attempt <= 3);
            let seconds = if warmup {
                WARMUP_INTERVAL_SECONDS
            } else {
                u64::from(policy.retry_interval_seconds())
            };
            tokio::select! {
                () = cancelled.cancelled() => {},
                () = tokio::time::sleep(Duration::from_secs(seconds)) => {},
            }
        }
        models.sort_by_key(|item| {
            account
                .session_keepalive_models()
                .iter()
                .position(|model| model == &item.model)
        });
        Ok(SessionStateRefresh {
            account_id: account_id.as_str().to_owned(),
            models,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn refresh_model_round(
        &self,
        account: &ProviderAccount,
        proxy: &OutboundProxy,
        sessions: &AccountSessions,
        generation: u64,
        model: &str,
        client: &Client,
        authorization: &str,
        installation_id: &str,
        binding: &[u8; 32],
        configured_concurrency: u32,
        force: bool,
    ) -> Result<Option<i64>, String> {
        self.validate_refresh_context(account, proxy, sessions, generation, model, binding)
            .await
            .map_err(str::to_owned)?;
        match self.load_ticket(account, model).await {
            Ok(Some(ticket))
                if !force && ticket.expires_at - Utc::now().timestamp() >= REFRESH_BEFORE_SECONDS =>
            {
                return Ok(Some(ticket.expires_at));
            }
            Err(reason)
                if matches!(
                    reason.as_str(),
                    "cache_not_configured" | "cache_read_failed" | "credential_lookup_failed"
                ) =>
            {
                return Err(format!("State 缓存或鉴权读取失败：{reason}"));
            }
            Err(reason) => {
                tracing::info!(target: "session_keepalive", account_id = account.id().as_str(), model, reason, "Session ticket requires refresh");
            }
            _ => {}
        }
        if !force
            && sessions
                .retry_after
                .lock()
                .await
                .get(model)
                .is_some_and(|deadline| *deadline > tokio::time::Instant::now())
        {
            return Ok(None);
        }
        let attempt = {
            let mut attempts = sessions.attempts.lock().await;
            let attempt = attempts.entry(model.to_owned()).or_default();
            *attempt = attempt.saturating_add(1);
            *attempt
        };
        if attempt > MAX_REFRESH_ATTEMPTS {
            sessions.attempts.lock().await.remove(model);
            tracing::warn!(
                account_id = account.id().as_str(),
                model,
                attempt,
                max_attempts = MAX_REFRESH_ATTEMPTS,
                "Session keepalive exceeded max attempts; pausing probes until next cycle"
            );
            return Err(format!("重试达上限 {} 次，暂停重写以避免空转", MAX_REFRESH_ATTEMPTS));
        }
        let concurrency = if attempt <= 3 {
            1
        } else {
            configured_concurrency
        };
        let probes = (0..concurrency).map(|_| {
            Box::pin(self.heartbeat(
                client,
                (account, proxy, attempt),
                authorization,
                installation_id,
                model,
                &sessions.retry_after,
            ))
        });
        match select_ok(probes).await {
            Ok((state, remaining)) => {
                drop(remaining);
                let expiry = self
                    .store_refreshed_state(
                        account, proxy, sessions, generation, model, binding, state,
                    )
                    .await
                    .map_err(str::to_owned)?;
                sessions.attempts.lock().await.remove(model);
                tracing::info!(target: "session_keepalive", account_id = account.id().as_str(), model, expire_at = expiry, "Session ticket stored");
                Ok(Some(expiry))
            }
            Err(error) => {
                tracing::warn!(
                    account_id = account.id().as_str(),
                    model,
                    attempt,
                    concurrency,
                    error,
                    "Session heartbeat round failed"
                );
                Ok(None)
            }
        }
    }

    async fn client(&self, proxy: &OutboundProxy) -> Result<Client, ProviderAdminError> {
        let mut cached = self.oam_client.lock().await;
        if let Some((current, client)) = cached.as_ref()
            && current == proxy
        {
            return Ok(client.clone());
        }
        // 必须单独构造连接池；空代理在调用前已拒绝，绝不退回业务出口或环境代理。
        let client = Client::builder()
            .no_proxy()
            .proxy(
                reqwest::Proxy::all(proxy.expose_url())
                    .map_err(|_| admin_error(ProviderAdminErrorKind::Invalid, "运维代理无效"))?,
            )
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(HEARTBEAT_TIMEOUT)
            .http1_only()
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| {
                admin_error(ProviderAdminErrorKind::Unavailable, "运维客户端初始化失败")
            })?;
        *cached = Some((proxy.clone(), client.clone()));
        Ok(client)
    }

    async fn heartbeat(
        &self,
        client: &Client,
        target: (&ProviderAccount, &OutboundProxy, u32),
        authorization: &str,
        installation_id: &str,
        model: &str,
        retry_after: &Mutex<HashMap<String, tokio::time::Instant>>,
    ) -> Result<String, String> {
        let (account, proxy, attempt) = target;
        let probe_id = uuid::Uuid::new_v4().to_string();
        let proxy_url = reqwest::Url::parse(proxy.expose_url()).map_err(|_| "运维代理无效")?;
        let log = diagnostics::ProbeLog {
            account_id: account.id().as_str(),
            model,
            probe_id: &probe_id,
            attempt,
            secrets: vec![
                authorization,
                authorization
                    .strip_prefix("Bearer ")
                    .unwrap_or(authorization),
                proxy.expose_url(),
                proxy_url.password().unwrap_or(""),
            ],
        };
        let started = std::time::Instant::now();
        let operation = crate::transport::request::build_connection_test_operation(
            &gateway_core::routing::UpstreamModelId::new(model).map_err(|_| "模型 ID 无效")?,
            "hi",
        )
        .map_err(|_| "账户测试请求构造失败")?;
        let gateway_core::operation::Operation::Generate(generate) = operation else {
            return Err("账户测试请求类型无效".to_owned());
        };
        let mut request = crate::encode_generate_request(&generate, model, None)
            .map_err(|_| "账户测试请求编码失败")?;
        scope_request_to_account(&mut request, installation_id, RequestAccountScope::Unknown);
        let backend = CodexBackendClient::new(client.clone(), &self.base_url, self.profile.clone());
        let context = CodexRequestContext {
            trace: None,
            authorization,
            account_id: account.upstream_account_id(),
            request_id: &probe_id,
            turn_state: None,
            turn_metadata: None,
            beta_features: None,
            include_timing_metrics: None,
            version: None,
            codex_window_id: None,
            parent_thread_id: None,
            cookie_header: None,
            installation_id: Some(installation_id),
            session_id: None,
            thread_id: None,
            client_request_id: None,
            turn_id: None,
            account_selection: Default::default(),
        };
        let headers = backend
            .request_headers_for_http_response(&request, context)
            .map_err(|_| "重写请求头无效")?;
        let body = serde_json::to_vec(request.body()).map_err(|_| "重写请求编码失败")?;
        let mut outgoing = backend
            .build_http_sse_request(headers, body)
            .map_err(|_| "重写请求编码失败")?;
        outgoing.headers_mut().insert(
            reqwest::header::CONNECTION,
            reqwest::header::HeaderValue::from_static("close"),
        );
        log.record("request", json!({"method":outgoing.method().as_str(), "path":outgoing.url().path(), "proxyEndpoint":proxy.endpoint(), "headers":diagnostics::headers(outgoing.headers()), "body":request.body()}));
        let response = client.execute(outgoing).await.map_err(|error| {
            log.record("transport_error", json!({"timeout":error.is_timeout(), "connect":error.is_connect(), "error":error.without_url().to_string(), "elapsedMs":started.elapsed().as_millis()}));
            format!("运维网络请求失败或超时（重写 {probe_id}）")
        })?;
        let status = response.status();
        if status.as_u16() == 429 {
            let delay =
                crate::transport::retry_after_seconds(response.headers(), None).unwrap_or(60);
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(delay.min(u64::from(u32::MAX)));
            let mut current = retry_after.lock().await;
            current
                .entry(model.to_owned())
                .and_modify(|previous| *previous = (*previous).max(deadline))
                .or_insert(deadline);
        }
        // 先断言 Header 原始字节长度，不复制、不 trim、不等待响应正文。
        if let Some(state) = response.headers().get("x-codex-turn-state")
            && !is_valid_state_length_for_account(account.session_keepalive_expected_length(), state.as_bytes().len())
        {
            log.record("invalid_state", json!({"status":status.as_u16(), "stateLength":state.as_bytes().len(), "elapsedMs":started.elapsed().as_millis()}));
            return Err(format!("上游 State 长度无效（重写 {probe_id}）"));
        }
        log.record("response_headers", json!({"status":status.as_u16(), "headers":diagnostics::headers(response.headers()), "elapsedMs":started.elapsed().as_millis()}));
        // 准入只取 HTTP 与 Header；包括合法 Header 的响应也不消费 SSE。
        if status != reqwest::StatusCode::OK {
            return Err(format!(
                "HTTP {}：上游拒绝重写请求（重写 {probe_id}）",
                status.as_u16()
            ));
        }
        let state = response
            .headers()
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok())
            .filter(|value| valid_state_for_account(account.session_keepalive_expected_length(), value))
            .ok_or_else(|| format!("上游未返回有效 State（重写 {probe_id}）"))?
            .to_owned();
        drop(response);
        Ok(state)
    }

    async fn validate_refresh_context(
        &self,
        account: &ProviderAccount,
        proxy: &OutboundProxy,
        sessions: &AccountSessions,
        generation: u64,
        model: &str,
        binding: &[u8; 32],
    ) -> Result<(), &'static str> {
        let current = self
            .repository
            .store()
            .load_current_credential(account.id())
            .await
            .map_err(|_| "账号状态校验失败")?;
        let credential = self
            .repository
            .decode_runtime_credential(&current)
            .map_err(|_| "账号鉴权校验失败")?;
        if !eligible(&current.account)
            || !current.account.model_access().allows(model)
            || !current
                .account
                .session_keepalive_models()
                .iter()
                .any(|selected| selected == model)
            || credential_binding(&current.account, &credential)? != *binding
        {
            return Err("账号状态已变化，已丢弃重写结果");
        }
        let current_proxy = self
            .policy
            .load_session_keepalive_proxy()
            .await
            .map_err(|_| "运维配置校验失败")?;
        if current_proxy.as_ref() != Some(proxy) {
            return Err("运维代理已变化，已丢弃重写结果");
        }
        if sessions.cache.read().await.generation != generation {
            return Err("账号配置已变化，已停止探活");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_refreshed_state(
        &self,
        account: &ProviderAccount,
        proxy: &OutboundProxy,
        sessions: &AccountSessions,
        generation: u64,
        model: &str,
        binding: &[u8; 32],
        state: String,
    ) -> Result<i64, &'static str> {
        self.validate_refresh_context(account, proxy, sessions, generation, model, binding)
            .await?;
        let cache = sessions.cache.write().await;
        if cache.generation != generation {
            return Err("账号配置已变化，已丢弃重写结果");
        }
        let expire_at = Utc::now().timestamp() + TTL_SECONDS;
        let tickets = self.tickets.as_ref().ok_or("State 缓存未配置")?;
        tickets
            .store(
                account.id(),
                model,
                &ProviderSessionTicket {
                    value: state,
                    credential_revision: account.revision().get(),
                    credential_binding: Some(*binding),
                    expires_at: expire_at,
                },
            )
            .await
            .map_err(|_| "State 缓存写入失败")?;
        Ok(expire_at)
    }

    async fn load_ticket(
        &self,
        account: &ProviderAccount,
        model: &str,
    ) -> Result<Option<ProviderSessionTicket>, String> {
        let tickets = self.tickets.as_ref().ok_or("cache_not_configured")?;
        let ticket = tickets
            .load(account.id(), model)
            .await
            .map_err(|_| "cache_read_failed")?;
        let Some(ticket) = ticket else {
            return Ok(None);
        };
        let now = Utc::now().timestamp();
        if ticket.expires_at <= now {
            return Err("ticket_expired".to_owned());
        }
        if ticket.expires_at > now + TTL_SECONDS || !valid_state_for_account(account.session_keepalive_expected_length(), &ticket.value) {
            return Err("invalid_ticket".to_owned());
        }
        if ticket.credential_revision != account.revision().get() {
            let Some(binding) = ticket.credential_binding else {
                return Err("legacy_credential_revision_changed".to_owned());
            };
            // Cookie 与鉴权共用 CAS 版本；版本变化时只比较实际鉴权材料。
            let loaded = self
                .repository
                .store()
                .load_current_credential(account.id())
                .await
                .map_err(|_| "credential_lookup_failed")?;
            let current = self
                .repository
                .decode_runtime_credential(&loaded)
                .map_err(|_| "credential_lookup_failed")?;
            if !eligible(&loaded.account)
                || !loaded.account.model_access().allows(model)
                || !managed(&loaded.account, model)
                || credential_binding(&loaded.account, &current)? != binding
            {
                return Err("credential_changed".to_owned());
            }
        }
        Ok(Some(ticket))
    }

    /// 选号与发送前共用缺票关闭规则，Redis 不可用也不能裸发。
    pub async fn available(&self, account: &ProviderAccount, model: &str) -> bool {
        if !managed(account, model) {
            return true;
        }
        match self.load_ticket(account, model).await {
            Ok(Some(_)) => true,
            result => {
                let reason = result.err().unwrap_or_else(|| "ticket_missing".to_owned());
                tracing::info!(target: "session_keepalive", account_id = account.id().as_str(), model, reason, "Session ticket unavailable; account/model blocked");
                false
            }
        }
    }

    /// 仅改写已选号请求的 State；不接收或替换业务 Client。
    pub async fn rewrite(
        &self,
        account: &ProviderAccount,
        request: &mut CodexResponsesRequest,
    ) -> bool {
        if !managed(account, request.model()) {
            return true;
        }
        let Ok(Some(ticket)) = self.load_ticket(account, request.model()).await else {
            return false;
        };
        let state = ticket.value;
        // HTTP 头和复用 WS 连接的逐帧 metadata 使用同一值。
        request.passthrough_headers.remove("x-codex-turn-state");
        request.turn_state = Some(state.clone());
        let mut metadata = request
            .client_metadata()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert("x-codex-turn-state".to_owned(), Value::String(state));
        request.set_client_metadata(Some(Value::Object(metadata)));
        true
    }

    async fn refresh_cycle(&self) -> bool {
        // 总开关关闭时连账号列表也不遍历；手动刷新仍通过同一持久策略检查。
        if !matches!(
            self.policy.load_session_keepalive_proxy().await,
            Ok(Some(_))
        ) {
            return false;
        }
        let Ok(accounts) = self.repository.list_for_provider().await else {
            tracing::warn!("Session keepalive account list unavailable");
            return false;
        };
        self.accounts.write().await.retain(|id, sessions| {
            accounts
                .iter()
                .any(|account| account.id() == id && eligible(account))
                // 仍被请求持有或处于冷却期的账号不能换锁；重新启用也须遵守原有边界。
                || Arc::strong_count(sessions) > 1
                || sessions.retry_after.try_lock().map_or(true, |retry_after| {
                    retry_after.values().any(|deadline| *deadline > tokio::time::Instant::now())
                })
        });
        // 每个账号每轮只探测一次，避免坏账号无限重试饿死后续账号。
        let warmups = futures::stream::iter(accounts.into_iter().filter(eligible))
            .map(|account| async move {
                if let Err(error) = self.refresh_rounds(account.id(), None, false).await {
                    tracing::warn!(kind = ?error.kind(), "Session keepalive refresh unavailable");
                }
                let sessions = self.account_sessions(account.id()).await;
                sessions
                    .attempts
                    .lock()
                    .await
                    .values()
                    .any(|attempt| *attempt <= 3)
            })
            .buffer_unordered(2)
            .collect::<Vec<_>>()
            .await;
        warmups.into_iter().any(|warmup| warmup)
    }
}

impl DaemonTask for SessionManager {
    fn run(&self, cancellation: CancellationToken) -> BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            loop {
                let warmup = tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    warmup = self.refresh_cycle() => warmup,
                };
                let configured = self
                    .policy
                    .load_session_rewrite_policy()
                    .await
                    .map_or(6, |policy| policy.retry_interval_seconds());
                let seconds = if warmup {
                    WARMUP_INTERVAL_SECONDS
                } else {
                    u64::from(configured)
                };
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(Duration::from_secs(seconds)) => {}
                }
            }
        })
    }
}

fn eligible(account: &ProviderAccount) -> bool {
    account.provider().as_str() == "openai"
        && account.authentication_kind() == "oauth"
        && account.enabled()
        && account.enable_session_keepalive()
        && account.credential_state() == CredentialState::Ready
        && account
            .access_token_expires_at()
            .is_none_or(|expires| expires > std::time::SystemTime::now())
}

fn admin_error(kind: ProviderAdminErrorKind, message: &'static str) -> ProviderAdminError {
    ProviderAdminError::new(kind).with_public_message(message)
}

fn is_valid_state_length_for_account(expected: Option<u32>, len: usize) -> bool {
    match expected {
        Some(exp) if exp > 0 => len == exp as usize,
        _ => (200..=600).contains(&len),
    }
}

fn valid_state_for_account(expected: Option<u32>, state: &str) -> bool {
    is_valid_state_length_for_account(expected, state.len())
        && state.is_ascii()
        && state.starts_with("gAAAAA")
}

fn managed(account: &ProviderAccount, model: &str) -> bool {
    account.authentication_kind() == "oauth"
        && account.enable_session_keepalive()
        && account
            .session_keepalive_models()
            .iter()
            .any(|selected| selected == model)
}

// 绑定实际鉴权与安装身份，不绑定 Cookie、名称、额度等可独立变化的事实。
fn credential_binding(
    account: &ProviderAccount,
    credential: &CodexRuntimeCredential,
) -> Result<[u8; 32], &'static str> {
    let secret = credential.authentication.oauth().ok_or("账号鉴权不可用")?;
    let mut digest = Sha256::new();
    digest.update(b"codex-session-ticket-v1");
    for value in [
        account.id().as_str(),
        account.upstream_user_id().unwrap_or_default(),
        account.upstream_account_id().unwrap_or_default(),
        secret.access_token.expose_secret(),
        credential.installation_id.as_str(),
        credential
            .principal
            .as_ref()
            .map_or("", |p| p.oauth_subject.as_str()),
        credential
            .principal
            .as_ref()
            .and_then(|p| p.poid.as_deref())
            .unwrap_or_default(),
    ] {
        digest.update(
            u64::try_from(value.len())
                .map_err(|_| "鉴权材料过长")?
                .to_be_bytes(),
        );
        digest.update(value.as_bytes());
    }
    Ok(digest.finalize().into())
}
