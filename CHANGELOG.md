# Changelog

Notable changes to agentgw, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). `scripts/release.sh` publishes the section for the
version being released as the release notes, and refuses to release without one. The rules are
in [README.md](README.md#release-notes).

## [Unreleased]

### Added
- `end_session` tool: a Claude ends its own session when the person says the work is finished, either marking the thread done (✅) or not. (WORK-68)
- `end_ask` takes `outcome`: `done` (the default) marks the asked thread done with ✅, `canceled` closes it without the mark. (WORK-68)

### Changed
- Only `done` marks a thread done with ✅; `exit` and `bye` now just end the agent. (WORK-68)
- While the agent is running, a bare `done` goes to it first: if it had asked you to do something, it reads `done` as "I did it" and carries on; otherwise it marks the thread done and ends. (WORK-68)
- The line posted when a thread ends now says what happened, e.g. "#channel marked this thread done." instead of "Thanks for your work! Ending the Claude Code session." (WORK-68)
- An ended thread's agent session is set to closed in Slack; the next message there opens it again. (WORK-68)

## [0.71.1] - 2026-11-18

### Security
- `thread_activity` now masks a password written right after `-p` (`mysql -pS3cret`, `sshpass -pS3cret`, `sshpass -p S3cret`), shown as `-p***`. (WORK-63)
- `thread_activity` now masks a JWT as a whole, not only its signature: the middle part is readable JSON that can name a person. (WORK-63)

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

[Unreleased]: https://github.com/takashito/agentgw/compare/v0.71.1...HEAD
[0.71.1]: https://github.com/takashito/agentgw/compare/v0.71.0...v0.71.1
[0.71.0]: https://github.com/takashito/agentgw/compare/v0.70.0...v0.71.0
