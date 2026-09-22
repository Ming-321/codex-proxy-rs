//! 按 Key 串行检查限额，并幂等累计已取得的 USD 费用。

use std::{collections::BTreeMap, sync::Mutex, time::Duration};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use gateway_admin::model::{
    MutationContext,
    client_keys::{ClientKeyBudgetPeriod, ResetClientKeyBudget},
};
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
    context: &MutationContext,
) -> StoreResult<()> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| postgres_unavailable("begin budget reset"))?;
    // 与准入、结算共用 Key 行锁，重置边界必须在取得锁之后确定。
    let exists =
        sqlx::query_scalar::<_, String>("select id from client_api_keys where id = $1 for update")
            .bind(command.id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| postgres_unavailable("lock budget reset key"))?;
    if exists.is_none() {
        return Err(StoreError::NotFound {
            entity: "client API key",
            id: command.id.as_str().to_owned(),
        });
    }
    let seat: Option<String> =
        sqlx::query_scalar("select seat_id from client_api_keys where id = $1")
            .bind(command.id.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| postgres_unavailable("read budget owner"))?;
    if seat.is_some() {
        return Err(crate::StoreError::InvalidData {
            entity: "client API key",
            message: "seat Key 不能独立重置共享费用".to_owned(),
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
        weekly_start = case when $3 and weekly_end > $4 then $4 else weekly_start end
        where client_api_key_id = $1",
    )
    .bind(command.id.as_str())
    .bind(daily)
    .bind(weekly)
    .bind(reset_at)
    .execute(&mut *tx)
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
        &mut tx,
        mutation_audit(
            context,
            "reset_budget",
            "client_api_key",
            command.id.as_str(),
            fields,
        ),
        None,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|_| postgres_unavailable("commit budget reset"))
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

    async fn admit_inner(
        &self,
        key_id: ClientApiKeyId,
        seat_id: Option<gateway_core::policy::SeatId>,
        request: Option<(gateway_core::engine::ModelRequestId, std::time::SystemTime)>,
    ) -> Result<(), GatewayError> {
        // 短暂存储故障后按原金额重试；进程退出丢失的费用不转成人工核账或阻断 Key。
        let retries = self
            .retry
            .lock()
            .map_err(|_| unavailable())?
            .values()
            .filter(|charge| {
                charge.key_id == key_id || (seat_id.is_some() && charge.seat_id == seat_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for charge in retries {
            self.settle(charge).await.map_err(|_| unavailable())?;
        }
        let mut tx = self.pool.begin().await.map_err(|_| unavailable())?;
        let mut row = sqlx::query(
            "select daily_limit_usd::text, weekly_limit_usd::text, enabled, seat_id
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
        if row.get::<Option<String>, _>("seat_id").as_deref()
            != seat_id.as_ref().map(gateway_core::policy::SeatId::as_str)
        {
            return Err(GatewayError::new(
                GatewayErrorKind::PolicyDenied,
                "client key membership changed; retry with current configuration",
            ));
        }
        let owner = match seat_id.as_ref() {
            Some(id) => {
                row = sqlx::query("select s.daily_limit_usd::text, s.weekly_limit_usd::text, (s.enabled and g.enabled) as enabled from seats s join account_groups g on g.id = s.account_group_id where s.id = $1 for update of s")
                    .bind(id.as_str()).fetch_one(&mut *tx).await.map_err(|_| unavailable())?;
                if !row.get::<bool, _>("enabled") {
                    return Err(GatewayError::new(
                        GatewayErrorKind::PolicyDenied,
                        "seat or car is disabled",
                    ));
                }
                BudgetOwner::Seat(id.as_str())
            }
            None => BudgetOwner::Key(key_id.as_str()),
        };
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
        advance_owner_windows(&mut tx, owner, now)
            .await
            .map_err(|_| unavailable())?;
        if limits.is_limited() {
            // 表名和列名只来自封闭的 BudgetOwner，所有外部值仍使用绑定参数。
            let window = sqlx::query(sqlx::AssertSqlSafe(format!(
                "select daily_used_usd::text, weekly_used_usd::text, daily_end, weekly_end from {} where {} = $1", owner.table(), owner.column()
            )))
            .bind(owner.id())
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
        if let Some((id, deadline)) = request {
            sqlx::query("insert into client_budget_admissions (request_id, client_api_key_id, expires_at) values ($1, $2, $3) on conflict (request_id) do nothing")
                .bind(id.as_str()).bind(key_id.as_str()).bind(DateTime::<Utc>::from(deadline) + chrono::Duration::minutes(1))
                .execute(&mut *tx).await.map_err(|_| unavailable())?;
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
        sqlx::query("delete from client_budget_admissions where request_id = $1")
            .bind(charge.request_id.as_str())
            .execute(&mut *tx)
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
    if let Some(id) = &charge.seat_id {
        sqlx::query("select id from seats where id = $1 for update")
            .bind(id.as_str())
            .fetch_one(&mut **tx)
            .await?;
        advance_owner_windows(tx, BudgetOwner::Seat(id.as_str()), Utc::now()).await?;
    }
    // 仅在请求结束时写入费用；请求 ID 冲突时不重复累计。
    let changed = sqlx::query(
        "insert into client_key_charge_events (request_id, client_api_key_id, amount_usd, completed_at, seat_id)
            values ($1, $2, $3::text::numeric, $4, $5)
            on conflict (request_id) do nothing",
    )
    .bind(charge.request_id.as_str())
    .bind(key)
    .bind(charge.amount_usd.canonical())
    .bind(DateTime::<Utc>::from(charge.completed_at))
    .bind(charge.seat_id.as_ref().map(gateway_core::policy::SeatId::as_str))
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed == 1 {
        sqlx::query("update client_key_budget_windows set
                daily_used_usd = daily_used_usd + case when $3 >= daily_start and $3 < daily_end then $2::text::numeric else 0 end,
                weekly_used_usd = weekly_used_usd + case when $3 >= weekly_start and $3 < weekly_end then $2::text::numeric else 0 end
                where client_api_key_id = $1")
                .bind(key).bind(charge.amount_usd.canonical()).bind(DateTime::<Utc>::from(charge.completed_at))
                .execute(&mut **tx).await?;
        if let Some(id) = &charge.seat_id {
            sqlx::query("update seat_budget_windows set
                daily_used_usd = daily_used_usd + case when $3 >= daily_start and $3 < daily_end then $2::text::numeric else 0 end,
                weekly_used_usd = weekly_used_usd + case when $3 >= weekly_start and $3 < weekly_end then $2::text::numeric else 0 end
                where seat_id = $1")
                .bind(id.as_str()).bind(charge.amount_usd.canonical()).bind(DateTime::<Utc>::from(charge.completed_at))
                .execute(&mut **tx).await?;
        }
    }
    Ok(())
}

impl ClientBudgetPort for PgClientBudgetStore {
    fn admit(
        &self,
        key_id: ClientApiKeyId,
        seat_id: Option<gateway_core::policy::SeatId>,
    ) -> BoxFuture<'_, Result<(), GatewayError>> {
        Box::pin(async move { self.admit_inner(key_id, seat_id, None).await })
    }

    fn begin_request(
        &self,
        key_id: ClientApiKeyId,
        seat_id: Option<gateway_core::policy::SeatId>,
        request_id: gateway_core::engine::ModelRequestId,
        deadline_at: std::time::SystemTime,
    ) -> BoxFuture<'_, Result<(), GatewayError>> {
        Box::pin(async move {
            self.admit_inner(key_id, seat_id, Some((request_id, deadline_at)))
                .await
        })
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
    advance_owner_windows(tx, BudgetOwner::Key(key), now).await
}

#[derive(Clone, Copy)]
pub(super) enum BudgetOwner<'a> {
    Key(&'a str),
    Seat(&'a str),
}

impl<'a> BudgetOwner<'a> {
    fn id(self) -> &'a str {
        match self {
            Self::Key(id) | Self::Seat(id) => id,
        }
    }
    fn table(self) -> &'static str {
        match self {
            Self::Key(_) => "client_key_budget_windows",
            Self::Seat(_) => "seat_budget_windows",
        }
    }
    fn column(self) -> &'static str {
        match self {
            Self::Key(_) => "client_api_key_id",
            Self::Seat(_) => "seat_id",
        }
    }
}

pub(super) async fn advance_owner_windows(
    tx: &mut Transaction<'_, Postgres>,
    owner: BudgetOwner<'_>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let table = owner.table();
    let column = owner.column();
    // 标识符来自上方枚举，不能由请求指定。
    sqlx::query(sqlx::AssertSqlSafe(format!("insert into {table}
        ({column}, daily_start, daily_end, weekly_start, weekly_end)
        select $1, day, day + interval '24 hours', day, day + interval '168 hours'
        from (select date_trunc('day', $2::timestamptz at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai' as day) d
        on conflict ({column}) do update set
            daily_start = case when {table}.daily_end <= $2 then excluded.daily_start else {table}.daily_start end,
            daily_end = case when {table}.daily_end <= $2 then excluded.daily_end else {table}.daily_end end,
            daily_used_usd = case when {table}.daily_end <= $2 then 0 else {table}.daily_used_usd end,
            weekly_start = case when {table}.weekly_end <= $2 then excluded.weekly_start else {table}.weekly_start end,
            weekly_end = case when {table}.weekly_end <= $2 then excluded.weekly_end else {table}.weekly_end end,
            weekly_used_usd = case when {table}.weekly_end <= $2 then 0 else {table}.weekly_used_usd end")))
        .bind(owner.id()).bind(now).execute(&mut **tx).await?;
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
        "select k.id, coalesce(s.daily_limit_usd, k.daily_limit_usd)::text as daily_limit_usd,
        coalesce(s.weekly_limit_usd, k.weekly_limit_usd)::text as weekly_limit_usd,
        s.id as seat_id, s.name as seat_name, s.max_concurrency as seat_concurrency,
        (case when w.daily_end > now() then w.daily_used_usd else 0 end)::text as daily_used,
        (case when w.weekly_end > now() then w.weekly_used_usd else 0 end)::text as weekly_used,
        case when w.daily_end > now() then w.daily_end end as daily_end,
        case when w.weekly_end > now() then w.weekly_end end as weekly_end
        from client_api_keys k
        left join seats s on s.id = k.seat_id
        left join lateral (
            select daily_used_usd, weekly_used_usd, daily_end, weekly_end
            from client_key_budget_windows where client_api_key_id = k.id and k.seat_id is null
            union all
            select daily_used_usd, weekly_used_usd, daily_end, weekly_end
            from seat_budget_windows where seat_id = k.seat_id
        ) w on true
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
                seat: row
                    .get::<Option<String>, _>("seat_id")
                    .map(|id| {
                        Ok::<_, crate::StoreError>(gateway_core::engine::budget::SeatBudgetRef {
                            id: gateway_core::policy::SeatId::new(id)
                                .map_err(|_| postgres_unavailable("decode seat ID"))?,
                            name: row.get("seat_name"),
                            max_concurrency: u64::try_from(row.get::<i64, _>("seat_concurrency"))
                                .map_err(|_| {
                                postgres_unavailable("decode seat concurrency")
                            })?,
                        })
                    })
                    .transpose()?,
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
