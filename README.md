# servo-term

A web browser that runs inside your terminal. [Servo](https://servo.org) renders
each page offscreen. Each frame goes to the terminal through the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/),
using POSIX shared memory (`t=s`), so moving the pixels takes almost no time.

Mouse, keyboard, and paste input from the terminal go back to Servo, so you can
click links, drag to select text, fill in forms, and enter URLs.

WebGL 1, WebGL 2, and WebGPU are on. WebGL runs on the software GL renderer
(slow for heavy scenes). WebGPU uses the real GPU through wgpu (Metal on macOS).
WebGPU needs a page with a host name: it does not work on `file://` URLs.

## Requirements

- A terminal that supports the Kitty graphics protocol with shared memory
  (`t=s`). The terminal and `servo-term` must run on the same host.
- For exact mouse positions: SGR pixel mouse mode (1016). Without it, clicks go to
  the center of the cell.
- Rust 1.88 or later. The first build compiles Servo and takes a few minutes.

## Run

```sh
cargo build --release
./target/release/servo-term https://news.ycombinator.com/
```

Options:

| Flag | Meaning |
|---|---|
| `--dpr F` | Device pixels per CSS pixel. The default is 2 when cells are tall (Retina), else 1 |
| `--scale F` | Render at this fraction of the pane's pixel size (0.1–1.0) |

## Keys

| Key | Action |
|---|---|
| Ctrl-L, or click the URL | Edit the URL. Plain words become a DuckDuckGo search |
| Alt-← / Alt-→, or the toolbar arrows | Back / forward |
| Ctrl-R, F5, or ↻ | Reload |
| Ctrl-+ / Ctrl-- / Ctrl-0 | Zoom in / out / reset |
| Ctrl-W | Close a tab (pages that use `target=_blank` open tabs) |
| Ctrl-Q or Ctrl-C | Quit |

All other keys go to the page.

## Limits

- `<select>` dropdowns, file pickers, and color pickers do not open.
- `alert()` / `confirm()` / `prompt()` text shows in the status line and gets
  the default answer.
- There is no clipboard and no IME composition.
- Some sites block Servo with a bot check.

## Debugging

Servo writes its log to `servo-term.log` in the current directory.
`SERVO_TERM_SNAPSHOT=frame.png` saves each frame to a PNG file (this is slow).
