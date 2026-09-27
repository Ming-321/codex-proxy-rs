//! 接管变更与计费共用 Key 行锁；版本和操作内容与窗口一起提交。

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationContext,
        client_keys::ClientKeyBudgetMutationOrigin,
        weekly_budget::{ChangeWeeklyBudget, WeeklyBudgetAction, WeeklyBudgetControl},
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
};
use gateway_core::policy::ClientApiKeyId;
use sqlx::{PgPool, Postgres, Row, Transaction};

fn error(kind: AdminStoreErrorKind) -> AdminStoreError {
    AdminStoreError::new(
        kind,
        "weekly budget control",
        "weekly budget control operation failed",
    )
}

fn unavailable(_: sqlx::Error) -> AdminStoreError {
    error(AdminStoreErrorKind::Unavailable)
}

pub(super) async fn get(
    pool: &PgPool,
    id: &ClientApiKeyId,
) -> AdminStoreResult<WeeklyBudgetControl> {
    let row = sqlx::query("select coalesce(w.weekly_control_revision,0) as revision, w.weekly_controller, w.weekly_end from client_api_keys k left join client_key_budget_windows w on w.client_api_key_id=k.id where k.id=$1")
        .bind(id.as_str()).fetch_optional(pool).await.map_err(unavailable)?
        .ok_or_else(|| error(AdminStoreErrorKind::NotFound))?;
    let controller: Option<String> = row.get("weekly_controller");
    let expires_at: Option<DateTime<Utc>> = row.get("weekly_end");
    Ok(WeeklyBudgetControl {
        revision: u64::try_from(row.get::<i64, _>("revision"))
            .map_err(|_| error(AdminStoreErrorKind::Unavailable))?,
        waiting: controller.is_some() && expires_at.is_some_and(|end| end <= Utc::now()),
        controller,
        expires_at,
    })
}

