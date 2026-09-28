use std::collections::BTreeMap;

use gateway_admin::model::AdminError;
use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::upstream_adapter::{UpstreamAdapterDeclaration, UpstreamPathPurpose},
};
use url::Url;

pub(crate) struct UpstreamTarget {
    base: Url,
    paths: BTreeMap<String, UpstreamPathPurpose>,
}

impl UpstreamTarget {
    pub(super) fn compile(declaration: &UpstreamAdapterDeclaration) -> Result<Self, AdminError> {
        let base = Url::parse(&declaration.base_url)
            .map_err(|_| AdminError::invalid("上游 Base URL 无效"))?;
        if !matches!(base.scheme(), "https" | "http")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
            || declaration.base_url.len() > 2048
            || declaration.paths.is_empty()
            || declaration.paths.len() > 16
        {
            return Err(AdminError::invalid(
                "上游 Base URL 须为无凭据、查询串或片段且以 / 结尾的 HTTP(S) 地址",
            ));
        }
        let mut paths = BTreeMap::new();
        for path in &declaration.paths {
            if !valid_path(&path.path) || paths.insert(path.path.clone(), path.purpose).is_some() {
                return Err(AdminError::invalid("上游业务路径无效或重复"));
            }
        }
        if !paths
            .values()
            .any(|purpose| *purpose == UpstreamPathPurpose::Inference)
        {
            return Err(AdminError::invalid("上游适配器须声明推理路径"));
        }
        Ok(Self { base, paths })
    }

    pub(crate) fn resolve(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<(Url, UpstreamPathPurpose), PluginFault> {
        let purpose = *self.paths.get(path).ok_or_else(invalid)?;
        if query.len() > 64
            || query
                .iter()
                .any(|(key, value)| key.len() > 256 || value.len() > 4096)
        {
            return Err(invalid());
        }
        let mut target = self.base.join(path).map_err(|_| invalid())?;
        if target.origin() != self.base.origin() || !target.path().starts_with(self.base.path()) {
            return Err(invalid());
        }
        if !query.is_empty() {
            target
                .query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (key, value)));
        }
        Ok((target, purpose))
    }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 2048
        && path.is_ascii()
        && !path.starts_with('/')
        && path.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~')
        })
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

pub(crate) fn protected_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "host"
            | "connection"
            | "upgrade"
            | "transfer-encoding"
            | "content-length"
            | "proxy-connection"
            | "te"
            | "trailer"
    ) || name.starts_with("sec-websocket-")
}

fn invalid() -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, "upstream target is not authorized")
}
