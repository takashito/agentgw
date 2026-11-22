# Changelog

Notable changes to agentgw, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). `scripts/release.sh` publishes the section for the
version being released as the release notes, and refuses to release without one. The rules are
in [README.md](README.md#release-notes).

## [Unreleased]

## [0.74.0] - 2026-11-22

### Added
- A machine that was offline during an update is brought up to the gateway's version as soon as it connects again, without restarting the others. The notice channel shows it.

## [0.73.2] - 2026-11-22

### Changed
- When an agent in another channel needs a person, the notice now says what it is stopped at and what to do to let it go on, with secrets hidden. It is sent once per stop instead of again when a permission prompt expires.

### Fixed
- Messages sent to an agent that was waiting out a usage limit are now delivered once the limit lifts, instead of waiting silently until the stated reset time. If the limit still holds, the thread is told.
- A thread no longer keeps showing as waiting for a person after the prompt has been answered, has expired, or the agent has moved on.
- A message quoting an agent's "waiting on a dialog" notice is no longer mistaken for a dialog, so it is delivered instead of being posted to Slack as buttons.

## [0.73.1] - 2026-11-22

### Fixed
- When Claude renames a thread without saying which channel it is in, the thread is now renamed instead of the request failing. Claude is also told whether the new name took, so a failed rename no longer looks like a success.
- Reading a thread or channel no longer fails when one of its messages contains a heading or another newer kind of formatting. Such threads can be read again, and the Owner's posts linked from them are recognized again.

## [0.73.0] - 2026-11-19

### Added
- Any thread of a channel can check on, and follow up, a request that channel sent to another channel, not only the thread that sent it. The thread that follows up takes the request over and gets its replies.

### Changed
- Agents started by agentgw no longer show the grey prompt suggestion in their input box.
- When a request from Claude in another channel never reaches the agent there, the asking Claude is now told it was not delivered, when, and why, instead of being told the other agent failed.

### Fixed
- When a request from Claude in another channel cannot be handed to the agent there for a couple of minutes, the asking Claude is now told it has not arrived yet, instead of waiting without a word.
- On Windows, agents are no longer stopped while they run a command.
- If an agent stops in the middle of its work, the thread and any Claude waiting on it are now told, instead of it showing as still working.

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
