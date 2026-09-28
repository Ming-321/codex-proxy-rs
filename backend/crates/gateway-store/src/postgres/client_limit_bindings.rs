//! 单层原生限额关系；配置锁串行化关系修改，Key 行锁与预算重置协调。

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        MutationActor, MutationContext,
        client_keys::{
            ChangeClientLimitBinding, ClientLimitBinding, ClientLimitBindingMutationOrigin,
        },
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
};
use gateway_core::{
    engine::budget::{ClientBudgetLimits, ClientBudgetStatus},
    policy::{ClientApiKeyId, RateLimits},
};
use serde_json::json;
use sqlx::{PgConnection, PgPool, Row};

use crate::{admin_store_error, mutation_audit, postgres_unavailable};

const RESOURCE: &str = "client limit binding";

fn unavailable(_: sqlx::Error) -> AdminStoreError {
    admin_store_error(RESOURCE, postgres_unavailable("native limit binding"))
}

fn conflict(message: &str) -> AdminStoreError {
    AdminStoreError::new(AdminStoreErrorKind::Conflict, RESOURCE, message)
}

pub(super) async fn get(
    pool: &PgPool,
    id: &ClientApiKeyId,
) -> AdminStoreResult<ClientLimitBinding> {
    let mut connection = pool.acquire().await.map_err(unavailable)?;
    read(&mut connection, id).await
}

async fn read(
    connection: &mut PgConnection,
    id: &ClientApiKeyId,
) -> AdminStoreResult<ClientLimitBinding> {
    let row = sqlx::query(
        "select coalesce(b.source_key_id,k.id) as source_id, coalesce(b.revision,0) as revision,
        r.config_revision, b.config_revision as binding_config_revision, x.enabled, x.max_concurrency, x.requests_per_minute,
        x.daily_limit_usd::text, x.weekly_limit_usd::text,
        (case when w.daily_end>now() then w.daily_used_usd else 0 end)::text as daily_used,
        (case when w.weekly_end>now() then w.weekly_used_usd else 0 end)::text as weekly_used,
        case when w.daily_end>now() then w.daily_end end as daily_end,
        case when w.weekly_end>now() then w.weekly_end end as weekly_end
        from client_api_keys k
        left join client_key_limit_bindings b on b.client_api_key_id=k.id
        join client_api_keys x on x.id=coalesce(b.source_key_id,k.id)
        left join client_key_budget_windows w on w.client_api_key_id=x.id
        cross join runtime_settings r where k.id=$1 and r.id=1",
    )
    .bind(id.as_str())
    .fetch_optional(connection)
    .await
    .map_err(unavailable)?
    .ok_or_else(|| {
        AdminStoreError::new(
            AdminStoreErrorKind::NotFound,
            RESOURCE,
            "Client API Key 不存在",
        )
    })?;
    let invalid = || {
        AdminStoreError::new(
            AdminStoreErrorKind::Unavailable,
            RESOURCE,
            "invalid persisted limit facts",
        )
    };
    let number = |name| u64::try_from(row.get::<i64, _>(name)).map_err(|_| invalid());
    let decimal = |name| row.get::<String, _>(name).parse().map_err(|_| invalid());
    Ok(ClientLimitBinding {
        id: id.clone(),
        source_key_id: ClientApiKeyId::new(row.get::<String, _>("source_id"))
            .map_err(|_| invalid())?,
        revision: number("revision")?,
        config_revision: gateway_admin::model::Revision::new(number("config_revision")?)
            .map_err(|_| invalid())?,
        binding_config_revision: row
            .get::<Option<i64>, _>("binding_config_revision")
            .map(u64::try_from)
            .transpose()
            .map_err(|_| invalid())?,
        loaded_config_revision: None,
        source_enabled: row.get("enabled"),
        limits: RateLimits {
            max_concurrency: number("max_concurrency")?,
            requests_per_minute: number("requests_per_minute")?,
        },
        budget: ClientBudgetStatus {
            limits: ClientBudgetLimits {
                daily_usd: decimal("daily_limit_usd")?,
                weekly_usd: decimal("weekly_limit_usd")?,
            },
            daily_used_usd: decimal("daily_used")?,
            weekly_used_usd: decimal("weekly_used")?,
            daily_resets_at: row
                .get::<Option<DateTime<Utc>>, _>("daily_end")
                .map(Into::into),
            weekly_resets_at: row
                .get::<Option<DateTime<Utc>>, _>("weekly_end")
                .map(Into::into),
        },
    })
}

