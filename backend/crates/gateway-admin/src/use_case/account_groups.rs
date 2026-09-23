//! Account group management use cases.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::SystemTime,
};

use async_trait::async_trait;
use gateway_core::{
    account::{AccountStatus, resolve_account_status},
    routing::AccountGroupId,
    runtime::SnapshotControl,
};
use uuid::Uuid;

use crate::{
    model::{
        AdminError, MutationContext,
        account_groups::{
            AccountGroupAccountSummary, AccountGroupCapacity, AccountGroupListQuery,
            AccountGroupMemberFact, AccountGroupMutation, AccountGroupPage, AccountGroupRecord,
            CreateAccountGroup, DeleteAccountGroup, NewAccountGroup, SetAccountGroupEnabled,
            UpdateAccountGroup,
        },
    },
    ports::store::{AccountGroupStore, AccountRuntimeStore},
};

use super::{map_store_error, publish_committed};

/// API-facing account group management service.
#[async_trait]
pub trait AccountGroupService: Send + Sync {
    async fn car_quota_settings(
        &self,
    ) -> Result<crate::model::account_groups::CarQuotaSettings, AdminError> {
        Err(AdminError::invalid("car 额度设置不可用"))
    }
    async fn replace_car_quota_settings(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::ReplaceCarQuotaSettings,
    ) -> Result<crate::model::account_groups::CarQuotaSettings, AdminError> {
        let _ = (context, command);
        Err(AdminError::invalid("car 额度设置不可用"))
    }
    async fn car_accounts(
        &self,
    ) -> Result<Vec<crate::model::account_groups::CarAccount>, AdminError> {
        Ok(Vec::new())
    }
    async fn car_quota_state(
        &self,
        group_id: AccountGroupId,
    ) -> Result<crate::model::account_groups::CarQuotaState, AdminError> {
        let _ = group_id;
        Err(AdminError::invalid("car 周期状态不可用"))
    }
    async fn reconcile_car_quota(
        &self,
        observation: crate::model::account_groups::CarQuotaObservation,
    ) -> Result<crate::model::account_groups::CarQuotaState, AdminError> {
        let _ = observation;
        Err(AdminError::invalid("car 周期状态不可用"))
    }
    async fn save_car_weights(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::SaveCarWeights,
    ) -> Result<crate::model::Revision, AdminError> {
        let _ = (context, command);
        Err(AdminError::invalid("car 权重不可用"))
    }
    async fn convert_to_car(
        &self,
        context: &MutationContext,
        id: AccountGroupId,
    ) -> Result<crate::model::Revision, AdminError>;
    async fn seats(
        &self,
        group_id: AccountGroupId,
    ) -> Result<Vec<crate::model::account_groups::SeatRecord>, AdminError>;
    async fn save_seat(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::SaveSeat,
    ) -> Result<crate::model::Revision, AdminError>;
    async fn join_seat(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::JoinSeat,
    ) -> Result<crate::model::Revision, AdminError>;
    async fn list(&self, query: AccountGroupListQuery) -> Result<AccountGroupPage, AdminError>;
    async fn create(
        &self,
        context: &MutationContext,
        command: CreateAccountGroup,
    ) -> Result<AccountGroupMutation, AdminError>;
    async fn update(
        &self,
        context: &MutationContext,
        command: UpdateAccountGroup,
    ) -> Result<AccountGroupMutation, AdminError>;
    async fn set_enabled(
        &self,
        context: &MutationContext,
        command: SetAccountGroupEnabled,
    ) -> Result<AccountGroupMutation, AdminError>;
    async fn delete(
        &self,
        context: &MutationContext,
        command: DeleteAccountGroup,
    ) -> Result<AccountGroupMutation, AdminError>;
}

pub(crate) struct DefaultAccountGroupService {
    store: Arc<dyn AccountGroupStore>,
    runtime: Arc<dyn AccountRuntimeStore>,
    snapshot: Arc<dyn SnapshotControl>,
}

impl DefaultAccountGroupService {
    #[must_use]
    pub(crate) fn new(
        store: Arc<dyn AccountGroupStore>,
        runtime: Arc<dyn AccountRuntimeStore>,
        snapshot: Arc<dyn SnapshotControl>,
    ) -> Self {
        Self {
            store,
            runtime,
            snapshot,
        }
    }

    async fn enrich_records(&self, records: &mut [AccountGroupRecord]) -> Result<(), AdminError> {
        if records.is_empty() {
            return Ok(());
        }
        let group_ids = records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let members = self
            .store
            .load_account_group_members(&group_ids)
            .await
            .map_err(|error| map_store_error(error, "account group members"))?;
        let account_ids = members
            .iter()
            .map(|member| member.account_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let runtime = match self.runtime.account_runtime(&account_ids).await {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(error = %error, "account group runtime projection is unavailable");
                Default::default()
            }
        };
        project_group_runtime(records, &members, &runtime);
        Ok(())
    }

