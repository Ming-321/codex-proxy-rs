-- 周窗口接管与重试版本同账本持久化；解除后保留版本，拒绝迟到操作。
alter table client_key_budget_windows
    add column weekly_controller text,
    add column weekly_control_revision bigint not null default 0 check (weekly_control_revision >= 0),
    add column weekly_last_operation jsonb;
create index client_key_budget_weekly_controller_idx
    on client_key_budget_windows (weekly_controller) where weekly_controller is not null;
