![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Claude & Codex Usage Monitor

A lightweight Windows taskbar widget for people already using Claude Code and/or Codex.

It sits in your taskbar and shows how much of your Claude and Codex usage window you have left, without needing to open the terminal or the provider site.

> This project is a derivative work based on [Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor) by Craig Constable, distributed under the MIT License. See [LICENSE](LICENSE) for full attribution.

## What You Get

- A **5h** bar for your current 5-hour Claude usage window
- A **7d** bar for your current 7-day window
- Optional Codex usage bars alongside Claude
- A live countdown until each limit resets
- A small native widget that lives directly in the Windows taskbar
- System tray icon badges showing your enabled model usage percentage
- Left-click the tray icon to toggle the taskbar widget on or off
- Right-click options for refresh, displayed models, update frequency, language, startup, and updates

## Who This Is For

This app is for Windows users who already have **Claude Code (CLI or App) installed and signed in**.

Codex support is optional. To show Codex usage, install and sign in to the Codex CLI, then enable Codex from the right-click **Models** menu.

It works best if you want a simple "how close am I to the limit?" display that is always visible.

## Requirements

- Windows 10 or Windows 11
- Claude Code (CLI or App) installed and authenticated
- Optional: Codex CLI installed and authenticated, if you want Codex usage

If you use Claude Code through WSL, that is supported too. The monitor can read your Claude Code credentials from Windows or from your WSL environment.

## Install

Download the latest `claude-codex-usage-monitor.exe` from the [Releases](https://github.com/Ch1nzo/claude-codex-usage-monitor/releases) page and run it directly.

## Use

Run the executable:

```powershell
claude-codex-usage-monitor
```

Once running, it will appear in your taskbar and as one or more tray icons in the notification area.

- Drag the left divider to move the taskbar widget
- Right-click the taskbar widget or tray icon for refresh, displayed models, update frequency, Start with Windows, reset position, language, updates, and exit
- Left-click the tray icon to toggle the taskbar widget on or off
- Enable `Start with Windows` from the right-click menu if you want it to launch automatically when you sign in

### Models

Use the right-click **Models** menu to choose what the widget displays:

- **Claude Code** is enabled by default
- **Codex** can be enabled alongside Claude Code or shown by itself

When both models are shown, each model has its own usage bar and matching usage text color.

### System Tray Icon

The tray icon shows your current 5-hour usage as a percentage badge.

If both Claude Code and Codex are enabled, the app shows two tray icons: one for Claude Code and one for Codex. If only one model is enabled, it shows one tray icon.

The Claude tray icon uses the same warm usage colors as the Claude bar. The Codex tray icon uses a black and white badge style.

Hovering over a tray icon shows the usage values for that model.

## Diagnostics

If you need to troubleshoot startup or visibility issues, run:

```powershell
claude-codex-usage-monitor --diagnose
```

This writes a log file to:

```text
%TEMP%\claude-codex-usage-monitor.log
```

Settings are saved to:

```text
%APPDATA%\ClaudeCodexUsageMonitor\settings.json
```

## Account Support

This app works with the same account types that Claude Code itself supports.

- **Supported:** Pro, Max, Teams, Enterprise, and Console accounts
- **Not supported:** the free Claude.ai plan

If Anthropic changes Claude Code availability in the future, this app should follow whatever Claude Code supports, as long as the usage data remains exposed through the same authenticated endpoints.

## Privacy And Security

This project is **open source**, so you can inspect exactly what it does.

What the app reads:

- Your local Claude Code OAuth credentials from `~/.claude/.credentials.json`
- If needed, the same credentials file inside an installed WSL distro
- If Codex is enabled, your local Codex credentials from `$CODEX_HOME/auth.json` or `~/.codex/auth.json`

What the app sends over the network:

- Requests to Anthropic's Claude endpoints to read your usage and rate-limit information
- Requests to ChatGPT's Codex usage endpoint to read your Codex usage and rate-limit information, if Codex is enabled
- Requests to GitHub only if you use the app's update check / self-update feature
- If proxy environment variables such as `HTTPS_PROXY`, `HTTP_PROXY`, or `ALL_PROXY` are set, those outbound requests may use that proxy

What the app stores locally:

- Widget position
- Polling frequency
- Language preference
- Last update check time
- Displayed model preferences

What it does **not** do:

- It does not send your credentials to any other server
- It does not use a separate backend service
- It does not collect analytics or telemetry
- It does not upload your project files
- It does not directly edit your Codex credentials file

Notes:

- If your Claude Code token is expired, the app may ask the local Claude CLI to refresh it in the background
- If your Codex token is expired, the app may ask the local Codex CLI to refresh it in the background. The monitor does not write `auth.json` itself; any credential update is handled by the Codex CLI.
- Portable installs can update themselves by downloading the latest release from this repository
- Proxies should be trusted because proxied usage requests include your OAuth bearer token inside the TLS connection

## How It Works

The monitor:

1. Finds your enabled model login credentials
2. Reads your current usage from Anthropic and/or ChatGPT
3. Shows the result directly in the Windows taskbar
4. Refreshes periodically in the background

If the newer usage endpoint is unavailable, it can fall back to reading the rate-limit headers returned by Claude's Messages API.

## Credits

- Original project: [Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor) by Craig Constable (MIT License)
- This fork is maintained by PIARY Co.Ltd

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details, including attribution to the original author.