pub(super) async fn change(
    pool: &PgPool,
    command: ChangeWeeklyBudget,
    origin: ClientKeyBudgetMutationOrigin,
    context: &MutationContext,
) -> AdminStoreResult<WeeklyBudgetControl> {
    let owner = match &origin {
        ClientKeyBudgetMutationOrigin::Plugin(owner) => Some(owner.instance_id.as_str()),
        ClientKeyBudgetMutationOrigin::Admin => None,
    };
    // 管理员只能解除接管；业务窗口由获授权的插件决定。
    if owner.is_none() && command.action != WeeklyBudgetAction::Release {
        return Err(error(AdminStoreErrorKind::Invalid));
    }
    let mut tx = match &origin {
        ClientKeyBudgetMutationOrigin::Plugin(owner) => {
            super::plugins::begin_authorized_mutation(pool, owner, "key_budgets").await?
        }
        ClientKeyBudgetMutationOrigin::Admin => pool.begin().await.map_err(unavailable)?,
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
    super::client_budgets::advance_windows(&mut tx, command.id.as_str(), now)
        .await
        .map_err(unavailable)?;
    let row = sqlx::query("select weekly_control_revision,weekly_controller,weekly_last_operation,weekly_end from client_key_budget_windows where client_api_key_id=$1")
        .bind(command.id.as_str()).fetch_one(&mut *tx).await.map_err(unavailable)?;
    let revision: i64 = row.get("weekly_control_revision");
    let controller: Option<String> = row.get("weekly_controller");
    let expected = i64::try_from(command.expected_revision)
        .map_err(|_| error(AdminStoreErrorKind::Invalid))?;
    let next = expected
        .checked_add(1)
        .ok_or_else(|| error(AdminStoreErrorKind::Invalid))?;
    let operation =
        serde_json::json!({"owner":owner,"expected_revision":expected,"operation":command.action});
    let previous: Option<serde_json::Value> = row.get("weekly_last_operation");
    if revision == next && previous.as_ref() == Some(&operation) {
        // 返回原子提交的版本；重试不触碰计费起点或已用金额。
        return Ok(WeeklyBudgetControl {
            revision: next as u64,
            waiting: controller.is_some() && row.get::<DateTime<Utc>, _>("weekly_end") <= now,
            controller,
            expires_at: Some(row.get("weekly_end")),
        });
    }
    if revision != expected {
        return Err(error(AdminStoreErrorKind::StaleRevision));
    }
    let expires_at = match command.action {
        WeeklyBudgetAction::Claim { expires_at, .. } | WeeklyBudgetAction::Sync { expires_at } => {
            Some(expires_at)
        }
        WeeklyBudgetAction::Release => None,
    };
    if expires_at.is_some_and(|end| end <= now) {
        return Err(error(AdminStoreErrorKind::Invalid));
    }
    match command.action {
        WeeklyBudgetAction::Claim { clear_used, .. } => {
            if controller.is_some() {
                return Err(error(AdminStoreErrorKind::Conflict));
            }
            sqlx::query("update client_key_budget_windows set weekly_controller=$2, weekly_end=$3, weekly_used_usd=case when $4 then 0 else weekly_used_usd end, weekly_start=case when $4 then $5 else weekly_start end where client_api_key_id=$1")
                .bind(command.id.as_str()).bind(owner).bind(expires_at).bind(clear_used).bind(now)
                .execute(&mut *tx).await.map_err(unavailable)?;
        }
        WeeklyBudgetAction::Sync { .. } => {
            if controller.as_deref() != owner || controller.is_none() {
                return Err(error(AdminStoreErrorKind::Conflict));
            }
            sqlx::query("update client_key_budget_windows set weekly_start=$2, weekly_end=$3, weekly_used_usd=0 where client_api_key_id=$1")
                .bind(command.id.as_str()).bind(now).bind(expires_at).execute(&mut *tx).await.map_err(unavailable)?;
        }
        WeeklyBudgetAction::Release => {
            if controller.is_none()
                || owner.is_some_and(|owner| controller.as_deref() != Some(owner))
            {
                return Err(error(AdminStoreErrorKind::Conflict));
            }
            release_key(&mut tx, command.id.as_str(), now).await?;
        }
    }
    sqlx::query("update client_key_budget_windows set weekly_control_revision=$2, weekly_last_operation=$3 where client_api_key_id=$1")
        .bind(command.id.as_str()).bind(next).bind(operation).execute(&mut *tx).await.map_err(unavailable)?;
    super::append_admin_audit_event_in_transaction(
        &mut tx,
        crate::mutation_audit(
            context,
            "weekly_control",
            "client_api_key",
            command.id.as_str(),
            vec!["weekly_window".into()],
        ),
        None,
    )
    .await
    .map_err(|e| crate::admin_store_error("weekly budget control", e))?;
    let row = sqlx::query("select weekly_controller,weekly_end from client_key_budget_windows where client_api_key_id=$1")
        .bind(command.id.as_str()).fetch_one(&mut *tx).await.map_err(unavailable)?;
    let result = WeeklyBudgetControl {
        revision: next as u64,
        controller: row.get("weekly_controller"),
        expires_at: Some(row.get("weekly_end")),
        waiting: false,
    };
    tx.commit().await.map_err(unavailable)?;
    Ok(result)
}

async fn release_key(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    now: DateTime<Utc>,
) -> AdminStoreResult<()> {
    // 保留计费起点和已用金额，避免丢失在途结算；仅近似恢复原生到期日。
    sqlx::query("update client_key_budget_windows set weekly_controller=null,weekly_end=(date_trunc('day',$2::timestamptz at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai') + interval '168 hours',weekly_control_revision=weekly_control_revision+1,weekly_last_operation=null where client_api_key_id=$1")
        .bind(key).bind(now).execute(&mut **tx).await.map_err(unavailable)?;
    Ok(())
}

pub(super) async fn release_owner(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
) -> AdminStoreResult<()> {
    // 配置事务先锁配置行，再按稳定顺序锁 Key，与插件写入保持同一锁顺序。
    let keys = sqlx::query_scalar::<_,String>("select k.id from client_api_keys k join client_key_budget_windows w on w.client_api_key_id=k.id where w.weekly_controller=$1 order by k.id for update of k")
        .bind(owner).fetch_all(&mut **tx).await.map_err(unavailable)?;
    let now = Utc::now();
    for key in keys {
        release_key(tx, &key, now).await?;
    }
    Ok(())
}
