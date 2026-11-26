# nftables (kernel/nft) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: kernel/nft"`
>
> Last distilled: 2026-11-26 (HEAD = 3ad68857)
>
> Decisions on kernel/nft made before it was split from kernel/netns keep their `kernel/netns` tags and live in `.agents/decisions/kernel/netns.md`: D4, D5, D8–D11 and the nftables half of D13.

## Active

### D1: nftables rulesets and evaluation are separate from packet paths

- **What**: kernel/nft owns nft.rs control-plane batches, rulesets and expression evaluation, while packet paths and hook selection remain in kernel/netns.
- **Why**: The extracted subsystem needs a focused architecture reference with explicit integration boundaries.
- **Tradeoffs**: Pre-split decisions remain in the parent snapshot and related changes may require several maps.
- **Watch out**: The split preserves inherited verification and open issues; it is not a fresh full audit.
- **Source**: 60790c14; 39e55b4b

### D2: Verdict-map targets participate in chain validation and lifetime

- **What**: Validate rule and element verdicts through one parser and count map references when reporting or deleting a chain.
- **Why**: A map can reference a chain without an ordinary jump rule.
- **Tradeoffs**: Removing a referenced chain returns EBUSY.
- **Watch out**: Account for references when modifying elements.
- **Source**: a1fd0beb, 3a90a0ef

### D3: Stateful nft expressions retain state at the Linux ownership boundary

- **What**: Bound xt_recent history and commit changes with the batch, keep dynset state per element and share named hashlimit tables within a family and namespace.
- **Why**: Different rule and element lifetimes must not leak or reset accounting.
- **Tradeoffs**: Stateful expressions require additional bounded storage.
- **Watch out**: Abort a failed transaction without publishing list changes.
- **Source**: f495f2cc, 71790fde

### D4: PREROUTING and conntrack matches use the correct destination

- **What**: Select pre- or post-DNAT addresses around priority −100, retain the packet mark and invert the original-destination match independently.
- **Why**: Rules can intentionally inspect either side of translation.
- **Tradeoffs**: Evaluation carries both addresses across hooks.
- **Watch out**: State-match inversion must not change destination inversion.
- **Source**: f7ddba97, 3db35725

### D5: Large nft set dumps continue at an element boundary

- **What**: Retain a cursor and pack elements to the receive budget, allowing one oversized element to guarantee progress.
- **Why**: A set can exceed one netlink datagram.
- **Tradeoffs**: Receiving the full dump can require repeated reads.
- **Watch out**: Emit the final completion record after the last element.
- **Source**: 9fc6d332
