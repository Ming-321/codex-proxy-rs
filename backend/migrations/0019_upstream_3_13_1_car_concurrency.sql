-- 自用 fork 保留已发布的0017/0018，只追加上游不限并发兼容。
alter table runtime_settings
    drop constraint runtime_settings_refresh_ck,
    add constraint runtime_settings_refresh_ck check (
        refresh_margin_seconds > 0 and refresh_concurrency > 0
        and max_concurrent_per_account >= 0 and request_interval_ms >= 0
    );

-- 迁移预检与后续配置提交共用约束，包含没有 seat 的 car。
create function check_car_seat_relations() returns void language plpgsql as $$
declare
    invalid_car text;
begin
    select g.id into invalid_car from account_groups g
    left join account_group_accounts a on a.account_group_id = g.id
    left join provider_accounts p on p.id = a.provider_account_id
    cross join runtime_settings r
    where g.is_car and coalesce(p.concurrency_limit, r.max_concurrent_per_account) <= 0
    order by g.id limit 1;
    if invalid_car is not null then
        raise check_violation using constraint = 'car_finite_capacity',
            message = 'car requires a positive effective concurrency limit: ' || invalid_car,
            hint = 'Set a positive account concurrency override before using an unlimited global default.';
    end if;
    if exists (
        select 1 from account_groups g
        left join account_group_accounts a on a.account_group_id = g.id
        where g.is_car group by g.id having count(a.provider_account_id) <> 1
    ) or exists (
        select 1 from account_group_accounts a
        join account_groups g on g.id = a.account_group_id and g.is_car
        join account_group_accounts other on other.provider_account_id = a.provider_account_id
            and other.account_group_id <> a.account_group_id
    ) then
        raise check_violation using message = 'car must exclusively own exactly one account';
    end if;
    if exists (
        select 1 from seats s join account_groups g on g.id = s.account_group_id
        left join account_group_accounts a on a.account_group_id = g.id
        left join provider_accounts p on p.id = a.provider_account_id
        cross join runtime_settings r
        where not g.is_car or s.max_concurrency > coalesce(p.concurrency_limit, r.max_concurrent_per_account)
    ) or exists (
        select 1 from seats s join account_group_accounts a on a.account_group_id = s.account_group_id
        join provider_accounts p on p.id = a.provider_account_id cross join runtime_settings r
        group by s.account_group_id, p.concurrency_limit, r.max_concurrent_per_account
        having count(*) > coalesce(p.concurrency_limit, r.max_concurrent_per_account)
    ) then
        raise check_violation using constraint = 'car_seat_capacity', message = 'seat count and concurrency must fit the car account';
    end if;
    if exists (
        select 1 from client_api_keys k join seats s on s.id = k.seat_id
        where exists (select 1 from client_api_key_groups kg where kg.client_api_key_id = k.id)
    ) then
        raise check_violation using message = 'seat keys inherit their car and cannot bind groups';
    end if;
end;
$$;

create or replace function validate_car_seat_relations() returns trigger language plpgsql as $$
begin
    perform check_car_seat_relations();
    return null;
end;
$$;

select check_car_seat_relations();
