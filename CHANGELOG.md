# Changelog

Notable changes to agentgw, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). `scripts/release.sh` publishes the section for the
version being released as the release notes, and refuses to release without one. The rules are
in [README.md](README.md#release-notes).

## [Unreleased]

## [0.72.0] - 2026-11-18

### Added
- `end_session` tool: a Claude ends its own session when the person says the work is finished, either marking the thread done (✅) or not. (WORK-68)
- `end_ask` takes `outcome`: `done` (the default) marks the asked thread done with ✅, `canceled` closes it without the mark. (WORK-68)
- After a machine reboot, a request that was still unanswered resumes its conversation once (`--resume`) with the messages it had not answered, including a request that was already acknowledged. When it cannot be resumed, the thread and the asking agent are told to send it again. (WORK-74)
- An agent stopped at a screen nobody answers (a start-up dialog or a permission prompt) is closed after 30 minutes; the prompt says so, the asker is told, and the thread can be resumed later. (WORK-82)

### Changed
- `scripts/release.sh` runs the same checks as `scripts/deploy.sh` and stops when the release would drop a commit that is on `main` or in the build running now. `--force` releases anyway and lists what is dropped. (WORK-67)
- A Slack login whose pasted code does not finish signing in waits 90 seconds, says when it timed out, and shows the last lines of the sign-in screen (code masked). (WORK-71)
- Only `done` marks a thread done with ✅; `exit` and `bye` now just end the agent. (WORK-68)
- While the agent is running, a bare `done` goes to it first: if it had asked you to do something, it reads `done` as "I did it" and carries on; otherwise it marks the thread done and ends. (WORK-68)
- The line posted when a thread ends now says what happened, e.g. "#channel marked this thread done." instead of "Thanks for your work! Ending the Claude Code session." (WORK-68)
- An ended thread's agent session is set to closed in Slack; the next message there opens it again. (WORK-68)

### Fixed
- The idle cleanup no longer takes down an agent that is still at work (in a turn, owing an answer to the agent that asked it, or not yet seen finishing a turn since a restart). Over the concurrency cap with every agent busy, nothing is taken down. (WORK-76)
- A folder whose external imports were approved under its real path is no longer asked again when the agent starts from a linked path. (WORK-82)
- A permission prompt whose hint goes past "Esc to cancel" (e.g. "Esc to cancel · Tab to amend") is recognized, so it gets Slack buttons instead of sitting on screen. (WORK-82)
- When an agent is stuck at a screen dialog, the agents that asked it are told a person is needed even if their exchange was already closed. (WORK-82)
- A reply that names an already acknowledged request now settles it, so a later reboot does not wake the thread for a request it had answered. (WORK-74)
- A progress message holding only the agent's own prose (e.g. "● .") is removed at turn end instead of staying in the thread. (WORK-64)

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
