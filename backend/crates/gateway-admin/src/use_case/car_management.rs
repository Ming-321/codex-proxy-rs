//! 完整拼车草稿的输入约束、身份校验及新凭据准备。

use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gateway_core::{
    account::{OpaqueProviderData, ProviderAccountId},
    engine::budget::ClientBudgetLimits,
    metering::Decimal,
    policy::{ClientApiKeyId, RateLimits, SeatId},
    routing::{AccountGroupId, ProviderKind},
};
use rand_core::{OsRng, RngCore as _};
use sha2::{Digest as _, Sha256};

use crate::{
    model::{
        AdminError,
        account_groups::{AccountGroupColor, CarManagementDraft, PreparedCarManagement},
        client_keys::NewClientKey,
    },
    ports::provider::ProviderAdminRegistry,
};

fn invalid() -> AdminError {
    AdminError::invalid("拼车配置无效，请检查名称、份额、额度及共享限制")
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.chars().count() <= 100
        && !value.chars().any(char::is_control)
}

fn amount(value: &str, weight: bool) -> Result<Decimal, AdminError> {
    let value: Decimal = value.parse().map_err(|_| invalid())?;
    let text = value.canonical();
    if text.starts_with('-')
        || (weight
            && (value == Decimal::ZERO
                || text
                    .split_once('.')
                    .is_some_and(|(_, fraction)| fraction.len() > 1)))
    {
        return Err(invalid());
    }
    Ok(value)
}

pub(super) fn prepare(
    draft: CarManagementDraft,
    providers: &ProviderAdminRegistry,
) -> Result<PreparedCarManagement, AdminError> {
    uuid::Uuid::parse_str(&draft.request_id).map_err(|_| invalid())?;
    AccountGroupId::new(draft.group_id.clone()).map_err(|_| invalid())?;
    ProviderAccountId::new(draft.account_id.clone()).map_err(|_| invalid())?;
    if !valid_name(&draft.name)
        || AccountGroupColor::parse(&draft.color).is_none()
        || draft
            .description
            .as_ref()
            .is_some_and(|value| value.chars().count() > 500)
        || !matches!(
            draft.quota_policy.as_str(),
            "manual" | "cycle" | "automatic"
        )
        || !matches!(draft.allocation.as_str(), "custom" | "equal")
        || draft.seats.is_empty()
        || draft.seats.len() > 100
        || draft
            .seats
            .iter()
            .map(|seat| seat.keys.len())
            .sum::<usize>()
            > 500
    {
        return Err(invalid());
    }
    amount(&draft.total_weight, true)?;
    if let Some(capacity) = &draft.initial_capacity_usd
        && amount(capacity, false)? == Decimal::ZERO
    {
        return Err(AdminError::invalid("初始总额度必须大于零"));
    }
    let bytes = serde_json::to_vec(&draft).map_err(|_| invalid())?;
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut new_keys = Vec::new();
    for seat in &draft.seats {
        let seat_id = SeatId::new(seat.id.clone()).map_err(|_| invalid())?;
        if !ids.insert(seat.id.clone())
            || !names.insert(seat.name.to_lowercase())
            || !valid_name(&seat.name)
            || seat.max_concurrency == 0
            || seat.max_concurrency > u64::from(u32::MAX)
            || seat.requests_per_minute > u64::from(u32::MAX)
        {
            return Err(invalid());
        }
        amount(&seat.weight, true)?;
        amount(&seat.daily_limit_usd, false)?;
        amount(&seat.weekly_limit_usd, false)?;
        for key in &seat.keys {
            let id = ClientApiKeyId::new(key.id.clone()).map_err(|_| invalid())?;
            if !ids.insert(key.id.clone()) || !valid_name(&key.name) || (key.create && key.revoke) {
                return Err(invalid());
            }
            for (provider, profile) in [
                ("openai", &key.openai_client_profile_override),
                ("xai", &key.xai_client_profile_override),
            ] {
                if let Some(profile) = profile {
                    let kind = ProviderKind::new(provider).map_err(|_| invalid())?;
                    providers
                        .require(&kind)
                        .and_then(|provider| {
                            provider
                                .preview_client_profile(&OpaqueProviderData::new(profile.clone()))
                        })
                        .map_err(|error| super::map_provider_error(error, "client profile"))?;
                }
            }
            if key.create {
                let mut bytes = [0_u8; 32];
                OsRng.fill_bytes(&mut bytes);
                new_keys.push(NewClientKey {
                    id,
                    seat_id: Some(seat_id.clone()),
                    name: key.name.clone(),
                    label: key.label.clone(),
                    group_ids: Vec::new(),
                    limits: RateLimits::default(),
                    budget: ClientBudgetLimits::default(),
                    plaintext: format!("sk_{}", URL_SAFE_NO_PAD.encode(bytes)),
                    openai_client_profile_override: key
                        .openai_client_profile_override
                        .clone()
                        .map(OpaqueProviderData::new),
                    xai_client_profile_override: key
                        .xai_client_profile_override
                        .clone()
                        .map(OpaqueProviderData::new),
                });
            }
        }
    }
    Ok(PreparedCarManagement {
        draft,
        fingerprint,
        new_keys,
    })
}
