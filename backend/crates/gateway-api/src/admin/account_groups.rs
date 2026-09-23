//! Account group HTTP wire and fixed routes.

use crate::auth::SessionState;

use std::collections::BTreeMap;

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use gateway_admin::model::{
    PageSize,
    account_groups::{
        AccountGroupAccountSummary, AccountGroupCapacity, AccountGroupColor, AccountGroupListQuery,
        AccountGroupMutation, AccountGroupPage, AccountGroupRecord, AccountGroupUsage,
        CreateAccountGroup, DeleteAccountGroup, SetAccountGroupEnabled, UpdateAccountGroup,
    },
};
use gateway_core::routing::AccountGroupId;
use serde::{Deserialize, Serialize};

use super::{
    AdminAuth, AdminEnvelope, AdminError, AdminJson, AdminQuery, AdminResponse, PageMeta,
    WireValidationError, wire::map_admin_service_error,
};

const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListAccountGroupsQuery {
    page: Option<u32>,
    page_size: Option<u32>,
    search: Option<String>,
    enabled: Option<bool>,
}

impl ListAccountGroupsQuery {
    fn into_command(self) -> Result<AccountGroupListQuery, WireValidationError> {
        let page = self.page.unwrap_or(1);
        let page_size = self.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
        if page == 0 || page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(WireValidationError::new("page"));
        }
        let search = self
            .search
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if search
            .as_deref()
            .is_some_and(|value| value.len() > 256 || value.chars().any(char::is_control))
        {
            return Err(WireValidationError::new("search"));
        }
        Ok(AccountGroupListQuery {
            page,
            page_size: PageSize::new(
                u16::try_from(page_size).map_err(|_| WireValidationError::new("pageSize"))?,
            )
            .map_err(|_| WireValidationError::new("pageSize"))?,
            search,
            enabled: self.enabled,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateAccountGroupRequest {
    #[serde(default)]
    disable_fast: bool,
    name: String,
    description: Option<String>,
    color: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateAccountGroupRequest {
    disable_fast: Option<bool>,
    id: String,
    name: String,
    description: Option<String>,
    color: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountGroupIdRequest {
    id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountGroupView {
    is_car: bool,
    disable_fast: bool,
    id: String,
    name: String,
    description: Option<String>,
    color: String,
    enabled: bool,
    member_count: u64,
    provider_counts: BTreeMap<String, u64>,
    client_key_count: u64,
    account_summary: AccountGroupAccountSummaryView,
    capacity: AccountGroupCapacityView,
    usage: AccountGroupUsageView,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountGroupAccountSummaryView {
    available: u64,
    limited: u64,
    total: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountGroupCapacityView {
    used_slots: Option<u64>,
    total_slots: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountGroupUsageView {
    today_usd: String,
    retained_total_usd: String,
}

impl From<AccountGroupRecord> for AccountGroupView {
    fn from(record: AccountGroupRecord) -> Self {
        Self {
            is_car: record.is_car,
            id: record.id.to_string(),
            name: record.name,
            description: record.description,
            disable_fast: record.disable_fast,
            color: record.color.as_str().to_owned(),
            enabled: record.enabled,
            member_count: record.member_count,
            provider_counts: record.provider_counts,
            client_key_count: record.client_key_count,
            account_summary: account_summary_view(record.account_summary),
            capacity: capacity_view(record.capacity),
            usage: usage_view(record.usage),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

fn account_summary_view(summary: AccountGroupAccountSummary) -> AccountGroupAccountSummaryView {
    AccountGroupAccountSummaryView {
        available: summary.available,
        limited: summary.limited,
        total: summary.total,
    }
}

fn capacity_view(capacity: AccountGroupCapacity) -> AccountGroupCapacityView {
    AccountGroupCapacityView {
        used_slots: capacity.used_slots,
        total_slots: capacity.total_slots,
    }
}

fn usage_view(usage: AccountGroupUsage) -> AccountGroupUsageView {
    AccountGroupUsageView {
        today_usd: usage.today_usd.to_string(),
        retained_total_usd: usage.retained_total_usd.to_string(),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountGroupPageData {
    items: Vec<AccountGroupView>,
    page: PageMeta,
    config_revision: u64,
}

impl From<AccountGroupPage> for AccountGroupPageData {
    fn from(page: AccountGroupPage) -> Self {
        let total_pages = if page.total == 0 {
            0
        } else {
            page.total.div_ceil(u64::from(page.page_size))
        };
        Self {
            items: page.items.into_iter().map(Into::into).collect(),
            page: PageMeta::new(
                page.page,
                u32::from(page.page_size),
                page.total,
                u32::try_from(total_pages).unwrap_or(u32::MAX),
            ),
            config_revision: page.config_revision.get(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountGroupMutationData {
    id: String,
    record: Option<AccountGroupView>,
    config_revision: u64,
}

impl From<AccountGroupMutation> for AccountGroupMutationData {
    fn from(mutation: AccountGroupMutation) -> Self {
        Self {
            id: mutation.id.to_string(),
            record: mutation.record.map(Into::into),
            config_revision: mutation.config_revision.get(),
        }
    }
}

/// Construct all fixed account-group management routes.
pub fn router<S>() -> Router<S>
where
    S: SessionState + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/api/admin/account-groups", get(list::<S>))
        .route("/api/admin/account-groups/create", post(create::<S>))
        .route("/api/admin/account-groups/update", post(update::<S>))
        .route("/api/admin/account-groups/enable", post(enable::<S>))
        .route("/api/admin/account-groups/disable", post(disable::<S>))
        .route("/api/admin/account-groups/delete", post(delete::<S>))
        .route(
            "/api/admin/account-groups/convert-car",
            post(convert_car::<S>),
        )
        .route("/api/admin/seats", get(list_seats::<S>))
        .route("/api/admin/seats/save", post(save_seat::<S>))
        .route("/api/admin/seats/join", post(join_seat::<S>))
}

async fn list<S>(
    _auth: AdminAuth,
    State(state): State<S>,
    AdminQuery(query): AdminQuery<ListAccountGroupsQuery>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let result = state
        .admin_services()
        .account_groups()
        .list(query.into_command().map_err(map_wire_error)?)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(AccountGroupPageData::from(result)),
    ))
}

async fn create<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<CreateAccountGroupRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    validate_group_fields(&request.name, request.description.as_deref())?;
    mutation_response(
        StatusCode::CREATED,
        state
            .admin_services()
            .account_groups()
            .create(
                &auth.context().mutation_context(),
                CreateAccountGroup {
                    name: request.name,
                    description: request.description,
                    disable_fast: request.disable_fast,
                    color: group_color(&request.color)?,
                },
            )
            .await,
    )
}

async fn update<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<UpdateAccountGroupRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    validate_group_fields(&request.name, request.description.as_deref())?;
    mutation_response(
        StatusCode::OK,
        state
            .admin_services()
            .account_groups()
            .update(
                &auth.context().mutation_context(),
                UpdateAccountGroup {
                    id: group_id(request.id)?,
                    name: request.name,
                    description: request.description,
                    disable_fast: request.disable_fast,
                    color: group_color(&request.color)?,
                },
            )
            .await,
    )
}

async fn enable<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<AccountGroupIdRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    set_enabled(auth, state, request, true).await
}

async fn disable<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<AccountGroupIdRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    set_enabled(auth, state, request, false).await
}

async fn set_enabled<S>(
    auth: AdminAuth,
    state: S,
    request: AccountGroupIdRequest,
    enabled: bool,
) -> Result<AdminResponse<AdminEnvelope<AccountGroupMutationData>>, AdminError>
where
    S: SessionState + Send + Sync,
{
    mutation_response(
        StatusCode::OK,
        state
            .admin_services()
            .account_groups()
            .set_enabled(
                &auth.context().mutation_context(),
                SetAccountGroupEnabled {
                    id: group_id(request.id)?,
                    enabled,
                },
            )
            .await,
    )
}

async fn delete<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<AccountGroupIdRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    mutation_response(
        StatusCode::OK,
        state
            .admin_services()
            .account_groups()
            .delete(
                &auth.context().mutation_context(),
                DeleteAccountGroup {
                    id: group_id(request.id)?,
                },
            )
            .await,
    )
}

fn mutation_response(
    status: StatusCode,
    result: Result<AccountGroupMutation, gateway_admin::model::AdminError>,
) -> Result<AdminResponse<AdminEnvelope<AccountGroupMutationData>>, AdminError> {
    let result = result.map_err(map_service_error)?;
    Ok(AdminResponse::new(
        status,
        AdminEnvelope::ok(AccountGroupMutationData::from(result)),
    ))
}

fn group_id(value: String) -> Result<AccountGroupId, AdminError> {
    AccountGroupId::new(value).map_err(|_| AdminError::bad_request("账号组 ID 不合法"))
}

fn group_color(value: &str) -> Result<AccountGroupColor, AdminError> {
    AccountGroupColor::parse(value).ok_or_else(|| AdminError::bad_request("账号组颜色不合法"))
}

fn validate_group_fields(name: &str, description: Option<&str>) -> Result<(), AdminError> {
    if name.trim() != name
        || name.is_empty()
        || name.chars().count() > 100
        || name.chars().any(char::is_control)
        || description
            .is_some_and(|value| value.len() > 4096 || value.chars().any(char::is_control))
    {
        return Err(AdminError::bad_request("账号组请求不合法"));
    }
    Ok(())
}

fn map_wire_error(_: WireValidationError) -> AdminError {
    AdminError::bad_request("账号组查询参数不合法")
}

fn map_service_error(error: gateway_admin::model::AdminError) -> AdminError {
    map_admin_service_error(error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SeatQuery {
    group_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveSeatRequest {
    id: Option<String>,
    group_id: String,
    name: String,
    enabled: bool,
    max_concurrency: u64,
    daily_limit_usd: String,
    weekly_limit_usd: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JoinSeatRequest {
    seat_id: String,
    key_ids: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeatView {
    id: String,
    group_id: String,
    name: String,
    enabled: bool,
    max_concurrency: u64,
    key_count: u64,
    daily_limit_usd: String,
    weekly_limit_usd: String,
    daily_used_usd: String,
    weekly_used_usd: String,
    daily_resets_at: Option<DateTime<Utc>>,
    weekly_resets_at: Option<DateTime<Utc>>,
}

impl From<gateway_admin::model::account_groups::SeatRecord> for SeatView {
    fn from(s: gateway_admin::model::account_groups::SeatRecord) -> Self {
        Self {
            id: s.id.as_str().to_owned(),
            group_id: s.group_id.to_string(),
            name: s.name,
            enabled: s.enabled,
            max_concurrency: s.max_concurrency,
            key_count: s.key_count,
            daily_limit_usd: s.budget.limits.daily_usd.canonical(),
            weekly_limit_usd: s.budget.limits.weekly_usd.canonical(),
            daily_used_usd: s.budget.daily_used_usd.canonical(),
            weekly_used_usd: s.budget.weekly_used_usd.canonical(),
            daily_resets_at: s.budget.daily_resets_at.map(DateTime::from),
            weekly_resets_at: s.budget.weekly_resets_at.map(DateTime::from),
        }
    }
}

async fn convert_car<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<AccountGroupIdRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let revision = state
        .admin_services()
        .account_groups()
        .convert_to_car(&auth.context().mutation_context(), group_id(request.id)?)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(serde_json::json!({"configRevision": revision.get()})),
    ))
}

async fn list_seats<S>(
    _auth: AdminAuth,
    State(state): State<S>,
    AdminQuery(query): AdminQuery<SeatQuery>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let seats = state
        .admin_services()
        .account_groups()
        .seats(group_id(query.group_id)?)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(seats.into_iter().map(SeatView::from).collect::<Vec<_>>()),
    ))
}

async fn save_seat<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<SaveSeatRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let command = gateway_admin::model::account_groups::SaveSeat {
        id: request
            .id
            .map(gateway_core::policy::SeatId::new)
            .transpose()
            .map_err(|_| AdminError::bad_request("seat ID 无效"))?,
        group_id: group_id(request.group_id)?,
        name: request.name,
        enabled: request.enabled,
        max_concurrency: request.max_concurrency,
        limits: gateway_core::engine::budget::ClientBudgetLimits {
            daily_usd: request
                .daily_limit_usd
                .parse()
                .map_err(|_| AdminError::bad_request("日限额无效"))?,
            weekly_usd: request
                .weekly_limit_usd
                .parse()
                .map_err(|_| AdminError::bad_request("周限额无效"))?,
        },
    };
    let revision = state
        .admin_services()
        .account_groups()
        .save_seat(&auth.context().mutation_context(), command)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(serde_json::json!({"configRevision": revision.get()})),
    ))
}

async fn join_seat<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<JoinSeatRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let command = gateway_admin::model::account_groups::JoinSeat {
        seat_id: gateway_core::policy::SeatId::new(request.seat_id)
            .map_err(|_| AdminError::bad_request("seat ID 无效"))?,
        key_ids: request
            .key_ids
            .into_iter()
            .map(gateway_core::policy::ClientApiKeyId::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AdminError::bad_request("Key ID 无效"))?,
    };
    let revision = state
        .admin_services()
        .account_groups()
        .join_seat(&auth.context().mutation_context(), command)
        .await
        .map_err(map_service_error)?;
    Ok(AdminResponse::new(
        StatusCode::OK,
        AdminEnvelope::ok(serde_json::json!({"configRevision": revision.get()})),
    ))
}
