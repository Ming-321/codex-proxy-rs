//! car / seat 的原子配置与既有费用承接。

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationContext, Revision,
        account_groups::{JoinSeat, SaveSeat, SeatRecord},
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
};
use gateway_core::{
    engine::budget::{ClientBudgetLimits, ClientBudgetStatus},
    metering::Decimal,
    policy::SeatId,
    routing::AccountGroupId,
};
use sqlx::{PgPool, Postgres, Row, Transaction};

use super::client_budgets::{BudgetOwner, advance_owner_windows};

fn invalid(message: &str) -> AdminStoreError {
    AdminStoreError::new(AdminStoreErrorKind::Invalid, "seat", message)
}

fn database(error: sqlx::Error) -> AdminStoreError {
    if let Some(message) = capacity_error_message(&error) {
        return AdminStoreError::new(AdminStoreErrorKind::CarCapacity, "seat", message);
    }
    if let Some(db) = error.as_database_error()
        && (db.is_check_violation() || db.is_unique_violation() || db.is_foreign_key_violation())
    {
        return invalid("car / seat 配置冲突：检查账号独占、seat 数量、并发上限和名称");
    }
    AdminStoreError::new(
        AdminStoreErrorKind::Unavailable,
        "seat",
        "seat 存储暂不可用",
    )
}

fn capacity_error_message(error: &sqlx::Error) -> Option<&'static str> {
    match error.as_database_error()?.constraint()? {
        "car_finite_capacity" => Some(
            "car 账号必须有正数并发上限，请先为继承默认值的 car 账号设置独立上限，再将全局默认设为不限",
        ),
        "car_seat_capacity" => {
            Some("账号并发上限不能小于 seat 数量或任一 seat 上限，停用 seat 也计入数量")
        }
        _ => None,
    }
}

pub(super) fn configuration_error(
    error: sqlx::Error,
    operation: &'static str,
) -> crate::StoreError {
    if let Some(message) = capacity_error_message(&error) {
        return crate::StoreError::InvalidData {
            entity: "car capacity",
            message: message.to_owned(),
        };
    }
    crate::postgres_unavailable(operation)
}

async fn begin(pool: &PgPool) -> AdminStoreResult<(Transaction<'_, Postgres>, crate::Revision)> {
    let mut tx = pool.begin().await.map_err(database)?;
    let revision = super::bump_config_revision_in_transaction(&mut tx)
        .await
        .map_err(|e| crate::admin_store_error("seat", e))?;
    Ok((tx, revision))
}

async fn commit(
    mut tx: Transaction<'_, Postgres>,
    revision: crate::Revision,
    context: &MutationContext,
    action: &str,
    id: &str,
) -> AdminStoreResult<Revision> {
    super::append_admin_audit_event_in_transaction(
        &mut tx,
        crate::mutation_audit(context, action, "seat", id, vec!["seat".to_owned()]),
        revision,
    )
    .await
    .map_err(|e| crate::admin_store_error("seat", e))?;
    tx.commit().await.map_err(database)?;
    crate::admin_revision(revision)
}

pub(super) async fn convert_to_car(
    pool: &PgPool,
    id: AccountGroupId,
    context: &MutationContext,
) -> AdminStoreResult<Revision> {
    let (mut tx, revision) = begin(pool).await?;
    let result =
        sqlx::query("update account_groups set is_car = true, updated_at = now() where id = $1")
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(database)?;
    if result.rows_affected() != 1 {
        return Err(invalid("账号分组不存在"));
    }
    commit(tx, revision, context, "convert_car", id.as_str()).await
}

