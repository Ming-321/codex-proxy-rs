//! 分组、车位和成员 Key 的一次性配置事务。

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationContext,
        account_groups::{CarManagementResult, JoinSeat, PreparedCarManagement, SaveSeat},
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
};
use gateway_core::{
    account::OpaqueProviderData,
    engine::budget::ClientBudgetLimits,
    policy::{ClientApiKeyId, SeatId},
    routing::AccountGroupId,
};
use sqlx::{PgPool, Postgres, Row as _, Transaction};

use super::{
    client_keys::{
        NewClientApiKey, UpdateClientApiKeyDetails, insert_client_api_key_in_transaction,
        update_client_api_key_in_transaction,
    },
    seats::{database, invalid, join_seat_in_transaction, save_seat_in_transaction},
};

fn conflict(message: &str) -> AdminStoreError {
    AdminStoreError::new(AdminStoreErrorKind::Conflict, "car", message)
}

pub(super) async fn save(
    pool: &PgPool,
    command: PreparedCarManagement,
    context: &MutationContext,
) -> AdminStoreResult<CarManagementResult> {
    let draft = &command.draft;
    let mut tx = pool.begin().await.map_err(database)?;
    // 与其他控制面写入共享版本锁，重试回执也在同一串行边界内读取。
    let revision = super::bump_config_revision_in_transaction(&mut tx)
        .await
        .map_err(|error| crate::admin_store_error("car", error))?;
    if let Some(row) = sqlx::query(
        "select fingerprint, result from car_management_receipts where request_id = $1::text::uuid",
    )
    .bind(&draft.request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    {
        if row.get::<String, _>("fingerprint") != command.fingerprint {
            return Err(conflict("保存请求已用于另一份配置，请重新预览"));
        }
        let result =
            serde_json::from_value(row.get("result")).map_err(|_| invalid("保存回执无效"))?;
        tx.rollback().await.map_err(database)?;
        return Ok(result);
    }
    if revision.get().saturating_sub(1) != draft.expected_revision {
        return Err(conflict("配置已变化，请刷新后重新预览，当前草稿尚未保存"));
    }
    let existing =
        sqlx::query("select is_car, car_quota_policy from account_groups where id = $1 for update")
            .bind(&draft.group_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(database)?;
    if draft.create != existing.is_none() {
        return Err(conflict("分组状态已变化，请刷新后重新编辑"));
    }
    let old_policy = existing
        .as_ref()
        .map_or("manual", |row| row.get::<&str, _>("car_quota_policy"));
    let cycle = sqlx::query(
        "select updated_at from car_quota_cycles where account_group_id = $1 for update",
    )
    .bind(&draft.group_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?;
    let updated_at = cycle
        .as_ref()
        .map(|row| row.get::<DateTime<Utc>, _>("updated_at"));
    if updated_at != draft.quota_updated_at {
        return Err(conflict("账号周期或额度已变化，请刷新后重新预览"));
    }
    let old_seats: Vec<String> =
        sqlx::query_scalar("select id from seats where account_group_id = $1 order by id")
            .bind(&draft.group_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(database)?;
    if old_seats
        .iter()
        .any(|id| !draft.seats.iter().any(|seat| &seat.id == id))
    {
        return Err(invalid("已有车位不能删除，请保留或停用"));
    }
    // 费用结算按 Key → seat 加锁，批量配置沿用相同顺序。
    let mut key_ids = draft
        .seats
        .iter()
        .flat_map(|seat| seat.keys.iter())
        .filter(|key| !key.create)
        .map(|key| key.id.clone())
        .collect::<Vec<_>>();
    let retained: Vec<String> = sqlx::query_scalar("select k.id from client_api_keys k join seats s on s.id = k.seat_id where s.account_group_id = $1")
        .bind(&draft.group_id).fetch_all(&mut *tx).await.map_err(database)?;
    key_ids.extend(retained);
    key_ids.sort_unstable();
    key_ids.dedup();
    sqlx::query("select id from client_api_keys where id = any($1) order by id for update")
        .bind(&key_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(database)?;
    sqlx::query("select id from seats where account_group_id = $1 order by id for update")
        .bind(&draft.group_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(database)?;

    if draft.create {
        sqlx::query("insert into account_groups (id, name, description, color, enabled, disable_fast, is_car, created_at, updated_at)
            values ($1, $2, $3, $4, $5, $6, true, now(), now())")
            .bind(&draft.group_id).bind(&draft.name).bind(&draft.description).bind(draft.color.to_uppercase())
            .bind(draft.enabled).bind(draft.disable_fast).execute(&mut *tx).await.map_err(database)?;
    }
    let accounts: Vec<String> = sqlx::query_scalar(
        "select provider_account_id from account_group_accounts where account_group_id = $1",
    )
    .bind(&draft.group_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(database)?;
    if accounts != [draft.account_id.clone()] {
        if !old_seats.is_empty() {
            return Err(invalid("已有车位的拼车不能换绑账号"));
        }
        sqlx::query("delete from account_group_accounts where account_group_id = $1")
            .bind(&draft.group_id)
            .execute(&mut *tx)
            .await
            .map_err(database)?;
        sqlx::query("insert into account_group_accounts (account_group_id, provider_account_id, created_at) values ($1, $2, now())")
            .bind(&draft.group_id).bind(&draft.account_id).execute(&mut *tx).await.map_err(database)?;
    }
    sqlx::query(
        "update account_groups set name = $2, description = $3, color = $4, enabled = $5,
        disable_fast = $6, is_car = true, car_total_weight = $7::text::numeric,
        car_allocation = $8, updated_at = now() where id = $1",
    )
    .bind(&draft.group_id)
    .bind(&draft.name)
    .bind(&draft.description)
    .bind(draft.color.to_uppercase())
    .bind(draft.enabled)
    .bind(draft.disable_fast)
    .bind(&draft.total_weight)
    .bind(&draft.allocation)
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    sqlx::query("insert into car_quota_cycles (account_group_id, prediction_reason) values ($1, '尚未启用账号周期') on conflict do nothing")
        .bind(&draft.group_id).execute(&mut *tx).await.map_err(database)?;
    if draft.quota_policy == "automatic" && old_policy != "automatic" {
        let capacity = draft
            .initial_capacity_usd
            .as_ref()
            .ok_or_else(|| invalid("请确认自动管理的初始总额度"))?;
        sqlx::query("update car_quota_cycles set published_capacity_usd = $2::text::numeric where account_group_id = $1")
            .bind(&draft.group_id).bind(capacity).execute(&mut *tx).await.map_err(database)?;
    } else if draft.initial_capacity_usd.is_some() {
        return Err(invalid("初始总额度只在启用自动管理时设置"));
    }
    if draft.quota_policy == "manual" && old_policy != "manual" {
        restore_manual_windows(&mut tx, &draft.group_id).await?;
    }
    sqlx::query(
        "update account_groups set car_quota_policy = $2,
        car_quota_mode = case when $2 = 'manual' then 'legacy'
        when car_quota_policy = 'manual' then 'waiting' else car_quota_mode end where id = $1",
    )
    .bind(&draft.group_id)
    .bind(&draft.quota_policy)
    .execute(&mut *tx)
    .await
    .map_err(database)?;
    if old_policy != draft.quota_policy
        && (old_policy == "manual" || draft.quota_policy == "manual")
    {
        let reason = if draft.quota_policy == "manual" {
            "手动设置额度与窗口"
        } else {
            "等待账号周期观测确认"
        };
        sqlx::query(
            "update car_quota_cycles set prediction_reason = $2 where account_group_id = $1",
        )
        .bind(&draft.group_id)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    }

    let group_id =
        AccountGroupId::new(draft.group_id.clone()).map_err(|_| invalid("分组 ID 无效"))?;
    for seat in &draft.seats {
        let seat_id = SeatId::new(seat.id.clone()).map_err(|_| invalid("车位 ID 无效"))?;
        save_seat_in_transaction(
            &mut tx,
            &SaveSeat {
                id: Some(seat_id.clone()),
                group_id: group_id.clone(),
                name: seat.name.clone(),
                enabled: seat.enabled,
                max_concurrency: seat.max_concurrency,
                requests_per_minute: seat.requests_per_minute,
                weight: seat.weight.parse().map_err(|_| invalid("份额无效"))?,
                limits: ClientBudgetLimits {
                    daily_usd: seat
                        .daily_limit_usd
                        .parse()
                        .map_err(|_| invalid("日额度无效"))?,
                    weekly_usd: seat
                        .weekly_limit_usd
                        .parse()
                        .map_err(|_| invalid("周期额度无效"))?,
                },
            },
        )
        .await?;
        let members = seat
            .keys
            .iter()
            .filter(|key| !key.create)
            .map(|key| ClientApiKeyId::new(key.id.clone()).map_err(|_| invalid("Key ID 无效")))
            .collect::<AdminStoreResult<Vec<_>>>()?;
        if !members.is_empty() {
            join_seat_in_transaction(
                &mut tx,
                &JoinSeat {
                    seat_id,
                    key_ids: members,
                },
            )
            .await?;
        }
        for key in &seat.keys {
            if key.create {
                let prepared = command
                    .new_keys
                    .iter()
                    .find(|item| item.id.as_str() == key.id)
                    .ok_or_else(|| invalid("新 Key 凭据未准备"))?;
                insert_client_api_key_in_transaction(
                    &mut tx,
                    &NewClientApiKey {
                        id: key.id.clone(),
                        seat_id: Some(seat.id.clone()),
                        name: key.name.clone(),
                        label: key.label.clone(),
                        key: prepared.plaintext.clone(),
                        group_ids: Vec::new(),
                        max_concurrency: 0,
                        requests_per_minute: 0,
                        budget: ClientBudgetLimits::default(),
                        openai_client_profile_override: prepared
                            .openai_client_profile_override
                            .clone(),
                        xai_client_profile_override: prepared.xai_client_profile_override.clone(),
                    },
                )
                .await
                .map_err(|error| crate::admin_store_error("car key", error))?;
            } else {
                update_client_api_key_in_transaction(
                    &mut tx,
                    &UpdateClientApiKeyDetails {
                        id: key.id.clone(),
                        name: key.name.clone(),
                        label: key.label.clone(),
                        group_ids: Vec::new(),
                        max_concurrency: 0,
                        requests_per_minute: 0,
                        daily_limit_usd: None,
                        weekly_limit_usd: None,
                        openai_client_profile_override: Some(
                            key.openai_client_profile_override
                                .clone()
                                .map(OpaqueProviderData::new),
                        ),
                        xai_client_profile_override: Some(
                            key.xai_client_profile_override
                                .clone()
                                .map(OpaqueProviderData::new),
                        ),
                    },
                )
                .await
                .map_err(|error| crate::admin_store_error("car key", error))?;
            }
            sqlx::query("update client_api_keys set enabled = $2 and not $3,
                revoked_at = case when $3 then now() else revoked_at end, updated_at = now() where id = $1")
                .bind(&key.id).bind(key.enabled).bind(key.revoke).execute(&mut *tx).await.map_err(database)?;
        }
    }
    allocate(&mut tx, &draft.group_id).await?;
    sqlx::query("update car_quota_cycles set updated_at = now() where account_group_id = $1")
        .bind(&draft.group_id)
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    let result = CarManagementResult {
        config_revision: revision.get(),
        group_id: draft.group_id.clone(),
        created_key_ids: command
            .new_keys
            .iter()
            .map(|key| key.id.as_str().to_owned())
            .collect(),
    };
    let json = serde_json::to_value(&result).map_err(|_| invalid("保存回执无效"))?;
    sqlx::query("insert into car_management_receipts (request_id, fingerprint, result) values ($1::text::uuid, $2, $3)")
        .bind(&draft.request_id).bind(&command.fingerprint).bind(json).execute(&mut *tx).await.map_err(database)?;
    super::append_admin_audit_event_in_transaction(
        &mut tx,
        crate::mutation_audit(
            context,
            "save_car",
            "account_group",
            &draft.group_id,
            vec!["car_configuration".to_owned()],
        ),
        revision,
    )
    .await
    .map_err(|error| crate::admin_store_error("car", error))?;
    tx.commit().await.map_err(database)?;
    Ok(result)
}

pub(super) async fn allocate(
    tx: &mut Transaction<'_, Postgres>,
    group_id: &str,
) -> AdminStoreResult<()> {
    sqlx::query("update seats s set weekly_limit_usd = trunc(c.published_capacity_usd *
        (case when g.car_allocation = 'equal' then 1::numeric / (select count(*) from seats where account_group_id = g.id)
        else s.weight / g.car_total_weight end), 10), updated_at = now()
        from account_groups g, car_quota_cycles c where s.account_group_id = $1
        and g.id = s.account_group_id and c.account_group_id = g.id and g.car_quota_policy = 'automatic'")
        .bind(group_id).execute(&mut **tx).await.map_err(database)?;
    let zero: bool = sqlx::query_scalar("select exists(select 1 from seats s join account_groups g on g.id = s.account_group_id where g.id = $1 and g.car_quota_policy = 'automatic' and s.weekly_limit_usd = 0)")
        .bind(group_id).fetch_one(&mut **tx).await.map_err(database)?;
    if zero {
        return Err(invalid(
            "分配后的额度小于金额精度，请提高总额度或车位份额，不能保存为不限",
        ));
    }
    Ok(())
}

async fn restore_manual_windows(
    tx: &mut Transaction<'_, Postgres>,
    group_id: &str,
) -> AdminStoreResult<()> {
    // 只有账本能够解释当前已用量时才切换，避免用新窗口掩盖历史缺口。
    let incomplete: bool = sqlx::query_scalar("select exists(select 1 from seat_budget_windows w
        join seats s on s.id = w.seat_id where s.account_group_id = $1 and w.weekly_used_usd <>
        coalesce((select sum(e.amount_usd) from client_key_charge_events e join client_api_keys k on k.id = e.client_api_key_id
        where k.seat_id = s.id and e.completed_at >= w.weekly_start and e.completed_at < w.weekly_end), 0))")
        .bind(group_id).fetch_one(&mut **tx).await.map_err(database)?;
    let running: bool = sqlx::query_scalar(
        "select exists(select 1 from client_budget_admissions a
        join client_api_keys k on k.id = a.client_api_key_id join seats s on s.id = k.seat_id
        where s.account_group_id = $1 and a.expires_at > now())
        or exists(select 1 from model_requests r join client_api_keys k on k.id = r.client_api_key_id
        join seats s on s.id = k.seat_id where s.account_group_id = $1 and r.outcome = 'running')",
    )
    .bind(group_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database)?;
    if incomplete || running {
        return Err(invalid(
            "当前账务尚不能安全切换窗口，请保留跟随账号周期模式",
        ));
    }
    sqlx::query("update seat_budget_windows w set weekly_start = p.starts_at, weekly_end = p.starts_at + interval '168 hours',
        weekly_used_usd = coalesce((select sum(e.amount_usd) from client_key_charge_events e
            join client_api_keys k on k.id = e.client_api_key_id where k.seat_id = w.seat_id
            and e.completed_at >= p.starts_at and e.completed_at < p.starts_at + interval '168 hours'), 0)
        from (select w2.seat_id, w2.weekly_start + floor(greatest(extract(epoch from now() - w2.weekly_start), 0) / 604800)
            * interval '168 hours' as starts_at from seat_budget_windows w2 join seats s on s.id = w2.seat_id
            where s.account_group_id = $1) p where w.seat_id = p.seat_id")
        .bind(group_id).execute(&mut **tx).await.map_err(database)?;
    // 下次重新启用跟随时重新确认周期，不复用停用期间的旧观测。
    sqlx::query(
        "update car_quota_cycles set window_key = null, cycle_start = null, cycle_end = null,
        last_observed_at = null, last_used_percent = null where account_group_id = $1",
    )
    .bind(group_id)
    .execute(&mut **tx)
    .await
    .map_err(database)?;
    Ok(())
}
