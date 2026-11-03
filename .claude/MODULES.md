# Modules

Registry of module IDs used by `.claude/maps/`, `.claude/decisions/` and `MODULE:` Decision blocks.

## Structural modules

- `kernel/mm` — guest memory (soft-MMU): `core/kernel/src/mm.rs`, `core/kernel/src/syscall/mem.rs`, the page table in `core/engine/src/c/mmu.c` and the `Mmu` wrapper, the software TLB in `core/engine/src/c/cpu.c` and `internal.h`, and the page-fault branch of the task run loop; VMAs, host backing, demand paging, TLB shootdown, fork copy-on-write, and memory introspection.

## Cross-cutting concerns