pub(super) async fn change(
    pool: &PgPool,
    command: ChangeClientLimitBinding,
    context: &MutationContext,
    origin: ClientLimitBindingMutationOrigin,
) -> AdminStoreResult<ClientLimitBinding> {
    if command.source_key_id.as_ref() == Some(&command.id) {
        return Err(conflict("解绑请使用 null 来源；不允许自引用"));
    }
    let actor = match (&origin, &context.actor) {
        (ClientLimitBindingMutationOrigin::Plugin(owner), _) => json!({
            "pluginInstance": owner.instance_id,
            "instanceRevision": owner.revision.get(),
            "artifactSha256": owner.artifact_sha256,
        }),
        (_, MutationActor::AdminSession { admin_user_id }) => json!({"adminSession":admin_user_id}),
        (_, MutationActor::AdminApiKey) => json!({"adminApiKey":true}),
        (_, MutationActor::System) => json!({"system":true}),
    };
    let operation = json!({"actor":actor,"expectedRevision":command.expected_revision,"sourceKeyId":command.source_key_id.as_ref().map(ClientApiKeyId::as_str)});
    // 重试也先复验当前实例授权，不能用已提交操作绕过撤权。
    let mut tx = match &origin {
        ClientLimitBindingMutationOrigin::Admin => {
            let mut tx = pool.begin().await.map_err(unavailable)?;
            sqlx::query("select config_revision from runtime_settings where id=1 for update")
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
            tx
        }
        ClientLimitBindingMutationOrigin::Plugin(owner) => {
            super::plugins::begin_authorized_mutation(pool, owner, "key_limit_bindings").await?
        }
    };
    let ids = vec![
        command.id.as_str(),
        command
            .source_key_id
            .as_ref()
            .unwrap_or(&command.id)
            .as_str(),
    ];
    let keys: Vec<(String, bool)> = sqlx::query_as(
        "select id,limit_anchor from client_api_keys where id=any($1) order by id for update",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(unavailable)?;
    for id in &ids {
        if !keys.iter().any(|(found, _)| found == id) {
            return Err(AdminStoreError::new(
                AdminStoreErrorKind::NotFound,
                RESOURCE,
                "设备或限额来源不存在",
            ));
        }
    }
    let previous: Option<(Option<String>,i64,serde_json::Value)> = sqlx::query_as("select source_key_id,revision,last_operation from client_key_limit_bindings where client_api_key_id=$1")
        .bind(command.id.as_str()).fetch_optional(&mut *tx).await.map_err(unavailable)?;
    let revision = previous.as_ref().map_or(0, |(_, revision, _)| *revision);
    if previous
        .as_ref()
        .is_some_and(|(_, _, last)| *last == operation)
    {
        let binding = read(&mut tx, &command.id).await?;
        tx.commit().await.map_err(unavailable)?;
        return Ok(binding);
    }
    if u64::try_from(revision).ok() != Some(command.expected_revision) {
        return Err(AdminStoreError::new(
            AdminStoreErrorKind::StaleRevision,
            RESOURCE,
            "绑定已被修改，请重新读取",
        ));
    }
    if let Some(source) = &command.source_key_id {
        if keys
            .iter()
            .any(|(id, anchor)| id == command.id.as_str() && *anchor)
        {
            return Err(conflict("已承载限额的来源不能再绑定到其他来源"));
        }
        let chained: bool=sqlx::query_scalar("select exists(select 1 from client_key_limit_bindings where client_api_key_id=$1 and source_key_id is not null)")
            .bind(source.as_str()).fetch_one(&mut *tx).await.map_err(unavailable)?;
        if chained {
            return Err(conflict("不允许链式限额关系"));
        }
        sqlx::query("update client_api_keys set limit_anchor=true where id=$1")
            .bind(source.as_str())
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
    }
    // 即使目标未变，也记录操作版本，保证丢失响应后的重试与后续操作可区分。
    let next = revision
        .checked_add(1)
        .ok_or_else(|| conflict("绑定版本已耗尽"))?;
    let config_revision = super::bump_config_revision_in_transaction(&mut tx)
        .await
        .map_err(|error| admin_store_error(RESOURCE, error))?;
    sqlx::query("insert into client_key_limit_bindings(client_api_key_id,source_key_id,revision,config_revision,last_operation)
        values($1,$2,$3,$4,$5) on conflict(client_api_key_id) do update set source_key_id=excluded.source_key_id,
        revision=excluded.revision,config_revision=excluded.config_revision,last_operation=excluded.last_operation")
        .bind(command.id.as_str()).bind(command.source_key_id.as_ref().map(ClientApiKeyId::as_str))
        .bind(next).bind(i64::try_from(config_revision.get()).map_err(|_| conflict("配置版本已耗尽"))?).bind(operation)
        .execute(&mut *tx).await.map_err(unavailable)?;
    let mut audit = mutation_audit(
        context,
        "change_limit_binding",
        "client_api_key",
        command.id.as_str(),
        vec!["limit_source_key_id".to_owned()],
    );
    if let ClientLimitBindingMutationOrigin::Plugin(owner) = &origin {
        audit.actor_kind = super::AdminAuditActorKind::System;
        audit.actor_admin_user_id = None;
        audit.actor_ref = format!(
            "plugin:{}:revision:{}:artifact:{}",
            owner.instance_id,
            owner.revision.get(),
            owner.artifact_sha256
        );
    }
    super::append_admin_audit_event_in_transaction(&mut tx, audit, config_revision)
        .await
        .map_err(|error| admin_store_error(RESOURCE, error))?;
    let binding = read(&mut tx, &command.id).await?;
    tx.commit().await.map_err(unavailable)?;
    Ok(binding)
}
