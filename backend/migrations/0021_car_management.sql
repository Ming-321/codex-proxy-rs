-- 旧部署保留已启用的周期策略，新建分组默认不接管手动额度。
alter table account_groups
    add column car_quota_policy text not null default 'manual'
        check (car_quota_policy in ('manual', 'cycle', 'automatic')),
    add column car_allocation text not null default 'custom'
        check (car_allocation in ('custom', 'equal'));

update account_groups set car_quota_policy = case
    when (select automatic_updates from car_quota_settings where singleton) then 'automatic'
    else 'cycle' end
where is_car and car_quota_mode <> 'legacy';

alter table seats add column requests_per_minute bigint not null default 0
    check (requests_per_minute between 0 and 4294967295);

-- 一次编辑可以交换两个车位的名称，唯一性按保存后的最终状态判断。
alter table seats drop constraint seats_account_group_id_name_key;
alter table seats add constraint seats_account_group_id_name_key
    unique (account_group_id, name) deferrable initially deferred;

-- 共享 RPM 承接已有成员中最严格的有限设置，空车位保持不限。
update seats s set requests_per_minute = coalesce((
    select min(k.requests_per_minute) filter (where k.requests_per_minute > 0)
    from client_api_keys k where k.seat_id = s.id and k.revoked_at is null
), 0);

alter table car_quota_settings alter column automatic_updates set default false;
-- 全局开关只作为新建时的建议，实际策略由每辆车保存。
update car_quota_settings set automatic_updates = false
where not exists (select 1 from account_groups where is_car);

-- 均分按车位数量计算比例，避免将 1/3 舍入为三个 6.7。
create or replace function validate_car_weights() returns trigger language plpgsql as $$
begin
    if exists (
        select 1 from account_groups g
        cross join lateral (select coalesce(sum(s.weight), 0) as allocated
            from seats s where s.account_group_id = g.id) allocation
        where g.is_car and g.car_allocation = 'custom'
          and allocation.allocated > g.car_total_weight
    ) then
        raise check_violation using message = 'seat weights exceed car total weight';
    end if;
    return null;
end;
$$;

-- 只记录保存回执和配置指纹，不保存明文凭据；重试可恢复同一次结果。
create table car_management_receipts (
    request_id uuid primary key,
    fingerprint text not null,
    result jsonb not null,
    created_at timestamptz not null default now()
);
