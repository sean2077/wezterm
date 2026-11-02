---
tags:
  - tab_bar
---
# `tab_bar_uniform_width`

*This is a self-use fork feature; it is not available upstream.*

When `true`, the fancy tab bar gives every tab the same width and reserves a
fixed-size slot for the new tab button. Without this option the button
consumes one share of the width split, and a tab whose title is shorter than
its share is rendered narrower than its neighbours.

Titles longer than a tab's share are truncated; shorter titles are padded to
the share. This keeps the tab bar geometry stable when titles change and
makes `format-tab-title` padding workarounds unnecessary.

Only affects `use_fancy_tab_bar = true`. Defaults to `false`.

```lua
config.tab_bar_uniform_width = true
```