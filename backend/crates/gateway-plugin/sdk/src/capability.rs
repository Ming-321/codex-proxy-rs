use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize, de};

/// 能力只声明可提供的行为，不等于管理员授予的回调权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    FrontendAuthentication,
    Scheduler,
    ModelRouter,
    ModelCatalog,
    RetryPolicy,
    Middleware,
    UpstreamAdapter,
    RequestLifecycle,
    WebSocketObserver,
    Usage,
    CommandLine,
    Management,
    Maintenance,
}

impl Capability {
    /// SDK 能描述的行为合同版本；具体宿主可以只开放其中一部分。
    #[must_use]
    pub const fn contract_versions(self) -> &'static [u32] {
        match self {
            Self::Middleware => &[1, 2],
            Self::FrontendAuthentication
            | Self::Scheduler
            | Self::ModelRouter
            | Self::ModelCatalog
            | Self::RetryPolicy
            | Self::RequestLifecycle
            | Self::WebSocketObserver
            | Self::Usage
            | Self::CommandLine
            | Self::Management
            | Self::Maintenance => &[1],
            Self::UpstreamAdapter => &[1],
        }
    }

    /// 稳定能力标识；默认扩展项 ID 由它派生，不受显示名称影响。
    #[must_use]
    pub const fn identifier(self) -> &'static str {
        match self {
            Self::FrontendAuthentication => "frontend_authentication",
            Self::Scheduler => "scheduler",
            Self::ModelRouter => "model_router",
            Self::ModelCatalog => "model_catalog",
            Self::RetryPolicy => "retry_policy",
            Self::Middleware => "middleware",
            Self::UpstreamAdapter => "upstream_adapter",
            Self::RequestLifecycle => "request_lifecycle",
            Self::WebSocketObserver => "web_socket_observer",
            Self::Usage => "usage",
            Self::CommandLine => "command_line",
            Self::Management => "management",
            Self::Maintenance => "maintenance",
        }
    }

    /// 固定调用阶段；只有中间件需要作者显式选择 request／attempt。
    #[must_use]
    pub const fn fixed_stages(self) -> &'static [Stage] {
        match self {
            Self::FrontendAuthentication => &[Stage::Authentication],
            Self::Scheduler => &[Stage::Scheduling],
            Self::ModelRouter => &[Stage::Routing],
            Self::ModelCatalog => &[Stage::Registration],
            Self::RetryPolicy => &[Stage::Retry],
            Self::Middleware => &[],
            Self::UpstreamAdapter => &[Stage::Upstream],
            Self::RequestLifecycle | Self::WebSocketObserver | Self::Usage => &[Stage::Observation],
            Self::CommandLine => &[Stage::CommandLine],
            Self::Management => &[Stage::Management],
            Self::Maintenance => &[Stage::Maintenance],
        }
    }
}

/// 调用阶段由宿主签发，插件不能通过方法参数提升阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Registration,
    Configuration,
    Authentication,
    Routing,
    Scheduling,
    Retry,
    Request,
    Attempt,
    /// Core 登记已选账号的 attempt 后才启动的受管上游执行。
    Upstream,
    Observation,
    Management,
    CommandLine,
    /// 公共登录回调不继承插件实例的任何宿主回调权限。
    PublicManagement,
    /// 宿主针对已发布实例签发的幂等维护调用。
    Maintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    Reject,
    Delegate,
    Observe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContributionDeclaration {
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_capability_version")]
    pub version: u32,
    #[serde(default)]
    pub stages: Vec<Stage>,
    #[serde(default)]
    pub input_formats: Vec<String>,
    #[serde(default)]
    pub output_formats: Vec<String>,
}

const fn default_capability_version() -> u32 {
    1
}

/// 插件按能力标识索引的扩展项声明；每种能力至多声明一个处理器。
pub type Contributions = BTreeMap<Capability, ContributionDeclaration>;

pub(crate) fn deserialize_contributions<'de, D>(deserializer: D) -> Result<Contributions, D::Error>
where
    D: Deserializer<'de>,
{
    struct ContributionsVisitor;

    impl<'de> de::Visitor<'de> for ContributionsVisitor {
        type Value = Contributions;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map with one declaration per plugin capability")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut contributions = Contributions::new();
            while let Some((capability, declaration)) = map.next_entry()? {
                if contributions.insert(capability, declaration).is_some() {
                    return Err(de::Error::custom("duplicate plugin capability declaration"));
                }
            }
            Ok(contributions)
        }
    }

    deserializer.deserialize_map(ContributionsVisitor)
}

