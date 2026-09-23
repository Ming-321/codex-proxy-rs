-- 两次观测必须指向同一完整窗口；旧候选缺少起点，升级后重新确认。
alter table car_quota_cycles add column reset_candidate_start timestamptz;
