//! 单页 Key 用量的查询合同；身份范围不由调用方提供。

use super::{
    PageSize,
    client_keys::ClientKeyRecord,
    observability::{
        HealthTimeline, OpsErrorPage, RequestMetricPoint, TimeRange, UsageOverview, UsagePage,
    },
};
use gateway_core::{metering::Decimal, policy::ClientApiKeyId};

#[derive(Debug, Clone)]
pub struct KeyUsageQuery {
    pub range: TimeRange,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum KeyUsageRecordKind {
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct KeyUsageRecordsQuery {
    pub usage: KeyUsageQuery,
    pub kind: KeyUsageRecordKind,
    pub current_page: u32,
    pub page_size: PageSize,
}

pub struct KeyUsageOverview {
    pub key: ClientKeyRecord,
    pub seat_keys: Vec<SeatKeyUsage>,
    pub overview: UsageOverview,
    pub trend: Vec<RequestMetricPoint>,
    pub health_timeline: HealthTimeline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatKeyUsage {
    pub id: ClientApiKeyId,
    pub name: String,
    pub prefix: String,
    pub daily_used_usd: Decimal,
    pub cycle_used_usd: Decimal,
}

pub enum KeyUsageRecords {
    Success(UsagePage),
    Error(OpsErrorPage),
}
