alter table account_groups add column is_car boolean not null default false;

create table seats (
    id text primary key check (id ~ '^seat_[0-9a-f]{32}$'),
    account_group_id text not null references account_groups(id) on delete restrict,
    name text not null check (char_length(btrim(name)) between 1 and 100 and name = btrim(name)),
    enabled boolean not null default true,
    max_concurrency bigint not null check (max_concurrency between 1 and 4294967295),
    daily_limit_usd numeric(20,10) not null default 0 check (daily_limit_usd >= 0),
    weekly_limit_usd numeric(20,10) not null default 0 check (weekly_limit_usd >= 0),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (account_group_id, name)
);

alter table client_api_keys add column seat_id text references seats(id) on delete restrict;
alter table client_api_keys add column revoked_at timestamptz;
create index client_api_keys_seat_idx on client_api_keys(seat_id) where seat_id is not null;

create view client_key_effective_groups as
    select kg.client_api_key_id, kg.account_group_id from client_api_key_groups kg
    join client_api_keys k on k.id = kg.client_api_key_id where k.revoked_at is null
    union all
    select k.id, s.account_group_id from client_api_keys k join seats s on s.id = k.seat_id
    where k.revoked_at is null;

create table seat_budget_windows (
    seat_id text primary key references seats(id) on delete restrict,
    daily_start timestamptz not null,
    daily_end timestamptz not null,
    weekly_start timestamptz not null,
    weekly_end timestamptz not null,
    daily_used_usd numeric(20,10) not null default 0 check (daily_used_usd >= 0),
    weekly_used_usd numeric(20,10) not null default 0 check (weekly_used_usd >= 0)
);

alter table client_key_charge_events add column seat_id text references seats(id) on delete restrict;
alter table model_requests add column seat_id text references seats(id) on delete restrict;

-- 只保护迁入时的在途请求，不预扣额度；结算删除记录，崩溃后由期限解除保护。
create table client_budget_admissions (
    request_id text primary key,
    client_api_key_id text not null references client_api_keys(id) on delete cascade,
    expires_at timestamptz not null
);
create index client_budget_admissions_key_idx on client_budget_admissions(client_api_key_id);

-- 以事务最终状态检查共享关系，覆盖批量导入、账号编辑和并发设置等入口。
-- 配置事务已统一锁定 runtime_settings；检查延迟到提交，允许同一事务完成迁入。
create function check_car_seat_relations() returns void language plpgsql as $$
declare
    invalid_car text;
begin
    -- 零 seat 的 car 也必须有有限容量，不能继承全局不限并发。
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

create function validate_car_seat_relations() returns trigger language plpgsql as $$
begin
    perform check_car_seat_relations();
    return null;
end;
$$;

create constraint trigger car_groups_integrity after insert or update or delete on account_groups
    deferrable initially deferred for each row execute function validate_car_seat_relations();
create constraint trigger car_accounts_integrity after insert or update or delete on account_group_accounts
    deferrable initially deferred for each row execute function validate_car_seat_relations();
create constraint trigger car_seats_integrity after insert or update or delete on seats
    deferrable initially deferred for each row execute function validate_car_seat_relations();
create constraint trigger car_key_integrity after insert or update or delete on client_api_key_groups
    deferrable initially deferred for each row execute function validate_car_seat_relations();
create constraint trigger car_key_seat_integrity after update on client_api_keys
    deferrable initially deferred for each row when (old.seat_id is distinct from new.seat_id)
    execute function validate_car_seat_relations();
create constraint trigger car_account_capacity after update on provider_accounts
    deferrable initially deferred for each row when (old.concurrency_limit is distinct from new.concurrency_limit)
    execute function validate_car_seat_relations();
create constraint trigger car_default_capacity after update on runtime_settings
    deferrable initially deferred for each row when (old.max_concurrent_per_account is distinct from new.max_concurrent_per_account)
    execute function validate_car_seat_relations();

create function protect_car_account() returns trigger language plpgsql as $$
begin
    if exists (select 1 from seats where account_group_id = old.account_group_id) then
        raise check_violation using message = 'a car with seats cannot change its account';
    end if;
    return old;
end;
$$;
create trigger car_account_preserved before delete or update on account_group_accounts
    for each row execute function protect_car_account();
