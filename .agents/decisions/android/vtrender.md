# Terminal frames and rendering (android/vtrender) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/vtrender"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions made before android/vtrender was split from android/vtview retain their tags in `.agents/decisions/android/vtview.md`: D1–D3 and the rendering part of D5; earlier `android/terminal` decisions remain in `.agents/decisions/android/terminal.md`: D5, D9, D16, D17, D32 and the frame/rendering parts of D7, D30, D36, D41 and D46–D48.

## Active

### D1: Frame encoding, decoding and drawing share one module

- **What**: android/vtrender owns VtFrame, VtScreen, VtRenderer, VtImages, VtGlyphs, fonts and vt.c's frame specification/Frames section, with native ownership, JNI, PNG decoding and locking in android/vtcore.
- **Why**: The frame format and drawing pipeline have source boundaries distinct from view interaction.
- **Tradeoffs**: View orchestration remains in android/vtview and frame-buffer lifetime still crosses into native terminal state.
- **Watch out**: Keep native layout and Kotlin decoding aligned, consume frames on the main thread and preserve shared locking plus inherited rendering/device verification gaps.
- **Source**: 9015b244, 81f60bef; d053be24; 39e55b4b

### D2: Frame flag bit 7 reports pending output

- **What**: flags bit 7 encodes unsettled resize output or an active synchronized-output hold and is decoded as VtFrame.outputPending.
- **Why**: The view must distinguish resize-dirtied rows from settled program output.
- **Tradeoffs**: The final flag bit is consumed without changing the 64-byte header or JNI interface.
- **Watch out**: Keep native and Kotlin layouts aligned; rejected captures consume dirty rows and require another full frame.
- **Source**: 2e73a06a; 39e55b4b

### D3: Frame capture releases the terminal lock before encoding

- **What**: Snapshot render metadata and placements under render then terminal locks and encode rows after releasing the terminal lock.
- **Why**: Expensive frame encoding stalls the output pump.
- **Tradeoffs**: A synchronized-output hold can require another render state or a full rebuild.
- **Watch out**: Preserve render-before-term-before-compression ordering and invalidate cached placements on state replacement.
- **Source**: acc5a7a0

### D4: Concealed cells retain backgrounds and hide all foreground marks

- **What**: Use the same visibility predicate for glyphs, decorations and block-cursor text while retaining selection and search backgrounds.
- **Why**: Decorations could disclose concealed characters.
- **Tradeoffs**: Alternate-screen cursor defaults follow the underlying terminal implementation.
- **Watch out**: An alternate-screen settings change may reach the primary cursor only after reset.
- **Source**: 2852cdf9
