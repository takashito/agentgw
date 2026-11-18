# Changelog

Notable changes to agentgw, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). `scripts/release.sh` publishes the section for the
version being released as the release notes, and refuses to release without one. The rules are
in [README.md](README.md#release-notes).

## [Unreleased]

## [0.71.0] - 2026-11-18

### Added
- `thread_activity` tool: an agent that asked another channel can read what that thread is doing (state, running tools and their seconds, last tool result, last post) without disturbing it. Secrets in the output are masked. (WORK-57)
- `exit` / `done` / `bye` add a :white_check_mark: reaction to the thread's first message when the session ends. (WORK-53)
- When the bot is brought into a channel that has no machine, it asks the Owner which machine takes the channel, with one button per connected machine. (WORK-34)

### Changed
- The heading on a cross-channel ask is shorter: `request from #channel (link)` / `follow-up from #channel (link)`. (WORK-60)

### Fixed
- Slack tokens and the link key no longer reach the agents' environment through tmux. A program started by an agent could connect to Slack as the app and take a share of the events, so some messages were never answered. (WORK-44)
- A reply's summary and timer bookings (`ScheduleWakeup`, Cron tools) no longer show up again in a progress message under the answer. (WORK-32, WORK-37)

[Unreleased]: https://github.com/takashito/agentgw/compare/v0.71.0...HEAD
[0.71.0]: https://github.com/takashito/agentgw/compare/v0.70.0...v0.71.0
