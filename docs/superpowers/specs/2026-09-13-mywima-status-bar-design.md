# Simple Status Bar for mywima Design

## Overview
A lightweight status bar rendered directly in `mywima` at the bottom of the screen (1280x800 resolution), featuring:
1. A clickable "Terminal" button on the bottom-left to launch `/bin/foot`.
2. A live clock on the bottom-right displaying the current system time (`HH:MM:SS`).
3. Pointer hit testing on left-click to trigger the terminal spawn.

## Architecture
- **Location:** `mywima/src/backends/kms.rs`
- **Render Element:** Rendered as part of the frame elements overlay using GLES/Smithay render elements or a bitmap surface.
- **Hit Detection:** In the pointer click handler in `kms.rs`, if a left-click occurs within the Terminal button bounding box (`x: 4..120`, `y: 770..800`), spawn `/bin/foot`.
- **Clock:** Updated from `std::time::SystemTime` on each render pass.
