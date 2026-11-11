# Terminal view (android/vtview) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vtview"`
>
> Last distilled: 2026-11-12 (HEAD = 0553229)
>
> Decisions on the terminal view made before android/vtview was split from android/terminal keep their `android/terminal` tags and live in `.claude/decisions/android/terminal.md`: D5, D7, D9, D16, D17, D25, D30–D32, D34, D36, D37, D40, D41, D43 and D46–D48.

## Active

### D1: android/vtview is split out of android/terminal
- **What**: android/vtview owns `VtView.kt`, `VtFrame.kt`, `VtScreen.kt`, `VtRenderer.kt`, `VtImages.kt`, `VtGlyphs.kt`, `VtKeys.kt`, the font in `assets/fonts/` and `vt.c`'s Frames, Input, Paste, Mouse and focus, Selection, Links, Taps on the prompt and Search sections: frames and drawing, input, gestures and selection, search, and the uses of prompt marks (jumping, selecting output, taps on the input).
- **Why**: Drawing, input and gestures form most of the terminal's recent changes and need their own map to stay readable in one Read.
- **Tradeoffs**: The frame format is described here while the threads that produce frames are android/vt's, so a frame change reads both maps.
- **Watch out**: Its map, about 50 KB at the split, is the largest of the three and the likeliest to outgrow one Read first.
- **Source**: d42013d (files since added by 49e8b08, D3)

### D2: The view drops its image bitmaps while its window is hidden
- **What**: `onWindowVisibilityChanged` calls `dropImages` when the window is not visible, and the next frame finds the placements' pictures missing, copies them from libghostty-vt and asks for a full frame, while the backing bitmap keeps showing the rows already painted.
- **Why**: The view's copies of the shown tab's images stayed in memory while the app was in the background, where nothing draws them and libghostty-vt still holds the pixels.
- **Tradeoffs**: Coming back from Settings, another app or the background copies every placed image again (one memcpy each) and repaints the screen once.
- **Watch out**: libghostty-vt's own images are cut only under memory pressure before Android 14 (android/vt D3), so on newer versions they stay until their tab closes.
- **Source**: e9caf75

### D3: Frames, the screen mirror, drawing and images are classes of their own beside `VtView`
- **What**: `VtFrame` (an inline class over the frame buffer with named header fields, placements and row copying, named by vt.c as the frame's reader), `VtScreen` (the cells, selected columns, default colours and cursor), `VtRenderer` (the paints, cell metrics, backing bitmap and all drawing, with the cell constants) and `VtImages` (the placements and bitmaps) own their state, while `VtView` keeps the interaction code (selection, gestures, scrolling), reads cols, rows and cell sizes through getters and writes the cursor and default colours from the frame header into `VtScreen`.
- **Why**: `VtView.kt` held the frame format's offsets, the screen mirror, the paints and every draw call next to the input and gesture code, so neither side could be read on its own.
- **Tradeoffs**: `VtView` is still about 2100 lines, the interaction half, which stays coupled to its own state, and the frame header is read through accessors instead of inline offsets.
- **Watch out**: A frame format change now touches `VtFrame` and vt.c (android/terminal D5), and the drawing path moved with its locals hoisted (screen arrays read once per row), so a slowdown would show in the appine-vt frame statistics.
- **Source**: 49e8b08
