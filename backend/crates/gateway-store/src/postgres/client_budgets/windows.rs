//! 原生窗口的查询与原子配置；调用方身份只用于写入授权和重试隔离。

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationContext,
        audit::MutationAuditOperation,
        client_keys::{
            ChangeClientKeyBudgetWindow, ClientKeyBudgetMutationOrigin, ClientKeyBudgetWindow,
            ClientKeyBudgetWindowMode, ClientKeyBudgetWindowPeriod, ClientKeyBudgetWindowUpdate,
        },
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
};
use gateway_core::policy::ClientApiKeyId;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};

use crate::{admin_store_error, mutation_audit};

fn error(kind: AdminStoreErrorKind) -> AdminStoreError {
    AdminStoreError::new(
        kind,
        "client API key budget window",
        "budget window operation failed",
    )
}

fn unavailable(_: sqlx::Error) -> AdminStoreError {
    error(AdminStoreErrorKind::Unavailable)
}

fn view(row: &PgRow, now: DateTime<Utc>) -> AdminStoreResult<ClientKeyBudgetWindow> {
    let fixed: bool = row.get("fixed");
    let expires_at: Option<DateTime<Utc>> = row.get("expires_at");
    let active = fixed || expires_at.is_some_and(|time| time > now);
    Ok(ClientKeyBudgetWindow {
        revision: u64::try_from(row.get::<i64, _>("revision"))
            .map_err(|_| error(AdminStoreErrorKind::Unavailable))?,
        mode: if fixed {
            ClientKeyBudgetWindowMode::Fixed
        } else {
            ClientKeyBudgetWindowMode::Automatic
        },
        accounting_start: if active {
            row.get("accounting_start")
        } else {
            None
        },
        expires_at: expires_at.filter(|_| active),
    })
}

pub(in super::super) async fn budget_window(
    pool: &PgPool,
    id: &ClientApiKeyId,
    period: ClientKeyBudgetWindowPeriod,
) -> AdminStoreResult<ClientKeyBudgetWindow> {
    let row = sqlx::query(
        "select coalesce(case when $2 then w.daily_window_revision else w.weekly_window_revision end, 0) as revision,
        coalesce(case when $2 then w.daily_fixed else w.weekly_fixed end, false) as fixed,
        case when $2 then w.daily_start else w.weekly_start end as accounting_start,
        case when $2 then w.daily_end else w.weekly_end end as expires_at
        from client_api_keys k left join client_key_budget_windows w on w.client_api_key_id=k.id where k.id=$1",
    ).bind(id.as_str()).bind(period == ClientKeyBudgetWindowPeriod::Daily)
        .fetch_optional(pool).await.map_err(unavailable)?
        .ok_or_else(|| error(AdminStoreErrorKind::NotFound))?;
    view(&row, Utc::now())
}

