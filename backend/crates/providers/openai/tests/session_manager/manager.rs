use super::*;
use futures::FutureExt;

#[tokio::test]
async fn refresh_and_rewrite_isolate_every_account_and_model_using_only_oam_proxy() {
    let oam = MockServer::start().await;
    let business = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&oam.uri())).await;
    for account in ["acct_a", "acct_b"] {
        store.set_egress(
            account,
            Some(OutboundProxy::parse(&business.uri()).unwrap()),
            None,
        );
        for model in SESSION_KEEPALIVE_MODELS {
            mock_model(&oam, account, model, success(&format!("{account}:{model}"))).await;
        }
        let result = manager
            .refresh(&ProviderAccountId::new(account).unwrap())
            .await
            .unwrap();
        assert!(
            result.models.iter().all(|item| item.error.is_none()
                && item.expire_at.unwrap() > Utc::now().timestamp() + 3590)
        );
    }
    for account in ["acct_a", "acct_b"] {
        for model in SESSION_KEEPALIVE_MODELS {
            let mut request = request(model);
            manager
                .rewrite(&store.account(account).unwrap(), &mut request)
                .await;
            assert_eq!(
                request.turn_state.as_deref(),
                Some(state(&format!("{account}:{model}")).as_str())
            );
            assert_eq!(
                request.client_metadata().unwrap()["x-codex-turn-state"],
                state(&format!("{account}:{model}"))
            );
            assert_eq!(request.client_metadata().unwrap()["preserved"], "value");
        }
    }
    assert_eq!(oam.received_requests().await.unwrap().len(), 4);
    assert!(business.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn disabled_accounts_other_models_and_missing_proxy_never_refresh_or_override() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    store.set_session_keepalive("acct_a", false);
    let id = ProviderAccountId::new("acct_a").unwrap();
    assert_eq!(
        manager.refresh(&id).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Invalid
    );
    let mut req = request("gpt-6-astra");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut req)
        .await;
    assert_eq!(req.turn_state.as_deref(), Some("client-state"));
    *policy.proxy.lock().unwrap() = None;
    assert!(
        manager
            .refresh(&ProviderAccountId::new("acct_b").unwrap())
            .await
            .is_err()
    );
    for model in ["5.6 sol", "6", "gpt-6", "gpt-6-astra ", "GPT-5.6-sol"] {
        let mut req = request(model);
        manager
            .rewrite(&store.account("acct_b").unwrap(), &mut req)
            .await;
        assert_eq!(req.turn_state.as_deref(), Some("client-state"));
    }
    assert!(proxy.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn partial_failure_keeps_previous_state_without_extending_its_expiry() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("old-state")).await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    manager.refresh(&id).await.unwrap();
    policy.tickets.near_expiry();
    proxy.reset().await;
    mock_model(&proxy, "acct_a", "gpt-5.6-sol", ResponseTemplate::new(500)).await;
    mock_model(&proxy, "acct_a", "gpt-6-astra", success("new-state")).await;
    let pending = spawn_refresh(&manager, &id);
    wait_for_state(&manager, &store, "gpt-6-astra", "new-state").await;
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let mut sol = request("gpt-5.6-sol");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut sol)
        .await;
    assert_eq!(sol.turn_state.as_deref(), Some(state("old-state").as_str()));
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3600)).await;
    let mut expired = request("gpt-5.6-sol");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut expired)
        .await;
    assert_eq!(expired.turn_state.as_deref(), Some("client-state"));
}

#[tokio::test]
async fn duplicate_refresh_is_rejected_and_invalidated_inflight_results_cannot_repopulate() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(
            &proxy,
            "acct_a",
            model,
            success("stale-state").set_delay(Duration::from_millis(250)),
        )
        .await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = {
        let manager = Arc::clone(&manager);
        let id = id.clone();
        tokio::spawn(async move { manager.refresh(&id).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while proxy.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        manager.refresh(&id).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Conflict
    );
    manager.invalidate(&id).await;
    assert_eq!(
        manager.refresh(&id).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Conflict
    );
    // 没有合格账号时首轮只执行清理；禁用再启用也不能创建另一把刷新锁。
    store.set_session_keepalive("acct_a", false);
    store.set_session_keepalive("acct_b", false);
    assert!(
        manager
            .run(CancellationToken::new())
            .now_or_never()
            .is_none()
    );
    store.set_session_keepalive("acct_a", true);
    assert_eq!(
        manager.refresh(&id).await.unwrap_err().kind(),
        ProviderAdminErrorKind::Conflict
    );
    let result = pending.await.unwrap().unwrap();
    assert!(result.models.iter().all(|item| item.error.is_some()));
    let mut req = request("gpt-6-astra");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut req)
        .await;
    assert_eq!(req.turn_state.as_deref(), Some("client-state"));

    proxy.reset().await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("fresh-state")).await;
    }
    let result = manager.refresh(&id).await.unwrap();
    assert!(result.models.iter().all(|item| item.error.is_none()));
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut req)
        .await;
    assert_eq!(
        req.turn_state.as_deref(),
        Some(state("fresh-state").as_str())
    );
    manager.invalidate(&id).await;
    let mut req = request("gpt-6-astra");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut req)
        .await;
    assert_eq!(req.turn_state.as_deref(), Some("client-state"));
}

