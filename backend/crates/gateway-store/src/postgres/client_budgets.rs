//! 按 Key 串行检查限额，并幂等累计已取得的 USD 费用。

use gateway_admin::model::audit::MutationAuditOperation;
use std::{collections::BTreeMap, sync::Mutex, time::Duration};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use gateway_admin::model::{
    MutationContext,
    client_keys::{
        ChangeClientKeyWeeklyWindow, ClientKeyBudgetMutationOrigin, ClientKeyBudgetPeriod,
        ClientKeyWeeklyControl, ClientKeyWeeklyWindowAction, ResetClientKeyBudget,
    },
    plugin_resources::PluginResourceOwner,
};
use gateway_admin::ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult};
use gateway_core::{
    engine::budget::{
        ClientBudgetCharge, ClientBudgetError, ClientBudgetLimits, ClientBudgetPort,
        ClientBudgetStatus,
    },
    error::{GatewayError, GatewayErrorKind},
    metering::Decimal,
    policy::ClientApiKeyId,
};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::{StoreError, StoreResult, mutation_audit, postgres_unavailable};

pub(super) async fn reset_client_key_budget(
    pool: &PgPool,
    command: ResetClientKeyBudget,
    origin: ClientKeyBudgetMutationOrigin,
    context: &MutationContext,
) -> AdminStoreResult<()> {
    let mut tx = match &origin {
        ClientKeyBudgetMutationOrigin::Admin => pool.begin().await.map_err(|_| {
            crate::admin_store_error(
                "client API key budget",
                postgres_unavailable("begin budget reset"),
            )
        })?,
        ClientKeyBudgetMutationOrigin::Plugin(owner) => {
            super::plugins::begin_plugin_mutation(pool, owner).await?
        }
    };
    reset_client_key_budget_in_transaction(&mut tx, &command, context)
        .await
        .map_err(|error| crate::admin_store_error("client API key budget", error))?;
    tx.commit().await.map_err(|_| {
        crate::admin_store_error(
            "client API key budget",
            postgres_unavailable("commit budget reset"),
        )
    })
}

async fn reset_client_key_budget_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    command: &ResetClientKeyBudget,
    context: &MutationContext,
) -> StoreResult<()> {
    // 与准入、结算共用 Key 行锁，重置边界必须在取得锁之后确定。
    let exists =
        sqlx::query_scalar::<_, String>("select id from client_api_keys where id = $1 for update")
            .bind(command.id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| postgres_unavailable("lock budget reset key"))?;
    if exists.is_none() {
        return Err(StoreError::NotFound {
            entity: "client API key",
            id: command.id.as_str().to_owned(),
        });
    }
    let daily = matches!(
        command.period,
        ClientKeyBudgetPeriod::Daily | ClientKeyBudgetPeriod::All
    );
    let weekly = matches!(
        command.period,
        ClientKeyBudgetPeriod::Weekly | ClientKeyBudgetPeriod::All
    );
    let reset_at = Utc::now();
    // 推进计费起点，避免重置前完成、稍后落盘的费用重新扣额；未使用的 Key 不开启窗口。
    sqlx::query(
        "update client_key_budget_windows set
        daily_used_usd = case when $2 then 0 else daily_used_usd end,
        daily_start = case when $2 and daily_end > $4 then $4 else daily_start end,
        weekly_used_usd = case when $3 then 0 else weekly_used_usd end,
        weekly_start = case when $3 and (weekly_end > $4 or weekly_controller is not null) then $4 else weekly_start end
        where client_api_key_id = $1",
    )
    .bind(command.id.as_str())
    .bind(daily)
    .bind(weekly)
    .bind(reset_at)
    .execute(&mut **tx)
    .await
    .map_err(|_| postgres_unavailable("reset client budget"))?;
    let mut fields = Vec::new();
    if daily {
        fields.extend(["daily_used_usd".to_owned(), "daily_start".to_owned()]);
    }
    if weekly {
        fields.extend(["weekly_used_usd".to_owned(), "weekly_start".to_owned()]);
    }
    super::append_admin_audit_event_in_transaction(
        tx,
        mutation_audit(
            context,
            MutationAuditOperation::ClientApiKeyResetBudget,
            command.id.as_str(),
            fields,
        ),
        None,
    )
    .await?;
    Ok(())
}