    async fn publish(
        &self,
        result: Result<AccountGroupMutation, crate::ports::store::AdminStoreError>,
    ) -> Result<AccountGroupMutation, AdminError> {
        let mut result = result.map_err(|error| map_store_error(error, "account group"))?;
        if let Some(record) = result.record.as_mut() {
            self.enrich_records(std::slice::from_mut(record)).await?;
        }
        publish_committed(self.snapshot.as_ref(), result.config_revision).await?;
        Ok(result)
    }
}

fn valid_weight(value: gateway_core::metering::Decimal) -> bool {
    let canonical = value.canonical();
    value != gateway_core::metering::Decimal::ZERO
        && !canonical.starts_with('-')
        && canonical
            .split('.')
            .next()
            .is_some_and(|integer| integer.len() <= 10)
        && canonical
            .split_once('.')
            .is_none_or(|(_, fraction)| fraction.len() <= 2)
}

#[async_trait]
impl AccountGroupService for DefaultAccountGroupService {
    async fn car_quota_settings(
        &self,
    ) -> Result<crate::model::account_groups::CarQuotaSettings, AdminError> {
        self.store
            .load_car_quota_settings()
            .await
            .map_err(|e| map_store_error(e, "car quota settings"))
    }

    async fn replace_car_quota_settings(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::ReplaceCarQuotaSettings,
    ) -> Result<crate::model::account_groups::CarQuotaSettings, AdminError> {
        self.store
            .replace_car_quota_settings(command, context)
            .await
            .map_err(|e| map_store_error(e, "car quota settings"))
    }

    async fn car_accounts(
        &self,
    ) -> Result<Vec<crate::model::account_groups::CarAccount>, AdminError> {
        self.store
            .list_car_accounts()
            .await
            .map_err(|e| map_store_error(e, "car accounts"))
    }

    async fn car_quota_state(
        &self,
        group_id: AccountGroupId,
    ) -> Result<crate::model::account_groups::CarQuotaState, AdminError> {
        self.store
            .load_car_quota_state(group_id)
            .await
            .map_err(|e| map_store_error(e, "car quota state"))
    }

    async fn reconcile_car_quota(
        &self,
        observation: crate::model::account_groups::CarQuotaObservation,
    ) -> Result<crate::model::account_groups::CarQuotaState, AdminError> {
        self.store
            .reconcile_car_quota(observation)
            .await
            .map_err(|e| map_store_error(e, "car quota reconcile"))
    }

    async fn save_car_weights(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::SaveCarWeights,
    ) -> Result<crate::model::Revision, AdminError> {
        if !valid_weight(command.total_weight) {
            return Err(AdminError::invalid("car 总权重应为最多两位小数的正数"));
        }
        let revision = self
            .store
            .save_car_weights(command, context)
            .await
            .map_err(|e| map_store_error(e, "car weights"))?;
        publish_committed(self.snapshot.as_ref(), revision).await?;
        Ok(revision)
    }
    async fn convert_to_car(
        &self,
        context: &MutationContext,
        id: AccountGroupId,
    ) -> Result<crate::model::Revision, AdminError> {
        let revision = self
            .store
            .convert_to_car(id, context)
            .await
            .map_err(|e| map_store_error(e, "car"))?;
        publish_committed(self.snapshot.as_ref(), revision).await?;
        Ok(revision)
    }

    async fn seats(
        &self,
        group_id: AccountGroupId,
    ) -> Result<Vec<crate::model::account_groups::SeatRecord>, AdminError> {
        self.store
            .list_seats(group_id)
            .await
            .map_err(|e| map_store_error(e, "seat"))
    }

    async fn save_seat(
        &self,
        context: &MutationContext,
        mut command: crate::model::account_groups::SaveSeat,
    ) -> Result<crate::model::Revision, AdminError> {
        if command.name.trim() != command.name
            || command.name.is_empty()
            || command.name.chars().count() > 100
            || command.name.chars().any(char::is_control)
            || command.max_concurrency == 0
            || command.max_concurrency > u64::from(u32::MAX)
            || !valid_weight(command.weight)
        {
            return Err(AdminError::invalid("seat 名称、并发上限或权重无效"));
        }
        if command.id.is_none() {
            command.id = Some(
                gateway_core::policy::SeatId::new(format!("seat_{}", Uuid::now_v7().simple()))
                    .map_err(|_| AdminError::internal("创建 seat ID 失败"))?,
            );
        }
        let revision = self
            .store
            .save_seat(command, context)
            .await
            .map_err(|e| map_store_error(e, "seat"))?;
        publish_committed(self.snapshot.as_ref(), revision).await?;
        Ok(revision)
    }

