# kazi TUI snapshots

Real frames from `Screen::draw`, captured at six terminal sizes (60x20 up to 160x44) because every layout bug in this UI is a narrow-terminal bug.

Regenerate with:

```sh
KAZI_TUI_SNAPSHOT=1 cargo test tui_snapshots
```

The test is opt-in so a normal `cargo test` never writes files. Compare a change by regenerating before and after, or by diffing the committed frames.

| scenario | what it shows |
|---|---|
| `idle` | a fresh session: the welcome panel |
| `working` | plan rail, tool cards, live header, queued composer |
| `long` | a scrolled transcript with the paused-tail footer |
| `help` | the help overlay, hanging-indented wrap |
| `approval` | the approval prompt with its risk note |
| `palette` | the Ctrl+K command palette |
| `diff` | the diff reader hugging its content |


`idle`: [60x20](idle-60x20.txt) · [80x24](idle-80x24.txt) · [100x30](idle-100x30.txt) · [120x34](idle-120x34.txt) · [140x40](idle-140x40.txt) · [160x44](idle-160x44.txt)

`working`: [60x20](working-60x20.txt) · [80x24](working-80x24.txt) · [100x30](working-100x30.txt) · [120x34](working-120x34.txt) · [140x40](working-140x40.txt) · [160x44](working-160x44.txt)

`long`: [60x20](long-60x20.txt) · [80x24](long-80x24.txt) · [100x30](long-100x30.txt) · [120x34](long-120x34.txt) · [140x40](long-140x40.txt) · [160x44](long-160x44.txt)

`help`: [60x20](help-60x20.txt) · [80x24](help-80x24.txt) · [100x30](help-100x30.txt) · [120x34](help-120x34.txt) · [140x40](help-140x40.txt) · [160x44](help-160x44.txt)

`approval`: [60x20](approval-60x20.txt) · [80x24](approval-80x24.txt) · [100x30](approval-100x30.txt) · [120x34](approval-120x34.txt) · [140x40](approval-140x40.txt) · [160x44](approval-160x44.txt)

`palette`: [60x20](palette-60x20.txt) · [80x24](palette-80x24.txt) · [100x30](palette-100x30.txt) · [120x34](palette-120x34.txt) · [140x40](palette-140x40.txt) · [160x44](palette-160x44.txt)

`diff`: [60x20](diff-60x20.txt) · [80x24](diff-80x24.txt) · [100x30](diff-100x30.txt) · [120x34](diff-120x34.txt) · [140x40](diff-140x40.txt) · [160x44](diff-160x44.txt)