fn control_error(kind: AdminStoreErrorKind) -> AdminStoreError {
    AdminStoreError::new(
        kind,
        "client API key weekly window",
        "weekly window control operation failed",
    )
}

fn control_unavailable(_: sqlx::Error) -> AdminStoreError {
    control_error(AdminStoreErrorKind::Unavailable)
}

/// 接管期间才报告窗口事实；未接管时原生窗口由 `get_budget` 读取。
fn control_from_row(
    row: &sqlx::postgres::PgRow,
    now: DateTime<Utc>,
) -> AdminStoreResult<ClientKeyWeeklyControl> {
    let controller: Option<String> = row.get("weekly_controller");
    let revision = u64::try_from(row.get::<i64, _>("revision"))
        .map_err(|_| control_error(AdminStoreErrorKind::Unavailable))?;
    if controller.is_none() {
        return Ok(ClientKeyWeeklyControl {
            revision,
            controller,
            expires_at: None,
            accounting_start: None,
            waiting: false,
        });
    }
    let expires_at: DateTime<Utc> = row.get("weekly_end");
    Ok(ClientKeyWeeklyControl {
        revision,
        controller,
        expires_at: Some(expires_at),
        accounting_start: Some(row.get("weekly_start")),
        waiting: expires_at <= now,
    })
}

pub(super) async fn weekly_control(
    pool: &PgPool,
    id: &ClientApiKeyId,
) -> AdminStoreResult<ClientKeyWeeklyControl> {
    let row = sqlx::query(
        "select coalesce(w.weekly_control_revision, 0) as revision, w.weekly_controller,
        w.weekly_start, w.weekly_end
        from client_api_keys k left join client_key_budget_windows w on w.client_api_key_id = k.id
        where k.id = $1",
    )
    .bind(id.as_str())
    .fetch_optional(pool)
    .await
    .map_err(control_unavailable)?
    .ok_or_else(|| control_error(AdminStoreErrorKind::NotFound))?;
    control_from_row(&row, Utc::now())
}