    async fn join_seat(
        &self,
        context: &MutationContext,
        command: crate::model::account_groups::JoinSeat,
    ) -> Result<crate::model::Revision, AdminError> {
        if command.key_ids.is_empty() || command.key_ids.len() > 100 {
            return Err(AdminError::invalid("每次迁入 1 至 100 个 Key"));
        }
        let revision = self
            .store
            .join_seat(command, context)
            .await
            .map_err(|e| map_store_error(e, "seat"))?;
        publish_committed(self.snapshot.as_ref(), revision).await?;
        Ok(revision)
    }
    async fn list(&self, query: AccountGroupListQuery) -> Result<AccountGroupPage, AdminError> {
        let mut page = self
            .store
            .list_account_groups(query)
            .await
            .map_err(|error| map_store_error(error, "account group"))?;
        self.enrich_records(&mut page.items).await?;
        Ok(page)
    }

    async fn create(
        &self,
        context: &MutationContext,
        command: CreateAccountGroup,
    ) -> Result<AccountGroupMutation, AdminError> {
        let id = AccountGroupId::new(format!("grp_{}", Uuid::now_v7().simple()))
            .map_err(|_| AdminError::internal("创建账号组 ID 失败"))?;
        self.publish(
            self.store
                .create_account_group(
                    NewAccountGroup {
                        id,
                        name: command.name,
                        description: command.description,
                        disable_fast: command.disable_fast,
                        color: command.color,
                    },
                    context,
                )
                .await,
        )
        .await
    }

    async fn update(
        &self,
        context: &MutationContext,
        command: UpdateAccountGroup,
    ) -> Result<AccountGroupMutation, AdminError> {
        self.publish(self.store.update_account_group(command, context).await)
            .await
    }

    async fn set_enabled(
        &self,
        context: &MutationContext,
        command: SetAccountGroupEnabled,
    ) -> Result<AccountGroupMutation, AdminError> {
        self.publish(self.store.set_account_group_enabled(command, context).await)
            .await
    }

    async fn delete(
        &self,
        context: &MutationContext,
        command: DeleteAccountGroup,
    ) -> Result<AccountGroupMutation, AdminError> {
        self.publish(self.store.delete_account_group(command, context).await)
            .await
    }
}

fn project_group_runtime(
    records: &mut [AccountGroupRecord],
    members: &[AccountGroupMemberFact],
    runtime: &crate::model::accounts::AccountRuntimeSnapshot,
) {
    let now = SystemTime::now();
    let mut status_by_account = BTreeMap::new();
    let mut slots_by_account = BTreeMap::new();
    let mut accounts_by_group = BTreeMap::<&str, Vec<&str>>::new();
    for member in members {
        let mut status = member.status.clone();
        status.cooldown = runtime.cooldown.get(&member.account_id).copied();
        status_by_account
            .entry(member.account_id.as_str())
            .or_insert_with(|| resolve_account_status(&status, now).status);
        slots_by_account
            .entry(member.account_id.as_str())
            .or_insert(member.total_slots);
        accounts_by_group
            .entry(member.group_id.as_str())
            .or_default()
            .push(member.account_id.as_str());
    }
    for record in records {
        let account_ids = accounts_by_group
            .get(record.id.as_str())
            .map_or(&[][..], Vec::as_slice);
        let available_ids = account_ids
            .iter()
            .copied()
            .filter(|account_id| status_by_account.get(account_id) == Some(&AccountStatus::Normal))
            .collect::<Vec<_>>();
        let total = u64::try_from(account_ids.len()).unwrap_or(u64::MAX);
        let available = u64::try_from(available_ids.len()).unwrap_or(u64::MAX);
        record.account_summary = AccountGroupAccountSummary {
            available,
            limited: total.saturating_sub(available),
            total,
        };
        record.capacity = AccountGroupCapacity {
            used_slots: if available_ids.is_empty() {
                Some(0)
            } else {
                runtime.in_flight.as_ref().map(|in_flight| {
                    available_ids.iter().fold(0_u64, |sum, account_id| {
                        sum.saturating_add(in_flight.get(*account_id).copied().unwrap_or(0))
                    })
                })
            },
            total_slots: available_ids.iter().try_fold(0_u64, |sum, account_id| {
                Some(sum.saturating_add(slots_by_account.get(*account_id).copied().flatten()?))
            }),
        };
    }
}
