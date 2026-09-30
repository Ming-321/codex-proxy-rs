-- 周窗口接管者与重试版本和窗口同账本持久化；解除接管后保留版本，用于拒绝迟到的旧操作。
alter table client_key_budget_windows
    add column weekly_controller text,
    add column weekly_control_revision bigint not null default 0 check (weekly_control_revision >= 0),
    add column weekly_last_operation jsonb;
create index client_key_budget_weekly_controller_idx
    on client_key_budget_windows (weekly_controller) where weekly_controller is not null;
