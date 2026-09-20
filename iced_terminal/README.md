# iced_terminal

An iced widget that draws a `zeughaus_mux::view::TerminalView` -- the client's
copy of a terminal the Zeughaus runner owns, held behind a shared
`iced_terminal::SharedView` the transport task writes and the widget reads
under a short lock -- as a single custom `wgpu` primitive per surface, and
turns keyboard, mouse, IME and clipboard input back into
`zeughaus_mux::TerminalCommand`s. Rows are shaped once per distinct
content and their GPU instances built once per shaped row and palette, so a
frame that changed one row uploads one row and a frame that only moved the
cursor reshapes nothing; the cursor blink is the widget's only timer and it
runs only while the pane has the keyboard. The bundled face is
[ComicShannsMono Nerd Font Mono](https://github.com/ryanoasis/nerd-fonts), in
regular and bold: the Comic Shanns source face is MIT, Copyright (c) 2018
Shannon Miwa (`fonts/LICENSE-ComicShanns.txt`), and the Nerd Fonts patch that
adds the icon glyphs is SIL OFL 1.1, Copyright (c) 2014 Ryan L McIntyre
(`fonts/LICENSE-NerdFonts.txt`).

```
cargo run -p iced_terminal --example demo
```
