-- 窗口策略属于原生账本；版本与最近操作用于原子更新及丢失回包后的重试。
alter table client_key_budget_windows
    add column daily_fixed boolean not null default false,
    add column weekly_fixed boolean not null default false,
    add column daily_window_revision bigint not null default 0 check (daily_window_revision >= 0),
    add column weekly_window_revision bigint not null default 0 check (weekly_window_revision >= 0),
    add column daily_window_last_operation jsonb,
    add column weekly_window_last_operation jsonb;
