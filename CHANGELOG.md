# Changelog

Notable changes to agentgw, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). `scripts/release.sh` publishes the section for the
version being released as the release notes, and refuses to release without one. The rules are
in [README.md](README.md#release-notes).

## [Unreleased]

## [0.72.0] - 2026-11-18

### Added
- Claude can now end its own session when you tell it the work is finished, with or without marking the thread done.
- When Claude asks Claude in another channel for help, it can close that request as done or as canceled.
- Requests still waiting for an answer when a machine restarts are picked up again automatically. If that is not possible, you are told to send the request again.
- If an agent is stuck on a prompt that nobody answers, it is closed after 30 minutes and you are told. You can continue the thread later.

### Changed
- Only `done` marks a thread as finished with ✅. `exit` and `bye` just stop the agent.
- Typing `done` while Claude is working tells Claude you finished what it asked you to do. If it was not waiting on you, the thread is marked finished.
- The message posted when a thread ends now says what happened, instead of a generic farewell.
- A finished thread shows as closed in Slack, and opens again when you write in it.
- Signing in from Slack waits longer for the code to be accepted, and tells you clearly when it timed out.

### Fixed
- Agents that are still working are no longer shut down to make room for new ones.
- Folders you have already approved are no longer asked about again when opened through a linked path.
- Some permission prompts were not recognized and got no buttons in Slack. They now do.
- When an agent is stuck waiting for a person, the channel that asked it for help is now always told.
- After a restart, requests that were already answered are no longer woken up again.
- Empty progress messages no longer stay in the thread after Claude finishes.

## [0.71.1] - 2026-11-18

### Security
- Passwords typed directly after `-p` (for example `mysql -pSECRET`) are now hidden when one Claude checks on another's progress.
- Login tokens (JWTs) are now hidden in full in the same view, not only in part.

## [0.71.0] - 2026-11-18

### Added
- Claude can check how a request it sent to another channel is going, without interrupting it. Passwords and keys are hidden.
- Ending a session marks the thread's first message with ✅.
- When the bot is added to a channel that no machine handles yet, it asks you which machine should take it.

### Changed
- Requests from another channel show a shorter heading.

### Fixed
- Some messages went unanswered because a program started by an agent could connect to Slack as the bot. Agents can no longer see the bot's credentials.
- A reply's summary and timer settings no longer appear again below the answer.

[Unreleased]: https://github.com/takashito/agentgw/compare/v0.72.0...HEAD
[0.72.0]: https://github.com/takashito/agentgw/compare/v0.71.1...v0.72.0
[0.71.1]: https://github.com/takashito/agentgw/compare/v0.71.0...v0.71.1
[0.71.0]: https://github.com/takashito/agentgw/compare/v0.70.0...v0.71.0