/// 插件可访问的稳定资源域；动作与后续资源限制由接口合同表达，不编码在域名中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Network,
    Models,
    Accounts,
    Data,
    Requests,
    PublicEndpoints,
    Groups,
    Keys,
    KeyBudgets,
    KeyLimitBindings,
    QuotaObservations,
    QuotaForecasts,
    UpstreamConnections,
}

impl Permission {
    /// 当前公开访问域，安装摘要与授权校验复用同一集合。
    pub const ALL: [Self; 13] = [
        Self::Network,
        Self::Models,
        Self::Accounts,
        Self::Data,
        Self::Requests,
        Self::PublicEndpoints,
        Self::Groups,
        Self::Keys,
        Self::KeyBudgets,
        Self::KeyLimitBindings,
        Self::QuotaObservations,
        Self::QuotaForecasts,
        Self::UpstreamConnections,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Models => "models",
            Self::Accounts => "accounts",
            Self::Data => "data",
            Self::Requests => "requests",
            Self::PublicEndpoints => "public_endpoints",
            Self::Groups => "groups",
            Self::Keys => "keys",
            Self::KeyBudgets => "key_budgets",
            Self::KeyLimitBindings => "key_limit_bindings",
            Self::QuotaObservations => "quota_observations",
            Self::QuotaForecasts => "quota_forecasts",
            Self::UpstreamConnections => "upstream_connections",
        }
    }

    /// 安装摘要显示名。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Network => "联网",
            Self::Models => "模型调用",
            Self::Accounts => "账号与凭据",
            Self::Data => "基础数据",
            Self::Requests => "请求处理",
            Self::PublicEndpoints => "公开入口",
            Self::Groups => "专用账号分组",
            Self::Keys => "专用 API Key",
            Self::KeyBudgets => "API Key 预算",
            Self::KeyLimitBindings => "共享限额关系",
            Self::QuotaObservations => "账号额度观测",
            Self::QuotaForecasts => "账号额度预测",
            Self::UpstreamConnections => "账号上游连接",
        }
    }

    /// 面向安装确认的访问含义；账号域明确包含原始凭据和修改权限。
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Network => "访问网络",
            Self::UpstreamConnections => {
                "使用当前请求所选账号的身份与代理连接插件声明的上游，不提供原始凭据读取或账号修改"
            }
            Self::Models => "查询模型与 API Key 信息并调用模型，可能产生消耗",
            Self::Accounts => "读取和修改账号，包括访问原始凭据",
            Self::Data => {
                "在管理、命令或维护入口只读查询账号、Key 的基础信息与已有额度观测，不包含凭据或修改权限"
            }
            Self::Requests => "查看和处理请求、响应、路由及账号选择",
            Self::PublicEndpoints => "提供无需登录即可访问的资源与回调入口",
            Self::Groups => {
                "创建本插件的分组，可将所有现有及未来新增账号加入或移出这些分组，不修改其他分组"
            }
            Self::Keys => "创建绑定本插件分组的 API Key，不读取密钥明文或修改管理员创建的 Key",
            Self::QuotaObservations => {
                "查询所有现有及未来账号的额度观测，并通过宿主刷新上游观测；不读取凭据、不执行上游额度重置"
            }
            Self::QuotaForecasts => {
                "只读查询所有现有及未来账号的周额度预测及费用汇总估计；不读取凭据或明细、不刷新上游、不修改预算"
            }
            Self::KeyBudgets => {
                "查询所有现有及未来新增 Key 的预算、修改日／周金额上限、重置用量和持续接管周窗口，包括管理员和其他插件创建的 Key；不读取密钥或修改其他配置"
            }
            Self::KeyLimitBindings => {
                "读取和修改所有现有及未来 Client Key 的原生限额来源，影响预算、并发、RPM 和等待队列；不读取 Key 明文凭据，停用插件不解除共享关系"
            }
        }
    }
}
