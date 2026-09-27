//! 插件 Client Key 目录；复用管理服务并收窄为非秘密投影。

use std::sync::Arc;

use async_trait::async_trait;
use gateway_core::policy::ClientApiKeyId;

use crate::{
    model::{
        AdminError,
        client_keys::{
            ClientKeyCursor, ClientKeyCursorValue, ClientKeyListQuery, ClientKeyPageSize,
            ClientKeySort, ClientKeySortField, SortDirection,
        },
        plugin_client_keys::{
            PluginClientKey, PluginClientKeyCursor, PluginClientKeyFacts, PluginClientKeyListQuery,
            PluginClientKeyPage,
        },
    },
    ports::plugin_client_keys::PluginClientKeyAccess,
    use_case::client_keys::ClientKeyService,
};

pub(crate) struct DefaultPluginClientKeyAccess {
    service: Arc<dyn ClientKeyService>,
}

impl DefaultPluginClientKeyAccess {
    pub(crate) fn new(service: Arc<dyn ClientKeyService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl PluginClientKeyAccess for DefaultPluginClientKeyAccess {
    async fn facts(&self, id: &ClientApiKeyId) -> Result<PluginClientKeyFacts, AdminError> {
        let key = self.service.get(id).await?;
        Ok(PluginClientKeyFacts {
            id: key.id,
            enabled: key.enabled,
            group_ids: key.groups.into_iter().map(|group| group.id).collect(),
        })
    }

    async fn list(
        &self,
        query: PluginClientKeyListQuery,
    ) -> Result<PluginClientKeyPage, AdminError> {
        let sort = ClientKeySort {
            field: ClientKeySortField::Name,
            direction: SortDirection::Asc,
        };
        let page_size = ClientKeyPageSize::new(query.limit.get())
            .map_err(|_| AdminError::internal("插件 Client Key 页大小不合法"))?;
        let page = self
            .service
            .list(ClientKeyListQuery {
                cursor: query.cursor.map(|cursor| ClientKeyCursor {
                    sort,
                    value: ClientKeyCursorValue::Name(cursor.name),
                    id: cursor.id,
                }),
                page_size,
                search: None,
                sort,
            })
            .await?;
        let next_cursor = page
            .next_cursor
            .map(|cursor| match cursor.value {
                ClientKeyCursorValue::Name(name)
                    if cursor.sort.field == ClientKeySortField::Name
                        && cursor.sort.direction == SortDirection::Asc =>
                {
                    Ok(PluginClientKeyCursor {
                        name,
                        id: cursor.id,
                    })
                }
                _ => Err(AdminError::internal(
                    "Client Key 服务返回了不匹配的插件分页游标",
                )),
            })
            .transpose()?;
        Ok(PluginClientKeyPage {
            items: page
                .items
                .into_iter()
                .map(|item| PluginClientKey {
                    id: item.id,
                    name: item.name,
                    enabled: item.enabled,
                })
                .collect(),
            next_cursor,
        })
    }
}