/// 接管变更与准入、结算共用 Key 行锁；版本、重试指纹和窗口在同一事务提交。
pub(super) async fn change_weekly_control(
    pool: &PgPool,
    owner: &PluginResourceOwner,
    command: ChangeClientKeyWeeklyWindow,
    context: &MutationContext,
) -> AdminStoreResult<ClientKeyWeeklyControl> {
    let mut tx = super::plugins::begin_plugin_mutation(pool, owner).await?;
    let exists =
        sqlx::query_scalar::<_, String>("select id from client_api_keys where id = $1 for update")
            .bind(command.id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(control_unavailable)?;
    if exists.is_none() {
        return Err(control_error(AdminStoreErrorKind::NotFound));
    }
    let now = Utc::now();
    advance_windows(&mut tx, command.id.as_str(), now)
        .await
        .map_err(control_unavailable)?;
    let row = sqlx::query(
        "select weekly_control_revision as revision, weekly_controller, weekly_last_operation,
        weekly_start, weekly_end from client_key_budget_windows where client_api_key_id = $1",
    )
    .bind(command.id.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(control_unavailable)?;
    let revision: i64 = row.get("revision");
    let controller: Option<String> = row.get("weekly_controller");
    let expected = i64::try_from(command.expected_revision)
        .map_err(|_| control_error(AdminStoreErrorKind::Invalid))?;
    let next = expected
        .checked_add(1)
        .ok_or_else(|| control_error(AdminStoreErrorKind::Invalid))?;
    let operation = serde_json::json!({
        "owner": owner.instance_id,
        "expected_revision": expected,
        "operation": command.action,
    });
    let previous: Option<serde_json::Value> = row.get("weekly_last_operation");
    if revision == next && previous.as_ref() == Some(&operation) {
        // 返回已提交的版本；原样重试不触碰计费起点或已用金额。
        return control_from_row(&row, now);
    }
    if revision != expected {
        return Err(control_error(AdminStoreErrorKind::StaleRevision));
    }
    let owns = controller.as_deref() == Some(owner.instance_id.as_str());
    match command.action {
        ClientKeyWeeklyWindowAction::Claim {
            expires_at,
            clear_used,
        } => {
            require_future(expires_at, now)?;
            if controller.is_some() {
                return Err(control_error(AdminStoreErrorKind::Conflict));
            }
            sqlx::query(
                "update client_key_budget_windows set weekly_controller = $2, weekly_end = $3,
                weekly_used_usd = case when $4 then 0 else weekly_used_usd end,
                weekly_start = case when $4 then $5 else weekly_start end
                where client_api_key_id = $1",
            )
            .bind(command.id.as_str())
            .bind(&owner.instance_id)
            .bind(expires_at)
            .bind(clear_used)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(control_unavailable)?;
        }
        ClientKeyWeeklyWindowAction::Sync { expires_at } => {
            require_future(expires_at, now)?;
            if !owns {
                return Err(control_error(AdminStoreErrorKind::Conflict));
            }
            sqlx::query(
                "update client_key_budget_windows set weekly_start = $2, weekly_end = $3,
                weekly_used_usd = 0 where client_api_key_id = $1",
            )
            .bind(command.id.as_str())
            .bind(now)
            .bind(expires_at)
            .execute(&mut *tx)
            .await
            .map_err(control_unavailable)?;
        }
        ClientKeyWeeklyWindowAction::Align { expires_at } => {
            require_future(expires_at, now)?;
            if !owns {
                return Err(control_error(AdminStoreErrorKind::Conflict));
            }
            sqlx::query(
                "update client_key_budget_windows set weekly_end = $2
                where client_api_key_id = $1",
            )
            .bind(command.id.as_str())
            .bind(expires_at)
            .execute(&mut *tx)
            .await
            .map_err(control_unavailable)?;
        }
        ClientKeyWeeklyWindowAction::Release => {
            if !owns {
                return Err(control_error(AdminStoreErrorKind::Conflict));
            }
            release_key(&mut tx, command.id.as_str()).await?;
        }
    }
    sqlx::query(
        "update client_key_budget_windows set weekly_control_revision = $2,
        weekly_last_operation = $3 where client_api_key_id = $1",
    )
    .bind(command.id.as_str())
    .bind(next)
    .bind(operation)
    .execute(&mut *tx)
    .await
    .map_err(control_unavailable)?;
    super::append_admin_audit_event_in_transaction(
        &mut tx,
        mutation_audit(
            context,
            MutationAuditOperation::ClientApiKeyWeeklyControl,
            command.id.as_str(),
            vec!["weekly_window".to_owned()],
        ),
        None,
    )
    .await
    .map_err(|error| crate::admin_store_error("client API key weekly window", error))?;
    let row = sqlx::query(
        "select weekly_control_revision as revision, weekly_controller, weekly_start, weekly_end
        from client_key_budget_windows where client_api_key_id = $1",
    )
    .bind(command.id.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(control_unavailable)?;
    let result = control_from_row(&row, now)?;
    tx.commit().await.map_err(control_unavailable)?;
    Ok(result)
}

fn require_future(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> AdminStoreResult<()> {
    if expires_at <= now {
        return Err(control_error(AdminStoreErrorKind::Invalid));
    }
    Ok(())
}

/// 只撤销接管者并推进版本；窗口保持原状，由原生规则在到期后滚动。
async fn release_key(tx: &mut Transaction<'_, Postgres>, key: &str) -> AdminStoreResult<()> {
    sqlx::query(
        "update client_key_budget_windows set weekly_controller = null,
        weekly_control_revision = weekly_control_revision + 1, weekly_last_operation = null
        where client_api_key_id = $1",
    )
    .bind(key)
    .execute(&mut **tx)
    .await
    .map_err(control_unavailable)?;
    Ok(())
}

/// 实例真正停用、被替换或删除时，在同一配置事务内释放它接管的全部窗口。
pub(super) async fn release_controlled_windows(
    tx: &mut Transaction<'_, Postgres>,
    controller: &str,
    context: &MutationContext,
) -> AdminStoreResult<()> {
    // 配置事务已持有配置行锁；再按稳定顺序锁 Key，与插件写入的锁顺序一致。
    let keys = sqlx::query_scalar::<_, String>(
        "select k.id from client_api_keys k
        join client_key_budget_windows w on w.client_api_key_id = k.id
        where w.weekly_controller = $1 order by k.id for update of k",
    )
    .bind(controller)
    .fetch_all(&mut **tx)
    .await
    .map_err(control_unavailable)?;
    for key in keys {
        release_key(tx, &key).await?;
        super::append_admin_audit_event_in_transaction(
            tx,
            mutation_audit(
                context,
                MutationAuditOperation::ClientApiKeyWeeklyControl,
                &key,
                vec!["weekly_window".to_owned()],
            ),
            None,
        )
        .await
        .map_err(|error| crate::admin_store_error("client API key weekly window", error))?;
    }
    Ok(())
}

pub struct PgClientBudgetStore {
    pool: PgPool,
    retry: Mutex<BTreeMap<String, ClientBudgetCharge>>,
}

impl PgClientBudgetStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            retry: Mutex::new(BTreeMap::new()),
        }
    }

    async fn admit_inner(&self, key_id: ClientApiKeyId) -> Result<(), GatewayError> {
        // 短暂存储故障后按原金额重试；进程退出丢失的费用不转成人工核账或阻断 Key。
        let retries = self
            .retry
            .lock()
            .map_err(|_| unavailable())?
            .values()
            .filter(|charge| charge.key_id == key_id)
            .cloned()
            .collect::<Vec<_>>();
        for charge in retries {
            self.settle(charge).await.map_err(|_| unavailable())?;
        }
        let mut tx = self.pool.begin().await.map_err(|_| unavailable())?;
        let row = sqlx::query(
            "select daily_limit_usd::text, weekly_limit_usd::text, enabled
            from client_api_keys where id = $1 for update",
        )
        .bind(key_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(|| {
            GatewayError::new(
                GatewayErrorKind::Unauthorized,
                "client API key no longer exists",
            )
        })?;
        if !row.get::<bool, _>("enabled") {
            return Err(GatewayError::new(
                GatewayErrorKind::PolicyDenied,
                "client API key is disabled",
            ));
        }
        let limits = ClientBudgetLimits {
            daily_usd: row
                .get::<String, _>("daily_limit_usd")
                .parse()
                .map_err(|_| unavailable())?,
            weekly_usd: row
                .get::<String, _>("weekly_limit_usd")
                .parse()
                .map_err(|_| unavailable())?,
        };
        let now = Utc::now();
        advance_windows(&mut tx, key_id.as_str(), now)
            .await
            .map_err(|_| unavailable())?;
        // 受控窗口到期后不自动滚动，须等待接管者同步；与是否设置金额上限无关。
        let waiting: bool = sqlx::query_scalar(
            "select weekly_controller is not null and weekly_end <= $2
            from client_key_budget_windows where client_api_key_id = $1",
        )
        .bind(key_id.as_str())
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| unavailable())?;
        if waiting {
            return Err(GatewayError::new(
                GatewayErrorKind::RateLimited,
                "client API key weekly window is waiting for its controller",
            )
            .with_client_code("key_weekly_window_waiting"));
        }
        if limits.is_limited() {
            let window = sqlx::query(
                "select daily_used_usd::text, weekly_used_usd::text, daily_end, weekly_end
                from client_key_budget_windows where client_api_key_id = $1",
            )
            .bind(key_id.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| unavailable())?;
            let daily: Decimal = window
                .get::<String, _>("daily_used_usd")
                .parse()
                .map_err(|_| unavailable())?;
            let weekly: Decimal = window
                .get::<String, _>("weekly_used_usd")
                .parse()
                .map_err(|_| unavailable())?;
            let daily_exceeded = limits.daily_usd != Decimal::ZERO && daily >= limits.daily_usd;
            let weekly_exceeded = limits.weekly_usd != Decimal::ZERO && weekly >= limits.weekly_usd;
            if daily_exceeded || weekly_exceeded {
                let daily_end: DateTime<Utc> = window.get("daily_end");
                let weekly_end: DateTime<Utc> = window.get("weekly_end");
                let reset = if weekly_exceeded {
                    weekly_end
                } else {
                    daily_end
                };
                let retry = (reset - now).to_std().unwrap_or(Duration::from_secs(1));
                return Err(GatewayError::new(
                    GatewayErrorKind::RateLimited,
                    "client API key budget is exhausted",
                )
                .with_client_code(if weekly_exceeded {
                    "key_weekly_budget_exceeded"
                } else {
                    "key_daily_budget_exceeded"
                })
                .with_retry_after(retry));
            }
        }
        tx.commit().await.map_err(|_| unavailable())
    }

    async fn settle_inner(&self, charge: &ClientBudgetCharge) -> Result<(), ClientBudgetError> {
        let mut tx = self.pool.begin().await.map_err(|_| ClientBudgetError)?;
        // 与准入统一先锁 Key，再写窗口和费用，串行化同一 Key 的并发结算。
        let key = sqlx::query_scalar::<_, String>(
            "select id from client_api_keys where id = $1 for update",
        )
        .bind(charge.key_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ClientBudgetError)?;
        let Some(key) = key else { return Ok(()) }; // 删除 Key 时也会删除其费用记录。
        settle_in_transaction(&mut tx, &key, charge)
            .await
            .map_err(|_| ClientBudgetError)?;
        tx.commit().await.map_err(|_| ClientBudgetError)
    }
}

async fn settle_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    charge: &ClientBudgetCharge,
) -> Result<(), sqlx::Error> {
    advance_windows(tx, key, Utc::now()).await?;
    // 仅在请求结束时写入费用；请求 ID 冲突时不重复累计。
    let changed = sqlx::query(
        "insert into client_key_charge_events (request_id, client_api_key_id, amount_usd, completed_at)
            values ($1, $2, $3::text::numeric, $4)
            on conflict (request_id) do nothing",
    )
    .bind(charge.request_id.as_str())
    .bind(key)
    .bind(charge.amount_usd.canonical())
    .bind(DateTime::<Utc>::from(charge.completed_at))
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed == 1 {
        sqlx::query("update client_key_budget_windows set
                daily_used_usd = daily_used_usd + case when $3 >= daily_start and $3 < daily_end then $2::text::numeric else 0 end,
                weekly_used_usd = weekly_used_usd + case when $3 >= weekly_start and ($3 < weekly_end or weekly_controller is not null) then $2::text::numeric else 0 end
                where client_api_key_id = $1")
                .bind(key).bind(charge.amount_usd.canonical()).bind(DateTime::<Utc>::from(charge.completed_at))
                .execute(&mut **tx).await?;
    }
    Ok(())
}

impl ClientBudgetPort for PgClientBudgetStore {
    fn admit(&self, key_id: ClientApiKeyId) -> BoxFuture<'_, Result<(), GatewayError>> {
        Box::pin(async move { self.admit_inner(key_id).await })
    }

    fn settle(&self, charge: ClientBudgetCharge) -> BoxFuture<'_, Result<(), ClientBudgetError>> {
        Box::pin(async move {
            let result = self.settle_inner(&charge).await;
            let mut retry = self.retry.lock().map_err(|_| ClientBudgetError)?;
            if result.is_err() {
                retry.insert(charge.request_id.as_str().to_owned(), charge);
            } else {
                retry.remove(charge.request_id.as_str());
            }
            result
        })
    }
}

