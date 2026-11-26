# Notifications, fault reports and diagnostics (android/reports) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/reports"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions on android/reports made before it was split from android/service keep their `android/service` tags and live in `.agents/decisions/android/service.md`: D6, D24, D29, and the notification, report and diagnostic parts of D10, D14, D15, D18, D19, D21, D23, D28 and D32.

## Active

### D1: Non-ongoing notifications, fault reports and diagnostics have a dedicated module

- **What**: android/reports owns `Notifications.kt` and `Diagnostics.kt` (`Notifications`, `ReportReceiver`, `Diagnostics`, `DiagnosticFiles` and `DiagnosticProvider`), while ongoing notification id 1 and event-routing decisions stay in `android/service`.
- **Why**: Non-ongoing notifications, copyable fault reports and service-independent diagnostic retention/export needed a focused module map.
- **Tradeoffs**: Pre-split decisions remain in `.agents/decisions/android/service.md`, and notification-routing decisions remain in `android/service`.
- **Watch out**: Adding notification channels, notification IDs or privacy redaction rules requires checking both `android/service` and `android/reports`.
- **Source**: e4d7bbe5

### D2: Listen notifications identify both ports

- **What**: Include guest and host ports in the notification tag and retain the guest port in the numeric ID.
- **Why**: Remapping the host port must distinguish successive notifications.
- **Tradeoffs**: Both endpoints must be available at cancellation.
- **Watch out**: Validate the full port range before forming a key.
- **Source**: ae1e05e0
