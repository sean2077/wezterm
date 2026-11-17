---
tags:
  - hyperlink
---
# `hyperlink_accent_rules`

*This is a self-use fork feature; it is not available upstream.*

Upstream underlines an implicit hyperlink only while the mouse hovers over
it, so a link in ordinary output looks like plain text until it is hovered.
Each entry in this list keeps a visual accent on links whose URI starts with
`prefix` while they are idle, so links that matter to the user (an internal
issue tracker, for example) are recognizable at a glance.

Each entry has the following fields:

* `prefix` - links whose URI starts with this literal, case-sensitive string
  are accented. An empty prefix never matches.
* `underline` - draw a single underline even when the link is not hovered.
  Text that already carries its own underline keeps that underline; hovering
  an accented link does not turn it into a double underline. Defaults to
  `true`.
* `foreground` - foreground color for the link text. The underline uses the
  same color unless the program set an explicit underline color. When unset,
  the text keeps its original color.

The first matching entry wins. Rules apply both to implicit links produced
by `hyperlink_rules` and to OSC 8 links emitted by programs.

```lua
config.hyperlink_accent_rules = {
  { prefix = 'https://git-mm.morphic.cn/smr/', foreground = '#89b4fa' },
}
```