async fn advance_windows(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query("insert into client_key_budget_windows
        (client_api_key_id, daily_start, daily_end, weekly_start, weekly_end)
        select $1, day, day + interval '24 hours', day, day + interval '168 hours'
        from (select date_trunc('day', $2::timestamptz at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai' as day) d
        on conflict (client_api_key_id) do update set
            daily_start = case when client_key_budget_windows.daily_end <= $2 then excluded.daily_start else client_key_budget_windows.daily_start end,
            daily_end = case when client_key_budget_windows.daily_end <= $2 then excluded.daily_end else client_key_budget_windows.daily_end end,
            daily_used_usd = case when client_key_budget_windows.daily_end <= $2 then 0 else client_key_budget_windows.daily_used_usd end,
            weekly_start = case when client_key_budget_windows.weekly_controller is null and client_key_budget_windows.weekly_end <= $2 then excluded.weekly_start else client_key_budget_windows.weekly_start end,
            weekly_end = case when client_key_budget_windows.weekly_controller is null and client_key_budget_windows.weekly_end <= $2 then excluded.weekly_end else client_key_budget_windows.weekly_end end,
            weekly_used_usd = case when client_key_budget_windows.weekly_controller is null and client_key_budget_windows.weekly_end <= $2 then 0 else client_key_budget_windows.weekly_used_usd end")
        .bind(key).bind(now).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn load_client_key_budgets(
    pool: &PgPool,
    records: &mut [super::ClientApiKeyRecord],
) -> StoreResult<()> {
    if records.is_empty() {
        return Ok(());
    }
    let ids = records
        .iter()
        .map(|record| record.id.as_str())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "select k.id, k.daily_limit_usd::text, k.weekly_limit_usd::text,
        (case when w.daily_end > now() then w.daily_used_usd else 0 end)::text as daily_used,
        (case when w.weekly_controller is not null or w.weekly_end > now() then w.weekly_used_usd else 0 end)::text as weekly_used,
        case when w.daily_end > now() then w.daily_end end as daily_end,
        case when w.weekly_controller is not null or w.weekly_end > now() then w.weekly_end end as weekly_end
        from client_api_keys k left join client_key_budget_windows w on w.client_api_key_id = k.id
        where k.id = any($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(|_| postgres_unavailable("load client budgets"))?;
    let mut budgets = BTreeMap::new();
    for row in rows {
        let parse = |field| -> StoreResult<Decimal> {
            row.get::<String, _>(field)
                .parse()
                .map_err(|_| postgres_unavailable("decode client budget"))
        };
        budgets.insert(
            row.get::<String, _>("id"),
            ClientBudgetStatus {
                limits: ClientBudgetLimits {
                    daily_usd: parse("daily_limit_usd")?,
                    weekly_usd: parse("weekly_limit_usd")?,
                },
                daily_used_usd: parse("daily_used")?,
                weekly_used_usd: parse("weekly_used")?,
                daily_resets_at: row
                    .get::<Option<DateTime<Utc>>, _>("daily_end")
                    .map(Into::into),
                weekly_resets_at: row
                    .get::<Option<DateTime<Utc>>, _>("weekly_end")
                    .map(Into::into),
            },
        );
    }
    for record in records {
        record.budget = budgets
            .remove(&record.id)
            .ok_or_else(|| postgres_unavailable("load client budget policy"))?;
    }
    Ok(())
}

fn unavailable() -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::ProviderInfrastructureUnavailable,
        "client budget service is temporarily unavailable",
    )
    .with_client_code("key_budget_unavailable")
}
