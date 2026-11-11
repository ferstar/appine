# Terminal view (android/vtview) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vtview"`
>
> Last distilled: 2026-11-11 (HEAD = c1a37da)
>
> Decisions on the terminal view made before android/vtview was split from android/terminal keep their `android/terminal` tags and live in `.claude/decisions/android/terminal.md`: D5, D7, D9, D16, D17, D25, D30–D32, D34, D36, D37, D40, D41, D43 and D46–D48.

## Active

### D1: android/vtview is split out of android/terminal
- **What**: android/vtview owns `VtView.kt`, `VtGlyphs.kt`, `VtKeys.kt`, the font in `assets/fonts/` and `vt.c`'s Frames, Input, Paste, Mouse and focus, Selection, Links, Taps on the prompt and Search sections: frames and drawing, input, gestures and selection, search, and the uses of prompt marks (jumping, selecting output, taps on the input).
- **Why**: Drawing, input and gestures form most of the terminal's recent changes and need their own map to stay readable in one Read.
- **Tradeoffs**: The frame format is described here while the threads that produce frames are android/vt's, so a frame change reads both maps.
- **Watch out**: Its map, about 50 KB at the split, is the largest of the three and the likeliest to outgrow one Read first.
- **Source**: d42013d

### D2: The view drops its image bitmaps while its window is hidden
- **What**: `onWindowVisibilityChanged` calls `dropImages` when the window is not visible, and the next frame finds the placements' pictures missing, copies them from libghostty-vt and asks for a full frame, while the backing bitmap keeps showing the rows already painted.
- **Why**: The view's copies of the shown tab's images stayed in memory while the app was in the background, where nothing draws them and libghostty-vt still holds the pixels.
- **Tradeoffs**: Coming back from Settings, another app or the background copies every placed image again (one memcpy each) and repaints the screen once.
- **Watch out**: libghostty-vt's own images are cut only under memory pressure before Android 14 (android/vt D3), so on newer versions they stay until their tab closes.
- **Source**: e9caf75