pub(super) async fn save_seat(
    pool: &PgPool,
    command: SaveSeat,
    context: &MutationContext,
) -> AdminStoreResult<Revision> {
    let id = command.id.ok_or_else(|| invalid("seat ID 缺失"))?;
    let (mut tx, revision) = begin(pool).await?;
    let result = sqlx::query("insert into seats (id, account_group_id, name, enabled, max_concurrency, daily_limit_usd, weekly_limit_usd)
        values ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric)
        on conflict (id) do update set name = excluded.name, enabled = excluded.enabled,
        max_concurrency = excluded.max_concurrency, daily_limit_usd = excluded.daily_limit_usd,
        weekly_limit_usd = excluded.weekly_limit_usd, updated_at = now()
        where seats.account_group_id = excluded.account_group_id")
        .bind(id.as_str()).bind(command.group_id.as_str()).bind(command.name).bind(command.enabled)
        .bind(i64::try_from(command.max_concurrency).map_err(|_| invalid("并发上限无效"))?)
        .bind(command.limits.daily_usd.canonical()).bind(command.limits.weekly_usd.canonical())
        .execute(&mut *tx).await.map_err(database)?;
    if result.rows_affected() != 1 {
        return Err(invalid("seat 不允许跨 car 转移"));
    }
    commit(tx, revision, context, "save_seat", id.as_str()).await
}