#[tokio::test]
async fn rate_limit_is_per_model_and_does_not_block_the_other_model() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    mock_model(
        &proxy,
        "acct_a",
        "gpt-5.6-sol",
        ResponseTemplate::new(429).insert_header("retry-after", "60"),
    )
    .await;
    mock_model(&proxy, "acct_a", "gpt-6-astra", success("healthy")).await;
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = spawn_refresh(&manager, &id);
    wait_for_state(&manager, &store, "gpt-6-astra", "healthy").await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(proxy.received_requests().await.unwrap().len(), 2);
    assert!(!pending.is_finished());
    manager.invalidate(&id).await;
    let result = pending.await.unwrap().unwrap();
    assert!(result.models[0].error.is_some());
    assert!(result.models[1].error.is_none());

    // 禁用、清理、再启用后，限流模型仍须等待原冷却；其他模型可以重新刷新。
    store.set_session_keepalive("acct_a", false);
    store.set_session_keepalive("acct_b", false);
    assert!(
        manager
            .run(CancellationToken::new())
            .now_or_never()
            .is_none()
    );
    store.set_session_keepalive("acct_a", true);
    let pending = spawn_refresh(&manager, &id);
    wait_for_state(&manager, &store, "gpt-6-astra", "healthy").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(proxy.received_requests().await.unwrap().len(), 3);
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());

    // 手动取消也不应重置冷却。
    let pending = spawn_refresh(&manager, &id);
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(proxy.received_requests().await.unwrap().len(), 3);
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    proxy.reset().await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("after-cooldown")).await;
    }
    let result = manager.refresh(&id).await.unwrap();
    assert!(result.models.iter().all(|model| model.error.is_none()));
    assert_eq!(proxy.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn missing_header_is_rejected_but_valid_header_does_not_wait_for_sse_completion() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    mock_model(&proxy, "acct_a", "gpt-5.6-sol", ResponseTemplate::new(200)).await;
    mock_model(
        &proxy,
        "acct_a",
        "gpt-6-astra",
        ResponseTemplate::new(200)
            .insert_header("x-codex-turn-state", state("unusable"))
            .set_body_raw(
                "data: {\"type\":\"response.failed\"}\n\n",
                "text/event-stream",
            ),
    )
    .await;
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = spawn_refresh(&manager, &id);
    wait_for_requests(&proxy, 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let mut req = request("gpt-6-astra");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut req)
        .await;
    assert_eq!(req.turn_state.as_deref(), Some(state("unusable").as_str()));
}

