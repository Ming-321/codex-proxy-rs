//! car / seat 的原子配置与既有费用承接。

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationContext, Revision,
        account_groups::{
            CarAccount, CarQuotaMode, CarQuotaObservation, CarQuotaSettings, CarQuotaState,
            JoinSeat, ReplaceCarQuotaSettings, SaveCarWeights, SaveSeat, SeatRecord,
        },
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
};
use gateway_core::{
    account::ProviderAccountId,
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
    sqlx::query(
        "insert into car_quota_cycles (account_group_id, published_capacity_usd, prediction_reason)
        select id, car_total_weight * 130, '等待账号周期数据' from account_groups where id = $1
        on conflict (account_group_id) do nothing",
    )
    .bind(id.as_str())
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    commit(tx, revision, context, "convert_car", id.as_str()).await
}

pub(super) async fn save_seat(
    pool: &PgPool,
    command: SaveSeat,
    context: &MutationContext,
) -> AdminStoreResult<Revision> {
    let id = command.id.ok_or_else(|| invalid("seat ID 缺失"))?;
    let (mut tx, revision) = begin(pool).await?;
    let result = sqlx::query("insert into seats (id, account_group_id, name, enabled, max_concurrency, weight, daily_limit_usd, weekly_limit_usd)
        values ($1, $2, $3, $4, $5, $6::text::numeric, $7::text::numeric, $8::text::numeric)
        on conflict (id) do update set name = excluded.name, enabled = excluded.enabled,
        max_concurrency = excluded.max_concurrency, weight = excluded.weight,
        daily_limit_usd = excluded.daily_limit_usd,
        weekly_limit_usd = excluded.weekly_limit_usd, updated_at = now()
        where seats.account_group_id = excluded.account_group_id")
        .bind(id.as_str()).bind(command.group_id.as_str()).bind(command.name).bind(command.enabled)
        .bind(i64::try_from(command.max_concurrency).map_err(|_| invalid("并发上限无效"))?)
        .bind(command.weight.canonical())
        .bind(command.limits.daily_usd.canonical()).bind(command.limits.weekly_usd.canonical())
        .execute(&mut *tx).await.map_err(database)?;
    if result.rows_affected() != 1 {
        return Err(invalid("seat 不允许跨 car 转移"));
    }
    sqlx::query("update seats s set weekly_limit_usd = c.published_capacity_usd * s.weight / g.car_total_weight
        from account_groups g, car_quota_cycles c where s.id = $1 and g.id = s.account_group_id
          and c.account_group_id = g.id and g.car_quota_mode = 'active'")
        .bind(id.as_str()).execute(&mut *tx).await.map_err(database)?;
    commit(tx, revision, context, "save_seat", id.as_str()).await
}

pub(super) async fn list_seats(
    pool: &PgPool,
    group_id: AccountGroupId,
) -> AdminStoreResult<Vec<SeatRecord>> {
    let rows = sqlx::query("select s.*, s.weight::text as seat_weight, s.daily_limit_usd::text as daily_limit, s.weekly_limit_usd::text as weekly_limit,
        (select count(*) from client_api_keys k where k.seat_id = s.id and k.revoked_at is null) as key_count,
        (case when w.daily_end > now() then w.daily_used_usd else 0 end)::text as daily_used,
        (case when g.car_quota_mode = 'active' or w.weekly_end > now() then w.weekly_used_usd else 0 end)::text as weekly_used,
        case when w.daily_end > now() then w.daily_end end as daily_end,
        case when g.car_quota_mode = 'active' or w.weekly_end > now() then w.weekly_end end as weekly_end
        from seats s join account_groups g on g.id = s.account_group_id
        left join seat_budget_windows w on w.seat_id = s.id where s.account_group_id = $1 order by s.created_at, s.id")
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
                weight: amount("seat_weight")?,
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
    let account_cycle = sqlx::query_as::<_, (DateTime<Utc>, DateTime<Utc>)>(
        "select c.cycle_start, c.cycle_end from seats s
        join account_groups g on g.id = s.account_group_id
        join car_quota_cycles c on c.account_group_id = g.id
        where s.id = $1 and g.car_quota_mode = 'active'
          and c.cycle_start is not null and c.cycle_end is not null",
    )
    .bind(seat)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database)?;
    // 日窗口继续承接 Key；周期窗口启用后始终服从账号事实。
    sqlx::query("insert into seat_budget_windows (seat_id, daily_start, daily_end, weekly_start, weekly_end)
        select $2, daily_start, daily_end, coalesce($3, weekly_start), coalesce($4, weekly_end)
        from client_key_budget_windows where client_api_key_id = $1
        on conflict (seat_id) do nothing")
        .bind(key)
        .bind(seat)
        .bind(account_cycle.map(|cycle| cycle.0))
        .bind(account_cycle.map(|cycle| cycle.1))
        .execute(&mut **tx)
        .await
        .map_err(database)?;
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
        let (start, end, used) = if period == "weekly" {
            if let Some((start, end)) = account_cycle {
                let used: Decimal = sqlx::query_scalar::<_, String>(
                    "select coalesce(sum(amount_usd), 0)::text from client_key_charge_events
                    where client_api_key_id = $1 and completed_at >= $2 and completed_at < $3",
                )
                .bind(key)
                .bind(start)
                .bind(end)
                .fetch_one(&mut **tx)
                .await
                .map_err(database)?
                .parse()
                .map_err(|_| invalid("费用无效"))?;
                (start, end, used)
            } else {
                (
                    row.get("key_start"),
                    row.get("key_end"),
                    row.get::<String, _>("key_used")
                        .parse()
                        .map_err(|_| invalid("费用无效"))?,
                )
            }
        } else {
            (
                row.get("key_start"),
                row.get("key_end"),
                row.get::<String, _>("key_used")
                    .parse()
                    .map_err(|_| invalid("费用无效"))?,
            )
        };
        if used == Decimal::ZERO {
            continue;
        }
        let seat_used: Decimal = row
            .get::<String, _>("seat_used")
            .parse()
            .map_err(|_| invalid("费用无效"))?;
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

fn amount(row: &sqlx::postgres::PgRow, field: &str) -> AdminStoreResult<Decimal> {
    row.get::<String, _>(field)
        .parse::<Decimal>()
        .map_err(|_| invalid("car 额度数据无效"))
}

fn percent_millis(row: &sqlx::postgres::PgRow, field: &str) -> AdminStoreResult<u32> {
    let value = row
        .get::<String, _>(field)
        .parse::<f64>()
        .map_err(|_| invalid("car 估算设置无效"))?;
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(invalid("car 估算设置超出范围"));
    }
    Ok((value * 1_000.0).round() as u32)
}

fn millis_text(value: u32) -> String {
    format!("{:.3}", f64::from(value) / 1_000.0)
}

pub(super) async fn load_car_quota_settings(pool: &PgPool) -> AdminStoreResult<CarQuotaSettings> {
    let row = sqlx::query(
        "select automatic_updates, publish_interval_seconds,
        outside_usage_protection, minimum_sample_percent::text as minimum_sample,
        estimate_weight_percent::text as estimate_weight,
        minimum_change_percent::text as minimum_change,
        maximum_adjustment_percent::text as maximum_adjustment,
        abnormal_change_percent::text as abnormal_change, updated_at
        from car_quota_settings where singleton",
    )
    .fetch_one(pool)
    .await
    .map_err(database)?;
    Ok(CarQuotaSettings {
        automatic_updates: row.get("automatic_updates"),
        publish_interval_seconds: u64::try_from(row.get::<i64, _>("publish_interval_seconds"))
            .map_err(|_| invalid("发布间隔无效"))?,
        outside_usage_protection: row.get("outside_usage_protection"),
        minimum_sample_millis: percent_millis(&row, "minimum_sample")?,
        estimate_weight_millis: percent_millis(&row, "estimate_weight")?,
        minimum_change_millis: percent_millis(&row, "minimum_change")?,
        maximum_adjustment_millis: percent_millis(&row, "maximum_adjustment")?,
        abnormal_change_millis: percent_millis(&row, "abnormal_change")?,
        updated_at: row.get("updated_at"),
    })
}

pub(super) async fn replace_car_quota_settings(
    pool: &PgPool,
    command: ReplaceCarQuotaSettings,
    context: &MutationContext,
) -> AdminStoreResult<CarQuotaSettings> {
    let (mut tx, revision) = begin(pool).await?;
    sqlx::query(
        "update car_quota_settings set automatic_updates = $1,
        publish_interval_seconds = $2, outside_usage_protection = $3,
        minimum_sample_percent = $4::text::numeric, estimate_weight_percent = $5::text::numeric,
        minimum_change_percent = $6::text::numeric, maximum_adjustment_percent = $7::text::numeric,
        abnormal_change_percent = $8::text::numeric, updated_at = now() where singleton",
    )
    .bind(command.automatic_updates)
    .bind(i64::try_from(command.publish_interval_seconds).map_err(|_| invalid("发布间隔无效"))?)
    .bind(command.outside_usage_protection)
    .bind(millis_text(command.minimum_sample_millis))
    .bind(millis_text(command.estimate_weight_millis))
    .bind(millis_text(command.minimum_change_millis))
    .bind(millis_text(command.maximum_adjustment_millis))
    .bind(millis_text(command.abnormal_change_millis))
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    super::append_admin_audit_event_in_transaction(
        &mut tx,
        crate::mutation_audit(
            context,
            "car_quota_settings",
            "settings",
            "car-quota",
            vec!["car_quota".to_owned()],
        ),
        revision,
    )
    .await
    .map_err(|e| crate::admin_store_error("car quota settings", e))?;
    tx.commit().await.map_err(database)?;
    load_car_quota_settings(pool).await
}

pub(super) async fn list_car_accounts(pool: &PgPool) -> AdminStoreResult<Vec<CarAccount>> {
    let rows = sqlx::query(
        "select g.id as group_id, a.provider_account_id as account_id
        from account_groups g join account_group_accounts a on a.account_group_id = g.id
        where g.is_car and g.enabled order by g.id",
    )
    .fetch_all(pool)
    .await
    .map_err(database)?;
    rows.into_iter()
        .map(|row| {
            Ok(CarAccount {
                group_id: AccountGroupId::new(row.get::<String, _>("group_id"))
                    .map_err(|_| invalid("car ID 无效"))?,
                account_id: ProviderAccountId::new(row.get::<String, _>("account_id"))
                    .map_err(|_| invalid("账号 ID 无效"))?,
            })
        })
        .collect()
}

fn quota_mode(value: &str) -> AdminStoreResult<CarQuotaMode> {
    match value {
        "legacy" => Ok(CarQuotaMode::Legacy),
        "waiting" => Ok(CarQuotaMode::Waiting),
        "active" => Ok(CarQuotaMode::Active),
        _ => Err(invalid("car 周期状态无效")),
    }
}

pub(super) async fn load_car_quota_state(
    pool: &PgPool,
    group_id: AccountGroupId,
) -> AdminStoreResult<CarQuotaState> {
    let row = sqlx::query(
        "select g.car_total_weight::text as total_weight, g.car_quota_mode,
        c.window_key, c.cycle_start, c.cycle_end, c.last_used_percent::text as last_used,
        c.published_capacity_usd::text as published_capacity,
        c.predicted_capacity_usd::text as predicted_capacity, c.prediction_reason,
        c.published_at, c.updated_at
        from account_groups g left join car_quota_cycles c on c.account_group_id = g.id
        where g.id = $1 and g.is_car",
    )
    .bind(group_id.as_str())
    .fetch_optional(pool)
    .await
    .map_err(database)?
    .ok_or_else(|| invalid("car 不存在"))?;
    let predicted = row
        .get::<Option<String>, _>("predicted_capacity")
        .map(|value| {
            value
                .parse::<Decimal>()
                .map_err(|_| invalid("预测额度无效"))
        })
        .transpose()?;
    Ok(CarQuotaState {
        group_id,
        total_weight: amount(&row, "total_weight")?,
        mode: quota_mode(row.get::<String, _>("car_quota_mode").as_str())?,
        window_key: row.get("window_key"),
        cycle_start: row.get("cycle_start"),
        cycle_end: row.get("cycle_end"),
        account_used_percent_millis: row
            .get::<Option<String>, _>("last_used")
            .and_then(|value| value.parse::<f64>().ok())
            .map(|value| (value * 1_000.0).round() as u32),
        published_capacity_usd: amount(&row, "published_capacity")?,
        predicted_capacity_usd: predicted,
        prediction_reason: row.get("prediction_reason"),
        published_at: row.get("published_at"),
        updated_at: row.get("updated_at"),
    })
}

pub(super) async fn save_car_weights(
    pool: &PgPool,
    command: SaveCarWeights,
    context: &MutationContext,
) -> AdminStoreResult<Revision> {
    let (mut tx, revision) = begin(pool).await?;
    let result = sqlx::query(
        "update account_groups set car_total_weight = $2::text::numeric,
        updated_at = now() where id = $1 and is_car",
    )
    .bind(command.group_id.as_str())
    .bind(command.total_weight.canonical())
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    if result.rows_affected() != 1 {
        return Err(invalid("car 不存在"));
    }
    sqlx::query(
        "update car_quota_cycles c set published_capacity_usd = $2::text::numeric * 130,
        prediction_reason = '等待账号周期数据', updated_at = now()
        from account_groups g where c.account_group_id = $1 and g.id = c.account_group_id
          and g.car_quota_mode <> 'active'",
    )
    .bind(command.group_id.as_str())
    .bind(command.total_weight.canonical())
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    sqlx::query("update seats s set weekly_limit_usd = c.published_capacity_usd * s.weight / g.car_total_weight,
        updated_at = now() from account_groups g, car_quota_cycles c
        where s.account_group_id = $1 and g.id = s.account_group_id
          and c.account_group_id = g.id and g.car_quota_mode = 'active'")
        .bind(command.group_id.as_str()).execute(&mut *tx).await.map_err(database)?;
    commit(
        tx,
        revision,
        context,
        "save_car_weights",
        command.group_id.as_str(),
    )
    .await
}

fn decimal_f64(value: Decimal) -> f64 {
    value.canonical().parse().unwrap_or(0.0)
}

fn decimal_from_f64(value: f64) -> AdminStoreResult<Decimal> {
    if !value.is_finite() || value < 0.0 {
        return Err(invalid("预测额度无效"));
    }
    format!("{value:.10}")
        .parse()
        .map_err(|_| invalid("预测额度超出范围"))
}

pub(super) async fn reconcile_car_quota(
    pool: &PgPool,
    observation: CarQuotaObservation,
) -> AdminStoreResult<CarQuotaState> {
    if observation.cycle_start > observation.observed_at
        || observation.observed_at >= observation.cycle_end
        || observation.used_percent_millis > 100_000
    {
        return Err(invalid("账号周期观测的时间或用量无效"));
    }
    let settings = load_car_quota_settings(pool).await?;
    let mut tx = pool.begin().await.map_err(database)?;
    let row = sqlx::query(
        "select g.car_quota_mode, g.car_total_weight::text as total_weight,
        c.window_key, c.cycle_start, c.cycle_end, c.last_observed_at, c.last_used_percent::text as last_used,
        c.published_capacity_usd::text as published_capacity, c.published_at,
        c.published_sample_end, c.reset_candidate_start, c.reset_candidate_end, c.reset_candidate_observed_at,
        c.abnormal_candidate_usd::text as abnormal_candidate,
        c.abnormal_candidate_sample_end
        from account_groups g join car_quota_cycles c on c.account_group_id = g.id
        where g.id = $1 and g.is_car for update",
    )
    .bind(observation.group_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    .ok_or_else(|| invalid("car 周期状态不存在"))?;
    let mode = quota_mode(row.get::<String, _>("car_quota_mode").as_str())?;
    let current_end = row.get::<Option<DateTime<Utc>>, _>("cycle_end");
    let current_start = row.get::<Option<DateTime<Utc>>, _>("cycle_start");
    let last_observed = row.get::<Option<DateTime<Utc>>, _>("last_observed_at");
    if last_observed.is_some_and(|last| observation.observed_at <= last) {
        tx.rollback().await.map_err(database)?;
        return load_car_quota_state(pool, observation.group_id).await;
    }
    let last_used = row
        .get::<Option<String>, _>("last_used")
        .and_then(|value| value.parse::<f64>().ok())
        .map(|value| (value * 1_000.0).round() as u32);
    let first_cycle = current_end.is_none();
    let same_cycle = current_start == Some(observation.cycle_start)
        && current_end == Some(observation.cycle_end)
        && row.get::<Option<String>, _>("window_key").as_deref() == Some(&observation.window_key);
    let forward_cycle = current_end.is_some_and(|end| observation.cycle_end > end)
        && current_start.is_some_and(|start| observation.cycle_start > start);
    if !first_cycle && !same_cycle && !forward_cycle {
        tx.rollback().await.map_err(database)?;
        return load_car_quota_state(pool, observation.group_id).await;
    }
    let clear_roll = forward_cycle
        && current_end
            .is_some_and(|end| observation.cycle_start >= end || observation.observed_at >= end);
    let earlier_new_end = forward_cycle && !clear_roll;
    let usage_reset = last_used.is_some_and(|used| observation.used_percent_millis + 1_000 < used);
    let candidate_confirmed = earlier_new_end
        && row.get::<Option<DateTime<Utc>>, _>("reset_candidate_start")
            == Some(observation.cycle_start)
        && row.get::<Option<DateTime<Utc>>, _>("reset_candidate_end")
            == Some(observation.cycle_end)
        && row
            .get::<Option<DateTime<Utc>>, _>("reset_candidate_observed_at")
            .is_some_and(|at| at < observation.observed_at)
        && usage_reset;
    let new_cycle = first_cycle || clear_roll || candidate_confirmed;

    if earlier_new_end && !candidate_confirmed {
        sqlx::query(
            "update car_quota_cycles set reset_candidate_end = $2, reset_candidate_start = $4,
            reset_candidate_observed_at = $3, prediction_reason = '提前重置等待再次确认',
            last_observed_at = $3, updated_at = now()
            where account_group_id = $1",
        )
        .bind(observation.group_id.as_str())
        .bind(usage_reset.then_some(observation.cycle_end))
        .bind(observation.observed_at)
        // 保留重置前用量基线，下一次新周期消费增加也能确认。
        .bind(usage_reset.then_some(observation.cycle_start))
        .execute(&mut *tx)
        .await
        .map_err(database)?;
        tx.commit().await.map_err(database)?;
        return load_car_quota_state(pool, observation.group_id).await;
    }

    if new_cycle {
        let total_weight = amount(&row, "total_weight")?;
        let mut capacity = amount(&row, "published_capacity")?;
        if capacity == Decimal::ZERO {
            capacity = decimal_from_f64(decimal_f64(total_weight) * 130.0)?;
        }
        sqlx::query(
            "update account_groups set car_quota_mode = 'active', updated_at = now() where id = $1",
        )
        .bind(observation.group_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(database)?;
        sqlx::query("update seats s set weekly_limit_usd = $2::text::numeric * s.weight / g.car_total_weight,
            updated_at = now() from account_groups g where s.account_group_id = $1 and g.id = s.account_group_id")
            .bind(observation.group_id.as_str()).bind(capacity.canonical())
            .execute(&mut *tx).await.map_err(database)?;
        sqlx::query("insert into seat_budget_windows (seat_id, daily_start, daily_end, weekly_start, weekly_end, weekly_used_usd)
            select s.id, date_trunc('day', now() at time zone 'Asia/Shanghai') at time zone 'Asia/Shanghai',
                (date_trunc('day', now() at time zone 'Asia/Shanghai') + interval '1 day') at time zone 'Asia/Shanghai',
                $2, $3, coalesce((select sum(e.amount_usd) from client_key_charge_events e
                    join client_api_keys k on k.id = e.client_api_key_id
                    where k.seat_id = s.id and e.completed_at >= $2 and e.completed_at < $3), 0)
            from seats s where s.account_group_id = $1
            on conflict (seat_id) do update set weekly_start = excluded.weekly_start,
                weekly_end = excluded.weekly_end, weekly_used_usd = excluded.weekly_used_usd")
            .bind(observation.group_id.as_str()).bind(observation.cycle_start).bind(observation.cycle_end)
            .execute(&mut *tx).await.map_err(database)?;
    }

    let predicted = observation.predicted_capacity_usd;
    let mut published = amount(&row, "published_capacity")?;
    let mut publish = false;
    let mut reason = observation.prediction_reason.clone();
    if (mode == CarQuotaMode::Active || new_cycle)
        && let Some(raw) = predicted
    {
        let sampled = observation.sample_percent_millis.unwrap_or(0);
        let interval_ready = row
            .get::<Option<DateTime<Utc>>, _>("published_at")
            .is_none_or(|at| {
                Utc::now() - at
                    >= chrono::Duration::seconds(settings.publish_interval_seconds as i64)
            });
        let sample_new = observation.sample_end.is_some()
            && observation.sample_end > row.get::<Option<DateTime<Utc>>, _>("published_sample_end");
        if settings.automatic_updates
            && observation.pending_request_count == 0
            && sampled >= settings.minimum_sample_millis
            && interval_ready
            && sample_new
        {
            let old = decimal_f64(published);
            let raw_value = decimal_f64(raw);
            let deviation = if old > 0.0 {
                ((raw_value - old) / old * 100.0).abs()
            } else {
                100.0
            };
            let abnormal = deviation * 1_000.0 > f64::from(settings.abnormal_change_millis);
            let candidate_value = row
                .get::<Option<String>, _>("abnormal_candidate")
                .and_then(|value| value.parse::<Decimal>().ok());
            let candidate_end =
                row.get::<Option<DateTime<Utc>>, _>("abnormal_candidate_sample_end");
            let abnormal_confirmed = abnormal
                && candidate_value.is_some()
                && observation
                    .sample_start
                    .zip(candidate_end)
                    .is_some_and(|(start, end)| start >= end)
                && ((decimal_f64(candidate_value.unwrap()) - old).signum()
                    == (raw_value - old).signum());
            if abnormal && settings.outside_usage_protection && !abnormal_confirmed {
                sqlx::query("update car_quota_cycles set abnormal_candidate_usd = $2::text::numeric,
                        abnormal_candidate_sample_end = $3, prediction_reason = '异常变化等待独立样本确认'
                        where account_group_id = $1")
                        .bind(observation.group_id.as_str()).bind(raw.canonical()).bind(observation.sample_end)
                        .execute(&mut *tx).await.map_err(database)?;
                reason = Some("异常变化等待独立样本确认".to_owned());
            } else if deviation * 1_000.0 >= f64::from(settings.minimum_change_millis) {
                let blended = old
                    + (raw_value - old) * f64::from(settings.estimate_weight_millis) / 100_000.0;
                let max_step = old * f64::from(settings.maximum_adjustment_millis) / 100_000.0;
                let next = blended.clamp((old - max_step).max(0.0), old + max_step);
                published = decimal_from_f64(next)?;
                publish = true;
                reason = None;
            } else {
                reason = Some("变化未达到最小更新幅度".to_owned());
            }
        } else {
            reason = Some(
                if !settings.automatic_updates {
                    "自动更新已关闭，保留当前生效限额"
                } else if observation.pending_request_count > 0 {
                    "存在尚未结算请求，等待完整交付"
                } else if sampled < settings.minimum_sample_millis {
                    "有效样本进度未达到自动更新门槛"
                } else if !interval_ready {
                    "未到自动更新间隔，保留当前生效限额"
                } else {
                    "尚无新的额度观测，保留当前生效限额"
                }
                .to_owned(),
            );
        }
    }

    sqlx::query("update car_quota_cycles set window_key = $2, cycle_start = $3, cycle_end = $4,
        last_observed_at = $5, last_used_percent = $6::text::numeric,
        predicted_capacity_usd = $7::text::numeric, prediction_reason = $8,
        published_capacity_usd = $9::text::numeric,
        published_at = case when $10 then now() else published_at end,
        published_sample_end = case when $10 then $11 else published_sample_end end,
        reset_candidate_start = null, reset_candidate_end = null,
        reset_candidate_observed_at = null,
        abnormal_candidate_usd = case when $10 then null else abnormal_candidate_usd end,
        abnormal_candidate_sample_end = case when $10 then null else abnormal_candidate_sample_end end,
        updated_at = now() where account_group_id = $1")
        .bind(observation.group_id.as_str()).bind(&observation.window_key)
        .bind(observation.cycle_start).bind(observation.cycle_end).bind(observation.observed_at)
        .bind(millis_text(observation.used_percent_millis))
        .bind(predicted.map(Decimal::canonical))
        .bind(reason).bind(published.canonical()).bind(publish).bind(observation.sample_end)
        .execute(&mut *tx).await.map_err(database)?;
    if publish {
        sqlx::query("update seats s set weekly_limit_usd = $2::text::numeric * s.weight / g.car_total_weight,
            updated_at = now() from account_groups g where s.account_group_id = $1 and g.id = s.account_group_id")
            .bind(observation.group_id.as_str()).bind(published.canonical())
            .execute(&mut *tx).await.map_err(database)?;
    }
    tx.commit().await.map_err(database)?;
    load_car_quota_state(pool, observation.group_id).await
}
