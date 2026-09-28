-- 来源 Key 保留结算责任；解绑不回收来源，也不迁移既有消费。
alter table client_api_keys add column limit_anchor boolean not null default false;

create table client_key_limit_bindings (
    client_api_key_id text primary key references client_api_keys(id) on delete cascade,
    source_key_id text references client_api_keys(id) on delete restrict,
    revision bigint not null check (revision > 0),
    config_revision bigint not null check (config_revision > 0),
    last_operation jsonb not null,
    check (source_key_id is null or source_key_id <> client_api_key_id)
);
create index client_key_limit_bindings_source on client_key_limit_bindings(source_key_id);

create function retain_native_limit_anchor() returns trigger language plpgsql as $$
begin
    if old.limit_anchor then
        raise exception 'native limit anchors retain settlement responsibility' using errcode = '23514';
    end if;
    return old;
end;
$$;
create trigger retain_native_limit_anchor before delete on client_api_keys
    for each row execute function retain_native_limit_anchor();

-- 历史请求不猜测是否取得过独立准入；启用共享前须排空旧执行及 RPM 窗口。
alter table model_requests add column limit_source_key_ref text;
alter table model_requests add column client_admission_acquired boolean not null default false;
update model_requests set limit_source_key_ref = client_api_key_ref;
alter table model_requests alter column limit_source_key_ref set not null;

alter table client_key_charge_events add column client_key_ref text;
update client_key_charge_events set client_key_ref = client_api_key_id;
alter table client_key_charge_events alter column client_key_ref set not null;