#[tokio::test]
async fn background_worker_stops_promptly_when_cancelled() {
    let (_, _, manager) = fixture(None).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), manager.run(cancellation))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn rewritten_business_http_uses_original_proxy_and_never_oam_pool() {
    use futures::StreamExt;
    use provider_openai::transport::{CodexBackendClient, CodexRequestContext};
    let oam = MockServer::start().await;
    let business = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&oam.uri())).await;
    store.set_egress(
        "acct_a",
        Some(OutboundProxy::parse(&business.uri()).unwrap()),
        None,
    );
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&oam, "acct_a", model, success("rotated-state")).await;
    }
    manager
        .refresh(&ProviderAccountId::new("acct_a").unwrap())
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(header("x-codex-turn-state", state("rotated-state")))
        .respond_with(success("business-state"))
        .expect(1)
        .mount(&business)
        .await;
    let account = store.account("acct_a").unwrap();
    let client = CodexBackendClient::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        "http://upstream.invalid/backend-api",
        wire_profile(),
    )
    .for_account(&account)
    .unwrap();
    let mut req = request("gpt-6-astra");
    req.force_http_sse = true;
    manager.rewrite(&account, &mut req).await;
    let context = CodexRequestContext {
        trace: None,
        authorization: "Bearer acct_a",
        account_id: Some("acct_a"),
        request_id: "business-request",
        turn_state: req.turn_state.as_deref(),
        turn_metadata: None,
        beta_features: None,
        include_timing_metrics: None,
        version: None,
        codex_window_id: None,
        parent_thread_id: None,
        cookie_header: None,
        installation_id: None,
        session_id: None,
        thread_id: None,
        client_request_id: None,
        turn_id: None,
        account_selection: Default::default(),
    };
    let mut response = client
        .create_response_stream_with_pool_account(&req, context, None)
        .await
        .unwrap();
    while let Some(chunk) = response.body.next().await {
        chunk.unwrap();
    }
    assert_eq!(business.received_requests().await.unwrap().len(), 1);
    assert_eq!(oam.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn actual_credential_rotation_prevents_reusing_previous_state() {
    use provider_openai::credential::CodexCredentialData;
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("old-credential-state")).await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    manager.refresh(&id).await.unwrap();
    let current = store.account("acct_a").unwrap();
    assert!(manager.available(&current, "gpt-6-astra").await);
    let repository = store.repository();
    let mut data = repository.load_complete_data(&current).await.unwrap();
    let CodexCredentialData::OAuth(ref mut oauth) = data else {
        panic!("OAuth fixture");
    };
    oauth.access_token = "rotated-token".to_owned();
    repository
        .compare_and_swap_data(&current, data)
        .await
        .unwrap();
    let rotated = store.account("acct_a").unwrap();
    assert!(!manager.available(&rotated, "gpt-6-astra").await);
    let mut req = request("gpt-6-astra");
    assert!(!manager.rewrite(&rotated, &mut req).await);
    assert_eq!(req.turn_state.as_deref(), Some("client-state"));
}

#[tokio::test]
async fn cookie_capture_preserves_tickets_expiry_and_success_pruning_after_restart() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("retained")).await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    let before = manager.refresh(&id).await.unwrap();
    let previous = store.account("acct_a").unwrap();
    capture_cookie(&store).await;
    let current = store.account("acct_a").unwrap();
    assert_ne!(previous.revision(), current.revision());
    drop(manager);
    let manager = SessionManager::new(
        store.repository(),
        policy.clone(),
        wire_profile(),
        "http://upstream.invalid/backend-api".to_owned(),
        Some(policy.tickets.clone()),
    );
    for model in SESSION_KEEPALIVE_MODELS {
        assert!(manager.available(&current, model).await);
        let mut req = request(model);
        assert!(manager.rewrite(&current, &mut req).await);
        assert_eq!(req.turn_state.as_deref(), Some(state("retained").as_str()));
    }
    let after = manager.refresh(&id).await.unwrap();
    for (old, new) in before.models.iter().zip(&after.models) {
        assert_eq!(old.expire_at, new.expire_at);
        assert!(new.error.is_none());
    }
    assert_eq!(proxy.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn cookie_capture_during_probe_does_not_discard_success() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(
            &proxy,
            "acct_a",
            model,
            success("inflight").set_delay(Duration::from_millis(250)),
        )
        .await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.refresh(&id).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while proxy.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    capture_cookie(&store).await;
    let result = pending.await.unwrap().unwrap();
    assert!(result.models.iter().all(|item| item.error.is_none()));
    let account = store.account("acct_a").unwrap();
    for model in SESSION_KEEPALIVE_MODELS {
        assert!(manager.available(&account, model).await);
    }
}

