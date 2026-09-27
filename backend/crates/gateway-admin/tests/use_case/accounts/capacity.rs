use std::collections::BTreeMap;

use gateway_admin::ports::store::AccountRuntimeStore;

use super::*;

struct CapacityRuntime {
    counts: Option<BTreeMap<String, u64>>,
    fail: bool,
    reads: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl AccountRuntimeStore for CapacityRuntime {
    async fn active_rate_limits(&self) -> AdminStoreResult<AccountRuntimeSnapshot> {
        Ok(AccountRuntimeSnapshot::default())
    }

    async fn account_runtime(&self, ids: &[String]) -> AdminStoreResult<AccountRuntimeSnapshot> {
        self.reads.lock().unwrap().push(ids.to_vec());
        if self.fail {
            return Err(store_unavailable());
        }
        Ok(AccountRuntimeSnapshot {
            in_flight: self.counts.clone(),
            ..Default::default()
        })
    }

    async fn active_freezes(
        &self,
    ) -> AdminStoreResult<BTreeMap<String, gateway_admin::model::accounts::AccountFreeze>> {
        Ok(BTreeMap::new())
    }

    async fn capacity_peaks(&self, _: &[String]) -> AdminStoreResult<BTreeMap<String, u32>> {
        Ok(BTreeMap::new())
    }

    async fn finish_freeze(
        &self,
        _: &str,
        _: &gateway_admin::model::accounts::AccountFreeze,
        _: Option<chrono::DateTime<Utc>>,
    ) -> AdminStoreResult<bool> {
        Ok(false)
    }
}

#[tokio::test]
async fn account_capacity_should_batch_page_ids_and_distinguish_idle_from_unavailable() {
    for (counts, fail, expected) in [
        (
            Some(BTreeMap::from([("acct_test".to_owned(), 3)])),
            false,
            Some(3),
        ),
        (Some(BTreeMap::new()), false, Some(0)),
        (None, false, None),
        (None, true, None),
    ] {
        for empty in [false, true] {
            let runtime = Arc::new(CapacityRuntime {
                counts: counts.clone(),
                fail,
                reads: Mutex::default(),
            });
            let mut account = account_record("openai");
            account.concurrency_limit = gateway_core::account::AccountConcurrencyLimit::new(5);
            let store = FakeAccountStore::with_account(account, events());
            if empty {
                store.set_accounts(Vec::new());
            }
            let services = super::super::AdminHarness::new()
                .accounts(store)
                .account_runtime(runtime.clone())
                .settings(Arc::new(StaticSettingsStore))
                .provider(FakeProviderAdmin::new("openai", events()))
                .build()
                .await;
            let page = services
                .accounts()
                .list(AccountListQuery {
                    page: 1,
                    page_size: gateway_admin::model::PageSize::new(20).unwrap(),
                    provider_kind: None,
                    group_filter: None,
                    search: None,
                    status: None,
                    sort: None,
                })
                .await
                .unwrap();
            if empty {
                assert!(page.items.is_empty());
                assert!(runtime.reads.lock().unwrap().is_empty());
            } else {
                assert_eq!(
                    *runtime.reads.lock().unwrap(),
                    [vec!["acct_test".to_owned()]]
                );
                assert_eq!(page.items[0].capacity.used_slots, expected);
                assert_eq!(page.items[0].capacity.total_slots, Some(5));
            }
        }
    }
}