pub(super) async fn ensure_windows(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
) -> Result<(), sqlx::Error> {
    // 未使用的 Key 也需要保存版本；未选周期以 Unix 纪元为关闭窗口的起点，避免丢弃另一周期的迟到结算。
    sqlx::query(
        "insert into client_key_budget_windows
        (client_api_key_id, daily_start, daily_end, weekly_start, weekly_end)
        values ($1,to_timestamp(0),to_timestamp(0),to_timestamp(0),to_timestamp(0))
        on conflict (client_api_key_id) do nothing",
    )
    .bind(key)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(in super::super) async fn change_budget_window(
    pool: &PgPool,
    command: ChangeClientKeyBudgetWindow,
    origin: ClientKeyBudgetMutationOrigin,
    context: &MutationContext,
) -> AdminStoreResult<ClientKeyBudgetWindow> {
    let mut tx = match &origin {
        ClientKeyBudgetMutationOrigin::Admin => pool.begin().await.map_err(unavailable)?,
        ClientKeyBudgetMutationOrigin::Plugin(owner) => {
            super::super::plugins::begin_plugin_mutation(pool, owner).await?
        }
    };
    let exists =
        sqlx::query_scalar::<_, String>("select id from client_api_keys where id=$1 for update")
            .bind(command.id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
    if exists.is_none() {
        return Err(error(AdminStoreErrorKind::NotFound));
    }
    let now = Utc::now();
    ensure_windows(&mut tx, command.id.as_str())
        .await
        .map_err(unavailable)?;
    let daily = command.period == ClientKeyBudgetWindowPeriod::Daily;
    let row = sqlx::query("select
        case when $2 then daily_window_revision else weekly_window_revision end as revision,
        case when $2 then daily_fixed else weekly_fixed end as fixed,
        case when $2 then daily_start else weekly_start end as accounting_start,
        case when $2 then daily_end else weekly_end end as expires_at,
        case when $2 then daily_window_last_operation else weekly_window_last_operation end as last_operation
        from client_key_budget_windows where client_api_key_id=$1")
        .bind(command.id.as_str()).bind(daily).fetch_one(&mut *tx).await.map_err(unavailable)?;
    let current = view(&row, now)?;
    let next = command
        .expected_revision
        .checked_add(1)
        .filter(|revision| i64::try_from(*revision).is_ok())
        .ok_or_else(|| error(AdminStoreErrorKind::Invalid))?;
    let caller = match &origin {
        ClientKeyBudgetMutationOrigin::Admin => {
            serde_json::json!({"kind":"admin","actor":context.actor})
        }
        ClientKeyBudgetMutationOrigin::Plugin(owner) => {
            serde_json::json!({"kind":"plugin","instance_id":owner.instance_id})
        }
    };
    let operation = serde_json::json!({"caller":caller,"expected_revision":command.expected_revision,"update":command.update});
    let previous: Option<serde_json::Value> = row.get("last_operation");
    if current.revision == next && previous.as_ref() == Some(&operation) {
        // 最近一次完整请求的重试返回已提交事实，不重复清零，也不重新校验已经过去的截止时间。
        tx.commit().await.map_err(unavailable)?;
        return Ok(current);
    }
    if current.revision != command.expected_revision {
        return Err(error(AdminStoreErrorKind::StaleRevision));
    }
    let (fixed, expiry, clear) = match command.update {
        ClientKeyBudgetWindowUpdate::Automatic => (false, None, false),
        ClientKeyBudgetWindowUpdate::Fixed {
            expires_at,
            clear_used,
        } => {
            if expires_at <= now {
                return Err(error(AdminStoreErrorKind::Invalid));
            }
            (true, Some(expires_at), clear_used)
        }
    };
    // 从已到期的自动窗口切换时沿用原生归属；固定窗口延长只改截止时间，绝不隐式清零。
    let row = sqlx::query("update client_key_budget_windows set
        daily_start = case when $2 and $3 then case when $5 then $6 when not daily_fixed and daily_end <= $6 then greatest(daily_start, date_trunc('day',$6::timestamptz at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai') else daily_start end else daily_start end,
        weekly_start = case when not $2 and $3 then case when $5 then $6 when not weekly_fixed and weekly_end <= $6 then greatest(weekly_start, date_trunc('day',$6::timestamptz at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai') else weekly_start end else weekly_start end,
        daily_used_usd = case when $2 and $3 and ($5 or (not daily_fixed and daily_end <= $6)) then 0 else daily_used_usd end,
        weekly_used_usd = case when not $2 and $3 and ($5 or (not weekly_fixed and weekly_end <= $6)) then 0 else weekly_used_usd end,
        daily_end = case when $2 then coalesce($4,daily_end) else daily_end end,
        weekly_end = case when not $2 then coalesce($4,weekly_end) else weekly_end end,
        daily_fixed = case when $2 then $3 else daily_fixed end,
        weekly_fixed = case when not $2 then $3 else weekly_fixed end,
        daily_window_revision = case when $2 then $7 else daily_window_revision end,
        weekly_window_revision = case when not $2 then $7 else weekly_window_revision end,
        daily_window_last_operation = case when $2 then $8 else daily_window_last_operation end,
        weekly_window_last_operation = case when not $2 then $8 else weekly_window_last_operation end
        where client_api_key_id=$1 returning
        case when $2 then daily_window_revision else weekly_window_revision end as revision,
        case when $2 then daily_fixed else weekly_fixed end as fixed,
        case when $2 then daily_start else weekly_start end as accounting_start,
        case when $2 then daily_end else weekly_end end as expires_at")
        .bind(command.id.as_str()).bind(daily).bind(fixed).bind(expiry).bind(clear).bind(now)
        .bind(i64::try_from(next).map_err(|_| error(AdminStoreErrorKind::Invalid))?).bind(operation)
        .fetch_one(&mut *tx).await.map_err(unavailable)?;
    super::super::append_admin_audit_event_in_transaction(
        &mut tx,
        mutation_audit(
            context,
            MutationAuditOperation::ClientApiKeyBudgetWindowChange,
            command.id.as_str(),
            vec![
                if daily {
                    "daily_window"
                } else {
                    "weekly_window"
                }
                .to_owned(),
            ],
        ),
        None,
    )
    .await
    .map_err(|error| admin_store_error("client API key budget window", error))?;
    let result = view(&row, now)?;
    tx.commit().await.map_err(unavailable)?;
    Ok(result)
}