#[tokio::test(start_paused = true)]
async fn worker_uses_configured_scan_interval_and_cancels_sleep() {
    let (_, policy, manager) = fixture(None).await;
    let cancellation = CancellationToken::new();
    let task = {
        let cancellation = cancellation.clone();
        tokio::spawn(async move { manager.run(cancellation).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(policy.reads.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::yield_now().await;
    assert_eq!(policy.reads.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(policy.reads.load(Ordering::SeqCst), 2);
    cancellation.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn selected_models_are_exact_and_support_more_than_two() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    let models = vec![
        "gpt-5.6-sol".to_owned(),
        "gpt-6-astra".to_owned(),
        "custom-model".to_owned(),
    ];
    store.set_session_models("acct_a", models.clone());
    for model in &models {
        mock_model(&proxy, "acct_a", model, success(model)).await;
    }
    let result = manager
        .refresh(&ProviderAccountId::new("acct_a").unwrap())
        .await
        .unwrap();
    assert_eq!(result.models.len(), 3);
    assert!(result.models.iter().all(|model| model.error.is_none()));
    for model in &models {
        let mut req = request(model);
        manager
            .rewrite(&store.account("acct_a").unwrap(), &mut req)
            .await;
        assert_eq!(req.turn_state.as_deref(), Some(state(model).as_str()));
    }
    store.set_session_models("acct_a", vec!["custom-model".to_owned()]);
    let mut req = request("gpt-6-astra");
    manager
        .rewrite(&store.account("acct_a").unwrap(), &mut req)
        .await;
    assert_eq!(req.turn_state.as_deref(), Some("client-state"));
    assert_eq!(proxy.received_requests().await.unwrap().len(), 3);
}

fn spawn_refresh(
    manager: &Arc<SessionManager>,
    id: &ProviderAccountId,
) -> tokio::task::JoinHandle<
    Result<
        gateway_admin::model::accounts::SessionStateRefresh,
        gateway_admin::ports::provider::ProviderAdminError,
    >,
> {
    let manager = Arc::clone(manager);
    let id = id.clone();
    tokio::spawn(async move { manager.refresh(&id).await })
}

async fn wait_for_requests(proxy: &MockServer, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while proxy.received_requests().await.unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_state(
    manager: &SessionManager,
    store: &MemoryAccountStore,
    model: &str,
    label: &str,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut req = request(model);
            manager
                .rewrite(&store.account("acct_a").unwrap(), &mut req)
                .await;
            if req.turn_state.as_deref() == Some(state(label).as_str()) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn first_three_rounds_are_single_then_configured_concurrency_prunes_successful_models() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    *policy.rewrite.lock().unwrap() =
        gateway_core::provider_ports::SessionRewritePolicy::try_new(3, 1).unwrap();
    mock_model(
        &proxy,
        "acct_a",
        "gpt-6-astra",
        success("early").set_delay(Duration::from_millis(100)),
    )
    .await;
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&attempts);
    Mock::given(method("POST"))
        .and(model_match("gpt-5.6-sol"))
        .respond_with(move |_: &wiremock::Request| {
            let mut attempts = observed.lock().unwrap();
            attempts.push(std::time::Instant::now());
            match attempts.len() {
                1 => response_with_state(&"A".repeat(312)),
                2..=9 => ResponseTemplate::new(503),
                _ => success("recovered").set_delay(Duration::from_millis(100)),
            }
        })
        .mount(&proxy)
        .await;
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = spawn_refresh(&manager, &id);
    wait_for_state(&manager, &store, "gpt-6-astra", "early").await;
    assert!(!pending.is_finished());
    let result = tokio::time::timeout(Duration::from_secs(26), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(result.models.iter().all(|item| item.error.is_none()));
    wait_for_state(&manager, &store, "gpt-5.6-sol", "recovered").await;
    {
        let times = attempts.lock().unwrap();
        assert_eq!(times.len(), 12);
        assert!(times[1].duration_since(times[0]) >= Duration::from_secs(6));
        assert!(times[2].duration_since(times[1]) >= Duration::from_secs(6));
        for start in [3, 6, 9] {
            assert!(times[start + 2].duration_since(times[start]) < Duration::from_secs(1));
            if start > 0 {
                assert!(times[start].duration_since(times[start - 1]) >= Duration::from_secs(1));
            }
        }
    }
    assert_eq!(proxy.received_requests().await.unwrap().len(), 13);
}

#[tokio::test]
async fn retry_rounds_pick_up_changed_global_concurrency_and_interval() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    store.set_session_models("acct_a", vec!["gpt-5.6-sol".to_owned()]);
    let times = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&times);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            observed.lock().unwrap().push(std::time::Instant::now());
            ResponseTemplate::new(503)
        })
        .mount(&proxy)
        .await;
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = spawn_refresh(&manager, &id);
    wait_for_requests(&proxy, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    *policy.rewrite.lock().unwrap() =
        gateway_core::provider_ports::SessionRewritePolicy::try_new(2, 2).unwrap();
    tokio::time::timeout(Duration::from_secs(26), async {
        while proxy.received_requests().await.unwrap().len() < 7 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let times = times.lock().unwrap();
    assert_eq!(times.len(), 7);
    assert!(times[1].duration_since(times[0]) >= Duration::from_secs(6));
    assert!(times[3].duration_since(times[2]) >= Duration::from_secs(6));
    assert!(times[4].duration_since(times[3]) < Duration::from_secs(1));
    assert!(times[5].duration_since(times[4]) >= Duration::from_secs(2));
}

#[tokio::test]
async fn every_non_292_length_is_rejected_and_previous_cache_is_preserved() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("previous")).await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    manager.refresh(&id).await.unwrap();
    policy.tickets.near_expiry();
    for length in [0, 291, 293, 312, 8193] {
        proxy.reset().await;
        for model in SESSION_KEEPALIVE_MODELS {
            mock_model(
                &proxy,
                "acct_a",
                model,
                response_with_state(&"A".repeat(length)),
            )
            .await;
        }
        let pending = spawn_refresh(&manager, &id);
        wait_for_requests(&proxy, 2).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "accepted invalid length {length}");
        for model in SESSION_KEEPALIVE_MODELS {
            wait_for_state(&manager, &store, model, "previous").await;
        }
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn first_success_does_not_wait_for_slower_siblings_or_overwrite_the_winner() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    store.set_session_models("acct_a", vec!["gpt-5.6-sol".to_owned()]);
    *policy.rewrite.lock().unwrap() =
        gateway_core::provider_ports::SessionRewritePolicy::try_new(3, 1).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(model_match("gpt-5.6-sol"))
        .respond_with(
            move |_: &wiremock::Request| match observed.fetch_add(1, Ordering::SeqCst) {
                0..=2 => ResponseTemplate::new(500),
                3 => success("winner").set_delay(Duration::from_millis(100)),
                _ => success("loser").set_delay(Duration::from_secs(2)),
            },
        )
        .mount(&proxy)
        .await;
    let id = ProviderAccountId::new("acct_a").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(26), manager.refresh(&id))
        .await
        .unwrap()
        .unwrap();
    assert!(result.models.iter().all(|item| item.error.is_none()));
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    wait_for_state(&manager, &store, "gpt-5.6-sol", "winner").await;
    assert_eq!(proxy.received_requests().await.unwrap().len(), 6);
}

#[tokio::test]
async fn cancellation_during_retry_sleep_stops_probes_and_releases_refresh_lock() {
    let proxy = MockServer::start().await;
    let (_, _, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, ResponseTemplate::new(500)).await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = spawn_refresh(&manager, &id);
    wait_for_requests(&proxy, 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(proxy.received_requests().await.unwrap().len(), 2);
    proxy.reset().await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("fresh")).await;
    }
    assert!(
        manager
            .refresh(&id)
            .await
            .unwrap()
            .models
            .iter()
            .all(|item| item.error.is_none())
    );
}

#[tokio::test]
async fn invalid_headers_and_first_wins_close_unfinished_http_responses() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinSet,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let (_, policy, manager) = fixture(Some(&proxy)).await;
    *policy.rewrite.lock().unwrap() =
        gateway_core::provider_ports::SessionRewritePolicy::try_new(3, 1).unwrap();
    let closed = Arc::new(AtomicUsize::new(0));
    let observed_closed = Arc::clone(&closed);
    let accepted = Arc::new(AtomicUsize::new(0));
    let observed_accepted = Arc::clone(&accepted);
    let server = tokio::spawn(async move {
        let attempts = Arc::new(Mutex::new(std::collections::HashMap::<String, usize>::new()));
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                        connection = listener.accept() => {
                            let (mut socket, _) = connection.unwrap();
                            let attempts = Arc::clone(&attempts);
                            *policy.rewrite.lock().unwrap() = gateway_core::provider_ports::SessionRewritePolicy::try_new(3, 1).unwrap();
            let closed = Arc::clone(&observed_closed);
                            observed_accepted.fetch_add(1, Ordering::SeqCst);
                            connections.spawn(async move {
                                let mut request_bytes = Vec::new();
                                let mut buffer = [0u8; 4096];
                                let body = loop {
                                    let read = socket.read(&mut buffer).await.unwrap();
                                    assert!(read > 0);
                                    request_bytes.extend_from_slice(&buffer[..read]);
                                    if let Some(end) = request_bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                        let headers = std::str::from_utf8(&request_bytes[..end]).unwrap();
                                        assert!(headers.lines().next().unwrap().ends_with("HTTP/1.1"));
                                        assert!(headers.to_ascii_lowercase().contains("connection: close"));
                                        let length = headers.lines().find_map(|line| {
                                            let (name, value) = line.split_once(':')?;
                                            name.eq_ignore_ascii_case("content-length")
                                                .then(|| value.trim().parse::<usize>().unwrap())
                                        }).unwrap();
                                        if request_bytes.len() >= end + 4 + length {
                                            break serde_json::from_slice::<serde_json::Value>(
                                                &zstd::stream::decode_all(&request_bytes[end + 4..end + 4 + length]).unwrap(),
                                            ).unwrap();
                                        }
                                    }
                                };
                                let attempt = {
                                    let mut attempts = attempts.lock().unwrap();
                                    let count = attempts.entry(body["model"].as_str().unwrap().to_owned()).or_default();
                                    *count += 1;
                                    *count
                                };
                                if attempt == 4 {
                                    // 等同轮三个请求都进入 HTTP 阶段，再返回首个完整成功。
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                    let response = format!(
                                        "HTTP/1.1 200 OK\r\nx-codex-turn-state: {}\r\nContent-Length: 100000\r\n\r\n",
                                        state("winner"),
                                    );
                                    socket.write_all(response.as_bytes()).await.unwrap();
                                    match socket.read(&mut buffer).await {
                                        Ok(0) | Err(_) => { closed.fetch_add(1, Ordering::SeqCst); }
                                        Ok(_) => panic!("valid Header should close without reading SSE"),
                                    }
                                } else {
                                    // 首轮三个非法长度与后续落败探针都只发 Header，响应体永久悬挂。
                                    let state = if attempt <= 3 { "A".repeat(312) } else { state("pending") };
                                    let response = format!(
                                        "HTTP/1.1 200 OK\r\nx-codex-turn-state: {state}\r\nContent-Length: 100000\r\n\r\n",
                                    );
                                    if attempt <= 3 { socket.write_all(response.as_bytes()).await.unwrap(); }
                                    match socket.read(&mut buffer).await {
                                        Ok(0) | Err(_) => { closed.fetch_add(1, Ordering::SeqCst); }
                                        Ok(_) => panic!("unexpected bytes on unfinished response"),
                                    }
                                }
                            });
                        }
                        Some(result) = connections.join_next(), if !connections.is_empty() => {
                            result.unwrap();
                        }
                    }
        }
    });
    let id = ProviderAccountId::new("acct_a").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(26), manager.refresh(&id)).await;
    let disconnected = tokio::time::timeout(Duration::from_secs(1), async {
        while closed.load(Ordering::SeqCst) < 12 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert!(
        result
            .unwrap()
            .unwrap()
            .models
            .iter()
            .all(|item| item.error.is_none())
    );
    disconnected.unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 12);
}

