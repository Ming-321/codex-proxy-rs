-- 支持账号单独配置期望的 State 原始字节长度（留空表示自动匹配 200..=600 有效范围）。
ALTER TABLE provider_accounts
    ADD COLUMN session_keepalive_expected_length INTEGER DEFAULT NULL;
