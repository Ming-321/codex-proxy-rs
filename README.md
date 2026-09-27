# Weekly budget window UI review

Before: upstream #300 commit `6311dab04166e581c9c44630320e29255bd00354`.
After: implementation commit `13fb643c9645474f2e55daf6bc604e61e3f24d95`.

These are screenshots of the actual Vue application rendered by Chromium, using synthetic API responses intercepted by Playwright. They are UI evidence, not a claim of live upstream quota-reset validation. No real keys, accounts or credentials are used. The independent backend tests use isolated PostgreSQL 18 and Redis 8.

Pages: `/keys` and `/key-usage`. Default viewport: 1440×1000 (key usage 1440×1100). Narrow viewport: 640×1000. `after-dark-popover.png` uses dark mode.

Verified interactions: open the budget popover, cancel release without sending a mutation, confirm release with the current revision, refresh the list and retain displayed used amount. Active, waiting and released states are included. No browser page errors occurred in the final runs.

- [Before Key list](before-list.png) / [After Key list](after-list.png)
- [Before popover](before-popover.png) / [After waiting popover](after-popover.png)
- [Active control](after-active-popover.png)
- [Release confirmation](after-confirm.png) / [Released](after-released.png)
- [Dark](after-dark-popover.png) / [Narrow](after-narrow-popover.png)
- [Before Key usage](before-key-usage.png) / [After Key usage](after-key-usage.png)