#[tokio::test]
async fn manual_progress_reports_cached_success_while_another_model_keeps_retrying() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    mock_model(&proxy, "acct_a", "gpt-6-astra", success("visible")).await;
    mock_model(&proxy, "acct_a", "gpt-5.6-sol", ResponseTemplate::new(503)).await;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let observer = Arc::new(move |item| {
        sender.send(item).unwrap();
    });
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = {
        let manager = Arc::clone(&manager);
        let id = id.clone();
        tokio::spawn(async move { manager.refresh_with_progress(&id, Some(observer)).await })
    };
    let first = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.model, "gpt-6-astra");
    assert!(first.error.is_none());
    assert!(first.expire_at.is_some());
    wait_for_state(&manager, &store, "gpt-6-astra", "visible").await;
    assert!(!pending.is_finished());
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(receiver.recv().await.is_none());
    // 前端断开进度流后，已经成功写入的缓存仍可服务业务。
    wait_for_state(&manager, &store, "gpt-6-astra", "visible").await;
}

#[tokio::test]
async fn tickets_restore_after_restart_skip_fresh_refresh_near_expiry_and_fail_closed() {
    let proxy = MockServer::start().await;
    let (store, policy, manager) = fixture(Some(&proxy.uri())).await;
    let account = store.account("acct_a").unwrap();
    assert!(!manager.available(&account, "gpt-5.6-sol").await);
    assert!(manager.available(&account, "unmanaged-model").await);
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, success("durable")).await;
    }
    manager.refresh(account.id()).await.unwrap();
    assert!(manager.available(&account, "gpt-5.6-sol").await);
    let restored = SessionManager::new(
        store.repository(),
        policy.clone(),
        wire_profile(),
        "http://upstream.invalid/backend-api".to_owned(),
        Some(policy.tickets.clone()),
    );
    assert!(restored.available(&account, "gpt-5.6-sol").await);
    restored.refresh(account.id()).await.unwrap();
    assert_eq!(proxy.received_requests().await.unwrap().len(), 2);
    policy.tickets.near_expiry();
    restored.refresh(account.id()).await.unwrap();
    assert_eq!(proxy.received_requests().await.unwrap().len(), 4);
    policy.tickets.unavailable.store(true, Ordering::SeqCst);
    assert!(!restored.available(&account, "gpt-5.6-sol").await);
    assert!(
        !restored
            .rewrite(&account, &mut request("gpt-5.6-sol"))
            .await
    );
    policy.tickets.unavailable.store(false, Ordering::SeqCst);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3601)).await;
    assert!(!restored.available(&account, "gpt-5.6-sol").await);
}