pub(super) async fn list_seats(
    pool: &PgPool,
    group_id: AccountGroupId,
) -> AdminStoreResult<Vec<SeatRecord>> {
    let rows = sqlx::query("select s.*, s.daily_limit_usd::text as daily_limit, s.weekly_limit_usd::text as weekly_limit,
        (select count(*) from client_api_keys k where k.seat_id = s.id and k.revoked_at is null) as key_count,
        (case when w.daily_end > now() then w.daily_used_usd else 0 end)::text as daily_used,
        (case when w.weekly_end > now() then w.weekly_used_usd else 0 end)::text as weekly_used,
        case when w.daily_end > now() then w.daily_end end as daily_end,
        case when w.weekly_end > now() then w.weekly_end end as weekly_end
        from seats s left join seat_budget_windows w on w.seat_id = s.id where s.account_group_id = $1 order by s.created_at, s.id")
        .bind(group_id.as_str()).fetch_all(pool).await.map_err(database)?;
    rows.iter()
        .map(|row| {
            let amount = |field| {
                row.get::<String, _>(field)
                    .parse::<Decimal>()
                    .map_err(|_| invalid("seat 费用数据无效"))
            };
            Ok(SeatRecord {
                id: SeatId::new(row.get::<String, _>("id")).map_err(|_| invalid("seat ID 无效"))?,
                group_id: group_id.clone(),
                name: row.get("name"),
                enabled: row.get("enabled"),
                max_concurrency: u64::try_from(row.get::<i64, _>("max_concurrency"))
                    .map_err(|_| invalid("并发上限无效"))?,
                key_count: u64::try_from(row.get::<i64, _>("key_count"))
                    .map_err(|_| invalid("Key 数量无效"))?,
                budget: ClientBudgetStatus {
                    seat: None,
                    limits: ClientBudgetLimits {
                        daily_usd: amount("daily_limit")?,
                        weekly_usd: amount("weekly_limit")?,
                    },
                    daily_used_usd: amount("daily_used")?,
                    weekly_used_usd: amount("weekly_used")?,
                    daily_resets_at: row
                        .get::<Option<DateTime<Utc>>, _>("daily_end")
                        .map(Into::into),
                    weekly_resets_at: row
                        .get::<Option<DateTime<Utc>>, _>("weekly_end")
                        .map(Into::into),
                },
            })
        })
        .collect()
}

pub(super) async fn join_seat(
    pool: &PgPool,
    command: JoinSeat,
    context: &MutationContext,
) -> AdminStoreResult<Revision> {
    let (mut tx, revision) = begin(pool).await?;
    let mut ids = command
        .key_ids
        .iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    // 所有费用路径都先锁 Key 再锁 seat，保持同一锁顺序。
    let keys = sqlx::query(
        "select id, seat_id from client_api_keys where id = any($1) and revoked_at is null order by id for update",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(database)?;
    if keys.len() != ids.len() {
        return Err(invalid("部分 Key 已不存在"));
    }
    sqlx::query("select id from seats where id = $1 for update")
        .bind(command.seat_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database)?
        .ok_or_else(|| invalid("seat 不存在"))?;
    let now = Utc::now();
    for key in keys {
        let id: String = key.get("id");
        if let Some(existing) = key.get::<Option<String>, _>("seat_id") {
            if existing == command.seat_id.as_str() {
                continue;
            }
            return Err(invalid("Key 不允许在 seat 之间转移"));
        }
        let running: bool = sqlx::query_scalar("select exists(select 1 from client_budget_admissions where client_api_key_id = $1 and expires_at > now()) or exists(select 1 from model_requests where client_api_key_id = $1 and outcome = 'running')")
            .bind(&id).fetch_one(&mut *tx).await.map_err(database)?;
        if running {
            return Err(invalid("请等待 Key 的在途请求完成后再迁入"));
        }
        advance_owner_windows(&mut tx, BudgetOwner::Key(&id), now)
            .await
            .map_err(database)?;
        carry_budget(&mut tx, &id, command.seat_id.as_str(), now).await?;
        sqlx::query("delete from client_api_key_groups where client_api_key_id = $1")
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(database)?;
        sqlx::query("update client_api_keys set seat_id = $2, updated_at = now() where id = $1")
            .bind(&id)
            .bind(command.seat_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(database)?;
    }
    commit(tx, revision, context, "join_seat", command.seat_id.as_str()).await
}

async fn carry_budget(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    seat: &str,
    now: DateTime<Utc>,
) -> AdminStoreResult<()> {
    // 首个有余额的 Key 决定共同窗口；空 seat 不应提前起算周窗口。
    sqlx::query("insert into seat_budget_windows (seat_id, daily_start, daily_end, weekly_start, weekly_end)
        select $2, daily_start, daily_end, weekly_start, weekly_end from client_key_budget_windows where client_api_key_id = $1
        on conflict (seat_id) do nothing")
        .bind(key).bind(seat).execute(&mut **tx).await.map_err(database)?;
    advance_owner_windows(tx, BudgetOwner::Seat(seat), now)
        .await
        .map_err(database)?;
    for period in ["daily", "weekly"] {
        // period 仅为上方两个常量，标识符不接收用户输入。
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("select k.{period}_start as key_start, k.{period}_end as key_end,
            k.{period}_used_usd::text as key_used, s.{period}_start as seat_start, s.{period}_end as seat_end,
            s.{period}_used_usd::text as seat_used from client_key_budget_windows k cross join seat_budget_windows s
            where k.client_api_key_id = $1 and s.seat_id = $2")))
            .bind(key).bind(seat).fetch_one(&mut **tx).await.map_err(database)?;
        let used: Decimal = row
            .get::<String, _>("key_used")
            .parse()
            .map_err(|_| invalid("费用无效"))?;
        if used == Decimal::ZERO {
            continue;
        }
        let seat_used: Decimal = row
            .get::<String, _>("seat_used")
            .parse()
            .map_err(|_| invalid("费用无效"))?;
        let start: DateTime<Utc> = row.get("key_start");
        let end: DateTime<Utc> = row.get("key_end");
        if seat_used != Decimal::ZERO
            && (start != row.get::<DateTime<Utc>, _>("seat_start")
                || end != row.get::<DateTime<Utc>, _>("seat_end"))
        {
            return Err(invalid(
                "已有消费的预算窗口不一致，不能直接迁入；请等待窗口对齐",
            ));
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "update seat_budget_windows set {period}_start = $2, {period}_end = $3,
            {period}_used_usd = {period}_used_usd + $4::text::numeric where seat_id = $1"
        )))
        .bind(seat)
        .bind(start)
        .bind(end)
        .bind(used.canonical())
        .execute(&mut **tx)
        .await
        .map_err(database)?;
    }
    Ok(())
}
