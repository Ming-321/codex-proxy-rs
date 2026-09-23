alter table account_groups
    add column car_total_weight numeric(20, 10) not null default 1
        check (car_total_weight > 0),
    add column car_quota_mode text not null default 'legacy'
        check (car_quota_mode in ('legacy', 'waiting', 'active'));

alter table seats
    add column weight numeric(20, 10) not null default 1 check (weight > 0);

create table car_quota_settings (
    singleton boolean primary key default true check (singleton),
    automatic_updates boolean not null default true,
    publish_interval_seconds bigint not null default 21600
        check (publish_interval_seconds between 300 and 2592000),
    outside_usage_protection boolean not null default true,
    minimum_sample_percent numeric(6, 3) not null default 10
        check (minimum_sample_percent > 0 and minimum_sample_percent <= 100),
    estimate_weight_percent numeric(6, 3) not null default 30
        check (estimate_weight_percent > 0 and estimate_weight_percent <= 100),
    minimum_change_percent numeric(6, 3) not null default 5
        check (minimum_change_percent >= 0 and minimum_change_percent <= 100),
    maximum_adjustment_percent numeric(6, 3) not null default 10
        check (maximum_adjustment_percent > 0 and maximum_adjustment_percent <= 100),
    abnormal_change_percent numeric(6, 3) not null default 30
        check (abnormal_change_percent > 0 and abnormal_change_percent <= 100),
    updated_at timestamptz not null default now()
);

insert into car_quota_settings (singleton) values (true);

create table car_quota_cycles (
    account_group_id text primary key references account_groups(id) on delete restrict,
    window_key text,
    cycle_start timestamptz,
    cycle_end timestamptz,
    last_observed_at timestamptz,
    last_used_percent numeric(6, 3),
    published_capacity_usd numeric(20, 10) not null default 0
        check (published_capacity_usd >= 0),
    predicted_capacity_usd numeric(20, 10),
    prediction_reason text,
    published_at timestamptz,
    published_sample_end timestamptz,
    reset_candidate_end timestamptz,
    reset_candidate_observed_at timestamptz,
    abnormal_candidate_usd numeric(20, 10),
    abnormal_candidate_sample_end timestamptz,
    updated_at timestamptz not null default now(),
    check (last_used_percent is null or (last_used_percent >= 0 and last_used_percent <= 100)),
    check (predicted_capacity_usd is null or predicted_capacity_usd >= 0),
    check (abnormal_candidate_usd is null or abnormal_candidate_usd >= 0)
);

-- 基础共享升级时已有多个seat；先建立合法总权重，不改动既有预算或费用。
update account_groups g set car_total_weight = greatest(1,
    (select count(*) from seats s where s.account_group_id = g.id)) where g.is_car;

insert into car_quota_cycles (account_group_id, published_capacity_usd, prediction_reason)
select id, car_total_weight * 130, '等待账号周期数据'
from account_groups where is_car
on conflict (account_group_id) do nothing;

create function validate_car_weights() returns trigger language plpgsql as $$
begin
    if exists (
        select 1 from account_groups g
        join lateral (
            select coalesce(sum(s.weight), 0) as allocated
            from seats s where s.account_group_id = g.id
        ) allocation on true
        where g.is_car and allocation.allocated > g.car_total_weight
    ) then
        raise check_violation using message = 'seat weights exceed car total weight';
    end if;
    return null;
end;
$$;

create constraint trigger car_group_weights after insert or update on account_groups
    deferrable initially deferred for each row execute function validate_car_weights();
create constraint trigger car_seat_weights after insert or update or delete on seats
    deferrable initially deferred for each row execute function validate_car_weights();

-- 两次观测必须指向同一完整窗口；旧候选缺少起点，升级后重新确认。
alter table car_quota_cycles add column reset_candidate_start timestamptz;