#[tokio::test]
async fn http_200_exact_length_and_prefix_are_required_without_consuming_body() {
    for (status, value, valid) in [
        (200, state("header-only"), true),
        (201, state("created"), false),
        (200, "A".repeat(292), false),
        (200, format!("gAAAAA{}", "A".repeat(800)), false),
    ] {
        let proxy = MockServer::start().await;
        let (store, _, manager) = fixture(Some(&proxy.uri())).await;
        store.set_session_models("acct_a", vec!["gpt-5.6-sol".to_owned()]);
        mock_model(
            &proxy,
            "acct_a",
            "gpt-5.6-sol",
            ResponseTemplate::new(status)
                .insert_header("x-codex-turn-state", value)
                .set_body_string("not SSE"),
        )
        .await;
        let pending = spawn_refresh(&manager, &ProviderAccountId::new("acct_a").unwrap());
        wait_for_requests(&proxy, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            manager
                .available(&store.account("acct_a").unwrap(), "gpt-5.6-sol")
                .await,
            valid
        );
        assert_eq!(pending.is_finished(), valid);
        pending.abort();
        let _ = pending.await;
    }
}

#[tokio::test]
async fn background_bad_account_does_not_block_other_accounts_or_reprobe_fresh_tickets() {
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(&proxy, "acct_a", model, ResponseTemplate::new(503)).await;
        mock_model(&proxy, "acct_b", model, success("other-account")).await;
    }
    let cancellation = CancellationToken::new();
    let task = {
        let manager = manager.clone();
        let cancel = cancellation.clone();
        tokio::spawn(async move { manager.run(cancel).await })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !manager
            .available(&store.account("acct_b").unwrap(), "gpt-5.6-sol")
            .await
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(6200)).await;
    cancellation.cancel();
    task.await.unwrap().unwrap();
    let requests = proxy.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.headers.get("authorization").unwrap() == "Bearer acct_b")
            .count(),
        2
    );
    assert!(
        requests
            .iter()
            .filter(|r| r.headers.get("authorization").unwrap() == "Bearer acct_a")
            .count()
            >= 4
    );
}

#[tokio::test]
async fn selector_skips_missing_ticket_and_recovers_only_the_ready_account_model() {
    use crate::support::{
        MemoryCooldownPort, MemorySessionAffinity, MemorySessionExclusions, TestLeaseCoordinator,
    };
    use gateway_core::{
        account::AccountFeedbackStats,
        engine::{AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext},
        policy::ClientApiKeyId,
        routing::{
            ClientRoutingScope, FrozenAccountScope, ProviderKind, RuntimeAccount,
            RuntimeAccountDirectory,
        },
    };
    use provider_openai::credential::{
        CodexCookiePolicy, CodexCredentialQuotaService, CodexCredentialSelector,
        SelectCodexCredential,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        time::SystemTime,
    };
    let (store, policy, manager) = fixture(None).await;
    let leases = Arc::new(TestLeaseCoordinator::default());
    let quota = Arc::new(CodexCredentialQuotaService::new(
        store.repository(),
        wire_profile(),
        reqwest::Client::new(),
        provider_openai::OFFICIAL_CODEX_BASE_URL.to_owned(),
        Arc::new(MemoryCooldownPort::default()),
        leases.clone(),
        crate::support::runtime_policy(),
    ));
    let selector = CodexCredentialSelector::new(
        ProviderKind::new("openai").unwrap(),
        store.repository(),
        leases,
        Arc::new(MemorySessionAffinity::default()),
        Arc::new(MemorySessionExclusions::default()),
        quota,
        Arc::new(AccountFeedbackStats::default()),
        CodexCookiePolicy::official().unwrap(),
    )
    .with_session_manager(manager);
    let accounts = ["acct_a", "acct_b"]
        .into_iter()
        .map(|id| {
            (
                ProviderAccountId::new(id).unwrap(),
                RuntimeAccount::new(ProviderKind::new("openai").unwrap(), BTreeSet::new()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let scope = Arc::new(FrozenAccountScope::new(
        Arc::new(RuntimeAccountDirectory::new(accounts)),
        ClientRoutingScope::all_accounts(),
    ));
    let attempt = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_ticket_test").unwrap(),
            ClientApiKeyId::new("key_ticket_test").unwrap(),
        )
        .with_session_keepalive_enabled(true),
        NonZeroU32::new(1).unwrap(),
        SystemTime::now() + Duration::from_secs(30),
        crate::support::account_policy(),
        AccountAttemptContext::new(BTreeSet::new(), None, None).with_account_scope(scope),
        None,
        CancellationToken::new(),
    );
    let url = url::Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap();
    let selection = SelectCodexCredential {
        upstream_model: "gpt-5.6-sol",
        request_url: &url,
        attempt: &attempt,
        session_affinity_key: None,
    };
    assert!(selector.select(&selection).await.is_err());
    let account = store.account("acct_b").unwrap();
    policy
        .tickets
        .store(
            account.id(),
            "gpt-5.6-sol",
            &ProviderSessionTicket {
                value: state("ready"),
                credential_revision: account.revision().get(),
                credential_binding: None,
                expires_at: Utc::now().timestamp() + 3600,
            },
        )
        .await
        .unwrap();
    let lease = selector.select(&selection).await.unwrap();
    assert_eq!(lease.account_id(), account.id());
    drop(lease);
    let other = SelectCodexCredential {
        upstream_model: "gpt-6-astra",
        ..selection
    };
    assert!(selector.select(&other).await.is_err());
    policy.tickets.unavailable.store(true, Ordering::SeqCst);
    assert!(selector.select(&selection).await.is_err());
}

#[tokio::test]
async fn legacy_ticket_is_accepted_only_while_its_credential_revision_matches() {
    let (store, policy, manager) = fixture(None).await;
    let current = store.account("acct_a").unwrap();
    let legacy: ProviderSessionTicket = serde_json::from_value(json!({
        "value": state("legacy"), "credential_revision": current.revision().get(),
        "expires_at": Utc::now().timestamp() + 3600
    }))
    .unwrap();
    assert!(legacy.credential_binding.is_none());
    policy
        .tickets
        .store(current.id(), "gpt-6-astra", &legacy)
        .await
        .unwrap();
    assert!(manager.available(&current, "gpt-6-astra").await);
    capture_cookie(&store).await;
    assert!(
        !manager
            .available(&store.account("acct_a").unwrap(), "gpt-6-astra")
            .await
    );
}

#[tokio::test]
async fn credential_rotation_during_probe_discards_old_identity_result() {
    use provider_openai::credential::CodexCredentialData;
    let proxy = MockServer::start().await;
    let (store, _, manager) = fixture(Some(&proxy.uri())).await;
    for model in SESSION_KEEPALIVE_MODELS {
        mock_model(
            &proxy,
            "acct_a",
            model,
            success("stale").set_delay(Duration::from_millis(250)),
        )
        .await;
    }
    let id = ProviderAccountId::new("acct_a").unwrap();
    let pending = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.refresh(&id).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while proxy.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let account = store.account("acct_a").unwrap();
    let repository = store.repository();
    let mut data = repository.load_complete_data(&account).await.unwrap();
    let CodexCredentialData::OAuth(ref mut oauth) = data else {
        panic!("OAuth fixture");
    };
    oauth.access_token = "rotated-during-probe".to_owned();
    repository
        .compare_and_swap_data(&account, data)
        .await
        .unwrap();
    let result = pending.await.unwrap().unwrap();
    assert!(result.models.iter().all(|item| item.error.is_some()));
    for model in SESSION_KEEPALIVE_MODELS {
        assert!(
            !manager
                .available(&store.account("acct_a").unwrap(), model)
                .await
        );
    }
}
