/*
 * Tier 1 strip-mined fast loops: recognize canonical single-block (and 1-boundary cross-page)
 * constant-stride or register-stride self-loops and emit a guarded native fast loop ahead of the
 * standard checked body.
 */
#define _GNU_SOURCE
#include "dbt_internal.h"

#ifdef DBT_SUPPORTED

/* Same as fetch_guest, allowing analyze_fast_loop to peek across at most one 4 KiB guest page
 * boundary when the next page is mapped executable and shares the block's code-group status. */
static inline bool fetch_guest_fast(struct xlate *x, unsigned idx, uint32_t *out)
{
    if (LIKELY(idx < x->limit)) {
        *out = fetch_guest(x, idx);
        return true;
    }
    if (idx >= MAX_BLOCK_INSNS)
        return false;
    if (!x->tried_src1) {
        x->tried_src1 = true;
        uint64_t next_page = (x->pc & ~PAGE_MASK) + APPINE_PAGE_SIZE;
        if (next_page > x->pc && next_page < (UINT64_C(1) << APPINE_VA_BITS) &&
            (!x->shared || !__atomic_load_n(&x->d->shared, __ATOMIC_ACQUIRE) ||
             in_shared(x->d, next_page)))
            x->src1 = appine_mmu_translate(x->mmu, next_page, APPINE_PROT_EXEC);
    }
    if (!x->src1)
        return false;
    if (x->copied < x->limit) {
        memcpy(x->words + x->copied, x->src + x->copied, (x->limit - x->copied) * 4);
        x->copied = x->limit;
    }
    if (idx >= x->copied) {
        unsigned want = x->copied + 64;
        if (want > MAX_BLOCK_INSNS)
            want = MAX_BLOCK_INSNS;
        memcpy(x->words + x->copied, x->src1 + (x->copied - x->limit), (want - x->copied) * 4);
        x->copied = want;
    }
    *out = x->words[idx];
    return true;
}

/* ---- Strip-mined in-page fast loops ---- */

/*
 * Recognize canonical single-block constant-stride self-loops (prefix 0..back_idx with back-edge
 * targeting x->pc) and emit a guarded native fast loop ahead of the normal translated body:
 *   1. Verify cpu->nwatch == 0 (never bypass hardware watchpoints).
 *   2. Compute a conservative lower bound `rem_backedges` on how many iterations are guaranteed to
 *      take the back-edge before the loop exits, clamped to `1 << kick_log2` so kicks stay prompt.
 *   3. For each induction pointer (up to 7, saving addends in dbt_groups[0..6] and safe_iters in
 *      dbt_groups[7]), verify its accessed range stays within one 4 KiB page and hits the data
 *      TLB, shrinking safe_iters to the number of whole iterations that fit in that page.
 *   4. If safe_iters >= 2, add each pointer's TLB addend to its register (converting guest pointers
 *      to host pointers), execute the loop body natively with zero TLB or kick checks for
 *      safe_iters iterations (counted in T2, which fast-body instructions never touch), then
 *      subtract each addend from the root pointer and any live end-of-iteration affine aliases and
 *      fall through into the standard checked loop for page crossings, tail iterations, and kicks.
 *
 * All inserted prologue/counter/epilogue code is strictly NZCV-free (D6) and leaves X18 untouched.
 * Because safe_iters <= rem_backedges, at least one full iteration always runs in the standard
 * loop afterward, producing exact exit NZCV and precise faults on any page boundary or mismatch.
 */
#define FAST_LOOP_MAX_PTRS 7
#define FAST_LOOP_MAX_STREAMS 16
#define FAST_LOOP_MAX_SIDE_EXITS 4
#define FAST_LOOP_MAX_LITS 4
#define FAST_LOOP_MAX_WORDS 1600
_Static_assert(FAST_LOOP_MAX_PTRS < GROUP_SLOTS, "dbt_groups[7] holds safe_iters during prologue");

enum fast_loop_kind { FL_COUNTDOWN, FL_COUNTUP_ZERO, FL_CMP_INC, FL_CMP_DEC };
enum fast_cmp_kind { FL_NE, FL_SLT, FL_SLE, FL_ULT, FL_ULE };

struct fast_ptr {
    uint8_t reg;         /* 0..30 = guest Xr; 31 = guest SP (SP_HOST) */
    uint32_t alias_mask; /* regs (a != reg) holding reg + const at end of iteration */
};

struct fast_stream {
    uint8_t ptr_idx;     /* index into fast_loop::ptrs */
    uint8_t idx_reg;     /* 31 = none (plain offset / bounded LUT); 0..30 = M_REG index register */
    uint8_t idx_reg2;    /* 31 = none; 0..30 = second 64-bit index reg added before idx_shift */
    uint8_t idx_opt;     /* M_REG option (2, 3, 6, 7) */
    uint8_t idx_shift;   /* M_REG / derived left shift (0..4) */
    uint8_t stride_reg;  /* 31 = constant stride; 0..30 = loop-invariant register stride */
    bool write;
    bool first_for_ptr;  /* true saves addend in dbt_groups[ptr_idx]; false checks equality */
    int32_t lo, hi;
    int32_t stride;      /* signed constant byte stride per iteration when stride_reg == 31 */
    int32_t idx_w32_step;    /* non-zero: 32-bit UXTW/SXTW index step requiring no-wrap guard */
    int32_t idx_w32_pre_off; /* max (if step>0) or min (if step<0) pre-access 32-bit index offset */
};

struct fast_side_exit {
    uint16_t idx;    /* guest instruction index in 0..back_idx-1 */
    uint64_t va;     /* guest VA of branch instruction */
    uint64_t target; /* external target VA */
};

struct fast_loop {
    unsigned back_idx;
    enum fast_loop_kind kind;
    unsigned ctrl_idx;   /* cnt_idx for countdown/up, cmp_idx for FL_CMP_INC/FL_CMP_DEC */
    bool header_tested;  /* true: bottom unconditional B loop with header exit at hdr_br_idx */
    unsigned hdr_br_idx; /* guest index of omitted header exit branch when header_tested */
    bool omit_ctrl;      /* true: omit ctrl_idx (CMP) in fast body */
    unsigned cnt_reg;
    unsigned bound_reg;
    uint32_t bound_imm;
    bool bound_is_imm;
    bool is_64;
    bool uses_sp;
    uint32_t step;
    int64_t cmp_off; /* for FL_CMP_INC/DEC: iter-0 compared value is cnt_reg + cmp_off */
    enum fast_cmp_kind cmp;
    unsigned cd_cond; /* B.cond code for FL_COUNTDOWN / FL_COUNTUP_ZERO */
    unsigned kick_log2;
    unsigned nptrs;
    unsigned nstreams;
    unsigned nside_exits;
    unsigned nlits;
    unsigned npin;
    unsigned nborrow;
    uint8_t pin_reg[6];
    uint8_t pin_host[6];
    bool pin_written[6];
    uint8_t borrow_host[8];
    uint64_t lit_addr[FAST_LOOP_MAX_LITS];
    uint8_t lit_host[FAST_LOOP_MAX_LITS];
    struct fast_ptr ptrs[FAST_LOOP_MAX_PTRS];
    struct fast_stream streams[FAST_LOOP_MAX_STREAMS];
    struct fast_side_exit side_exits[FAST_LOOP_MAX_SIDE_EXITS];
};

static bool simd_reads_nzcv(uint32_t insn)
{
    if ((FIELD(insn, 31, 28) & 0x5) != 0x1 || BIT(insn, 24) || !BIT(insn, 21))
        return false;
    if (FIELD(insn, 15, 10) == 0)
        return false;
    unsigned op = FIELD(insn, 11, 10);
    return op == 1 || op == 3; /* FCCMP/FCCMPE, FCSEL */
}

static bool simd_writes_nzcv(uint32_t insn)
{
    if ((FIELD(insn, 31, 28) & 0x5) != 0x1 || BIT(insn, 24) || !BIT(insn, 21))
        return false;
    if (FIELD(insn, 15, 10) == 0) /* FJCVTZS */
        return !BIT(insn, 31) && FIELD(insn, 23, 22) == 1 && FIELD(insn, 20, 19) == 3 &&
               FIELD(insn, 18, 16) == 6;
    unsigned op = FIELD(insn, 11, 10);
    return op == 1 || (op == 0 && !BIT(insn, 12) && BIT(insn, 13)); /* FCCMP/E, FCMP/E */
}

static bool dp_is_flagm(uint32_t insn)
{
    return FIELD(insn, 28, 26) != 4 && BIT(insn, 28) && FIELD(insn, 24, 21) == 0 &&
           FIELD(insn, 15, 10) != 0;
}

static bool dp_reads_nzcv(uint32_t insn)
{
    if (FIELD(insn, 28, 26) == 4 || !BIT(insn, 28))
        return false;
    unsigned op2 = FIELD(insn, 24, 21);
    return op2 == 0 || op2 == 2 || op2 == 4; /* ADC/SBC/FlagM, CCMP/CCMN, CSEL */
}

static bool dp_writes_nzcv(uint32_t insn)
{
    if (FIELD(insn, 28, 26) == 4) {
        unsigned g = FIELD(insn, 25, 23);
        return (g == 2 && BIT(insn, 29)) || (g == 4 && FIELD(insn, 30, 29) == 3);
    }
    if (!BIT(insn, 28))
        return BIT(insn, 24) ? BIT(insn, 29) : FIELD(insn, 30, 29) == 3;
    unsigned op2 = FIELD(insn, 24, 21);
    if (op2 == 2) /* CCMP/CCMN */
        return true;
    if (op2 == 0) /* ADCS/SBCS or FlagM */
        return BIT(insn, 29) || FIELD(insn, 15, 10) != 0;
    return false;
}

static unsigned count_distinct_stolen(const struct xlate *x, uint32_t insn, const struct operands *o)
{
    unsigned seen[3], n = 0;
    for (unsigned i = 0; i < o->n; i++) {
        unsigned r = (insn >> o->pos[i]) & 31;
        if ((r == 31 && o->sp[i]) || !is_stolen(r))
            continue;
        unsigned k;
        for (k = 0; k < n && seen[k] != r; k++)
            ;
        if (k == n) {
            if (n == 3)
                return 4;
            seen[n++] = r;
        }
    }
    return n;
}

/* Record that guest register r (0..30) was read by a non-pointer-preserving operation */
static void fl_mark_read(unsigned r, const int8_t sym_root[31], uint32_t written, uint32_t *rbw,
                         uint32_t *nonptr)
{
    if (r >= 31)
        return;
    if (!(written & (1u << r)))
        *rbw |= 1u << r;
    if (sym_root[r] >= 0)
        *nonptr |= 1u << (unsigned)sym_root[r];
}

/* Compute proven unsigned bit-width bound (1..16, or 0 = unknown) written to rd by an integer DP
 * instruction. All 32-bit operations zero-extend into bits 63:32. */
static uint8_t u_bits_from_dp(uint32_t insn, const uint8_t u_bits[31])
{
    unsigned rn = FIELD(insn, 9, 5);
    /* Logical immediate: AND (opc=0) / ANDS (opc=3) with low bitmask (1 << k) - 1, 1 <= k <= 12.
     * Require N == sf so 64-bit does not replicate a 32-bit element across both halves. */
    if (FIELD(insn, 28, 23) == 0x24) {
        unsigned opc = FIELD(insn, 30, 29);
        if ((opc == 0 || opc == 3) && BIT(insn, 22) == BIT(insn, 31) &&
            FIELD(insn, 21, 16) == 0 && FIELD(insn, 15, 10) < 12)
            return (uint8_t)(FIELD(insn, 15, 10) + 1);
        return 0;
    }
    /* UBFM (UBFX / UXTB / UXTH / LSR / UBFIZ) */
    if ((insn & 0xffc00000) == 0xd3400000 || (insn & 0xffc00000) == 0x53000000) {
        bool is64 = BIT(insn, 31);
        unsigned datasize = is64 ? 64u : 32u;
        unsigned immr = FIELD(insn, 21, 16), imms = FIELD(insn, 15, 10);
        if (imms >= immr) {
            unsigned width = imms - immr + 1;
            if (rn < 31 && u_bits[rn] > 0 && imms == datasize - 1) {
                if (immr >= u_bits[rn])
                    return 1;
                unsigned eff = u_bits[rn] - immr;
                if (eff < width)
                    width = eff;
            }
            return width <= 16 ? (uint8_t)width : 0;
        }
        unsigned sh = datasize - immr;
        unsigned width = sh + imms + 1;
        return width <= 16 ? (uint8_t)width : 0;
    }
    /* Unshifted register logical ops: AND/ANDS/ORR/EOR (N == 0, shift == 0, imm6 == 0) */
    if ((insn & 0x1fe0fc00) == 0x0a000000) {
        unsigned opc = FIELD(insn, 30, 29);
        unsigned rm = FIELD(insn, 20, 16);
        uint8_t bn = rn < 31 ? u_bits[rn] : 0;
        uint8_t bm = rm < 31 ? u_bits[rm] : 0;
        if (opc == 0 || opc == 3) { /* AND / ANDS */
            if (rn == 31 || rm == 31)
                return 1;
            if (bn && bm)
                return bn < bm ? bn : bm;
            return bn ? bn : bm;
        }
        if (rn == 31) /* MOV Xd/Wd, Xm/Wm or EOR with ZR */
            return bm;
        if (rm == 31)
            return bn;
        if (bn && bm)
            return bn > bm ? bn : bm;
    }
    return 0;
}

/* Derived pointer expression: root + ext(idx_reg [+ idx_reg2]) << idx_shift + off,
 * or (when lut_bits > 0) root + [0, ((1 << lut_bits) - 1) << idx_shift] + off. */
struct fl_pidx {
    int8_t root;      /* -1 = none; 0..30 = root base pointer */
    int8_t idx_reg;   /* -1 = none (bounded LUT); 0..30 = index reg */
    int8_t idx_reg2;  /* -1 = none; 0..30 = second 64-bit index reg */
    uint8_t idx_opt;  /* 2 (UXTW), 3 (LSL), 6 (SXTW), 7 (SXTX) */
    uint8_t idx_shift;/* 0..4 */
    uint8_t lut_bits; /* 0 = strided/invariant idx_reg; 1..12 = bounded LUT */
    int32_t w32_off;  /* pre-access 32-bit index offset when UXTW/SXTW w32_valid */
    int64_t off;
};

static bool fl_match_cmp(uint32_t fi_insn, unsigned flag_idx, unsigned cont_cond,
                         bool header_tested, unsigned hdr_br_idx, uint32_t hdr_written,
                         const int8_t hdr_sym_root[31], const int8_t hdr_sym_sreg[31],
                         const int8_t hdr_pidx_root[31], const int64_t hdr_sym_off[31],
                         const int8_t sym_root[31], const int8_t sym_sreg[31],
                         const struct fl_pidx sym_pidx[31], const int64_t sym_off[31],
                         uint32_t w32_valid, const int32_t w32_step[31], uint32_t written,
                         const unsigned last_write_idx[31], uint32_t used_as_ptr,
                         struct fast_loop *fl)
{
    if ((fi_insn & 0x7fe0fc1f) != 0x6b00001f && (fi_insn & 0x7f80001f) != 0x7100001f)
        return false;
    bool is64 = BIT(fi_insn, 31);
    bool is_imm = (fi_insn & 0x7f80001f) == 0x7100001f;
    unsigned rn = FIELD(fi_insn, 9, 5);
    unsigned rm = is_imm ? 31 : FIELD(fi_insn, 20, 16);

    for (unsigned side = 0; side < (is_imm ? 1u : 2u); side++) {
        bool cnt_first = is_imm || side == 1;
        unsigned d = cnt_first ? rn : rm;
        unsigned b = cnt_first ? rm : rn;
        if (d >= 31)
            continue;
        if (!is_imm && (b >= 31 || b == d || (written & (1u << b)) || (used_as_ptr & (1u << b))))
            continue;

        unsigned c = d;
        int64_t signed_step = 0;
        int64_t cmp_off = 0;
        if (!header_tested) {
            if (!(written & (1u << d)) || last_write_idx[d] >= flag_idx)
                continue;
            if (is64) {
                if (sym_root[d] < 0 || sym_sreg[d] >= 0 || sym_pidx[d].root >= 0)
                    continue;
                c = (unsigned)sym_root[d];
                if ((!is_imm && c == b) || sym_root[c] != (int8_t)c || sym_sreg[c] >= 0 ||
                    sym_pidx[c].root >= 0 || sym_off[c] == 0 || sym_off[c] < -4096 ||
                    sym_off[c] > 4096)
                    continue;
                if (d != c && ((used_as_ptr & (1u << c)) || (used_as_ptr & (1u << d)) ||
                               sym_off[d] < -4095 || sym_off[d] > 4095))
                    continue;
                signed_step = sym_off[c];
                cmp_off = sym_off[d];
            } else {
                if (used_as_ptr & (1u << d))
                    continue;
                if (((w32_valid >> d) & 1) && w32_step[d] != 0)
                    signed_step = w32_step[d];
                else if (sym_root[d] == (int8_t)d && sym_sreg[d] < 0 && sym_pidx[d].root < 0 &&
                         sym_off[d] != 0 && sym_off[d] >= -4096 && sym_off[d] <= 4096)
                    signed_step = sym_off[d];
                else
                    continue;
                cmp_off = signed_step;
            }
        } else {
            if (is64) {
                if (hdr_sym_root[d] < 0 || hdr_sym_sreg[d] >= 0 || hdr_pidx_root[d] >= 0)
                    continue;
                c = (unsigned)hdr_sym_root[d];
                if ((hdr_written & (1u << c)) || (!is_imm && c == b) ||
                    !(written & (1u << c)) || last_write_idx[c] <= hdr_br_idx)
                    continue;
                if (sym_root[c] != (int8_t)c || sym_sreg[c] >= 0 || sym_pidx[c].root >= 0 ||
                    sym_off[c] == 0 || sym_off[c] < -4096 || sym_off[c] > 4096)
                    continue;
                if (d != c && ((used_as_ptr & (1u << c)) || (used_as_ptr & (1u << d)) ||
                               hdr_sym_off[d] < -4095 || hdr_sym_off[d] > 4095))
                    continue;
                signed_step = sym_off[c];
                cmp_off = hdr_sym_off[d];
            } else {
                if ((hdr_written & (1u << d)) || (used_as_ptr & (1u << d)) ||
                    !(written & (1u << d)) || last_write_idx[d] <= hdr_br_idx)
                    continue;
                if (((w32_valid >> d) & 1) && w32_step[d] != 0)
                    signed_step = w32_step[d];
                else if (sym_root[d] == (int8_t)d && sym_sreg[d] < 0 && sym_pidx[d].root < 0 &&
                         sym_off[d] != 0 && sym_off[d] >= -4096 && sym_off[d] <= 4096)
                    signed_step = sym_off[d];
                else
                    continue;
                cmp_off = 0;
            }
        }

        bool is_dec = signed_step < 0;
        bool eff_cnt_first = is_dec ? !cnt_first : cnt_first;
        enum fast_cmp_kind ck;
        bool cond_ok = true;
        if (cont_cond == 1) {
            ck = FL_NE;
        } else if (!eff_cnt_first) {
            if (cont_cond == 12)
                ck = FL_SLT;
            else if (cont_cond == 10)
                ck = FL_SLE;
            else if (cont_cond == 8)
                ck = FL_ULT;
            else if (cont_cond == 2)
                ck = FL_ULE;
            else
                cond_ok = false;
        } else {
            if (cont_cond == 11)
                ck = FL_SLT;
            else if (cont_cond == 13)
                ck = FL_SLE;
            else if (cont_cond == 3)
                ck = FL_ULT;
            else if (cont_cond == 9)
                ck = FL_ULE;
            else
                cond_ok = false;
        }
        if (!cond_ok)
            continue;
        fl->kind = is_dec ? FL_CMP_DEC : FL_CMP_INC;
        fl->ctrl_idx = flag_idx;
        fl->cnt_reg = c;
        fl->bound_reg = b;
        fl->bound_is_imm = is_imm;
        fl->bound_imm = is_imm ? (FIELD(fi_insn, 21, 10) << (BIT(fi_insn, 22) ? 12 : 0)) : 0;
        fl->is_64 = is64;
        fl->step = (uint32_t)(is_dec ? -signed_step : signed_step);
        fl->cmp_off = cmp_off;
        fl->cmp = ck;
        return true;
    }
    return false;
}

static bool analyze_fast_loop(struct xlate *x, struct fast_loop *fl)
{
    if (x->limit == 0)
        return false;

    int8_t sym_root[31], sym_sreg[31];
    int64_t sym_off[31];
    struct fl_pidx sym_pidx[31];
    uint8_t u_bits[31] = {0};
    /* Block-local derived 64-bit index expressions: (reg0 + reg1 + off) << shift (reg1 == -1 if single). */
    struct sym_idx {
        int8_t reg0, reg1;
        int64_t off;
        uint8_t shift;
    } sym_idx[31];
    struct raw_stream {
        uint8_t root, idx_reg, idx_reg2, idx_opt, idx_shift;
        bool write;
        int32_t lo, hi;
        int32_t w32_pre_off;
        bool has_w32_pre;
    } raw[FAST_LOOP_MAX_STREAMS];
    unsigned nraw = 0;
    uint64_t lit_addrs[FAST_LOOP_MAX_LITS];
    unsigned nlits = 0;
    struct cmp_rec {
        unsigned idx;
        int8_t root0, root1;
    } cmps[8];
    unsigned ncmps = 0;
    struct cand_se {
        unsigned idx;
        uint32_t insn;
        uint64_t va;
        uint64_t target;
        unsigned flag_idx;
        unsigned flag_seq;
        bool has_flag;
        bool had_mem;
        uint32_t written_mask;
        int8_t sym_root[31];
        int8_t sym_sreg[31];
        int8_t pidx_root[31];
        int64_t sym_off[31];
    } cands[FAST_LOOP_MAX_SIDE_EXITS + 1];
    unsigned ncands = 0;
    uint32_t used_as_ptr = 0, used_as_nonptr = 0;
    uint32_t written = 0, read_before_write = 0;
    uint32_t w32_valid = (1u << 31) - 1;
    int32_t w32_step[31] = {0};
    unsigned write_count[31] = {0};
    unsigned last_write_idx[31] = {0};
    unsigned flag_writers = 0, last_flag_idx = 0;
    bool nzcv_written = false, nzcv_read_after_last_write = false;
    bool hdr_flag_read_by_other = false;
    unsigned body_words = 0, extra_body_words = 0;

    for (unsigned r = 0; r < 31; r++) {
        sym_root[r] = (int8_t)r;
        sym_sreg[r] = -1;
        sym_off[r] = 0;
        sym_pidx[r] = (struct fl_pidx){.root = -1, .idx_reg = -1, .idx_reg2 = -1};
        sym_idx[r] = (struct sym_idx){.reg0 = (int8_t)r, .reg1 = -1, .off = 0, .shift = 0};
    }

    unsigned back_idx = 0;
    uint32_t back_insn = 0;
    bool found_back = false, back_is_uncond = false;

    for (unsigned i = 0; i < MAX_BLOCK_INSNS; i++) {
        uint32_t insn;
        if (!fetch_guest_fast(x, i, &insn))
            return false;
        uint64_t va = x->pc + 4ull * i;
        unsigned top = FIELD(insn, 28, 25);

        /* Unconditional B targeting x->pc closes a header-tested while loop */
        if ((insn & 0xfc000000) == 0x14000000) {
            uint64_t target = va + (uint64_t)(sext(FIELD(insn, 25, 0), 26) * 4);
            if (target == x->pc && i > 0 && ncands > 0 &&
                dbt_tier0_defined(x->probe, x->defined, insn, va)) {
                back_idx = i;
                back_insn = insn;
                back_is_uncond = true;
                found_back = true;
                break;
            }
            return false;
        }

        /* Conditional branch (B.cond / CBZ / CBNZ / TBZ / TBNZ): back-edge or candidate side exit */
        if ((insn & 0xff000010) == 0x54000000 || (insn & 0x7e000000) == 0x34000000 ||
            (insn & 0x7e000000) == 0x36000000) {
            bool is_tb = (insn & 0x7e000000) == 0x36000000;
            bool is_bcond = (insn & 0xff000010) == 0x54000000;
            uint64_t target = va + (uint64_t)((is_tb ? sext(FIELD(insn, 18, 5), 14)
                                                     : sext(FIELD(insn, 23, 5), 19)) *
                                              4);
            if (!dbt_tier0_defined(x->probe, x->defined, insn, va))
                return false;
            if (target == x->pc && i > 0) {
                back_idx = i;
                back_insn = insn;
                found_back = true;
                break;
            }
            if (ncands >= FAST_LOOP_MAX_SIDE_EXITS + 1)
                return false;
            if (is_bcond) {
                if (!nzcv_written)
                    return false;
                if (ncands == 0 && nzcv_read_after_last_write)
                    hdr_flag_read_by_other = true;
                if (ncands > 0 && cands[0].has_flag && flag_writers == cands[0].flag_seq)
                    hdr_flag_read_by_other = true;
                nzcv_read_after_last_write = true;
            } else {
                unsigned rt = insn & 31;
                fl_mark_read(rt, sym_root, written, &read_before_write, &used_as_nonptr);
                if (rt < 31 && is_stolen(rt))
                    body_words += 1;
            }
            struct cand_se *ce = &cands[ncands++];
            ce->idx = i;
            ce->insn = insn;
            ce->va = va;
            ce->target = target;
            ce->flag_idx = last_flag_idx;
            ce->flag_seq = flag_writers;
            ce->has_flag = nzcv_written;
            ce->had_mem = nraw > 0 || nlits > 0;
            ce->written_mask = written;
            memcpy(ce->sym_root, sym_root, sizeof(sym_root));
            memcpy(ce->sym_sreg, sym_sreg, sizeof(sym_sreg));
            for (unsigned r = 0; r < 31; r++)
                ce->pidx_root[r] = sym_pidx[r].root;
            memcpy(ce->sym_off, sym_off, sizeof(sym_off));
            body_words += 1;
            continue;
        }
        if (insn == 0xd503201f) /* NOP */
            continue;

        if ((top & 7) == 7) { /* SIMD&FP data processing */
            struct native_desc nd;
            if (!appine_has(APPINE_HWCAP_FP | APPINE_HWCAP_ASIMD) || !appine_native_available() ||
                !appine_simd_dp_decode(insn, &nd))
                return false;
            bool reads_flags = simd_reads_nzcv(insn);
            bool writes_flags = simd_writes_nzcv(insn);
            if (reads_flags) {
                if (!nzcv_written)
                    return false;
                if (ncands > 0 && cands[0].has_flag && flag_writers == cands[0].flag_seq)
                    hdr_flag_read_by_other = true;
                nzcv_read_after_last_write = true;
            }
            if (writes_flags) {
                nzcv_written = true;
                flag_writers++;
                last_flag_idx = i;
                nzcv_read_after_last_write = false;
            }
            struct operands o = {.n = 0, .writes_rd = false};
            for (unsigned k = 0; k < nd.n; k++) {
                add_operand(&o, nd.gpr[k].pos, false);
                unsigned r = (insn >> nd.gpr[k].pos) & 31;
                if (nd.gpr[k].load)
                    fl_mark_read(r, sym_root, written, &read_before_write, &used_as_nonptr);
            }
            unsigned ns = count_distinct_stolen(x, insn, &o);
            if (ns > 2)
                return false;
            body_words += 1 + ns * 2;
            for (unsigned k = 0; k < nd.n; k++) {
                if (nd.gpr[k].store) {
                    unsigned r = (insn >> nd.gpr[k].pos) & 31;
                    if (r < 31) {
                        sym_root[r] = -1;
                        sym_sreg[r] = -1;
                        sym_pidx[r].root = -1;
                        sym_idx[r].reg0 = -1;
                        u_bits[r] = 0;
                        w32_valid &= ~(1u << r);
                        written |= 1u << r;
                        write_count[r]++;
                        last_write_idx[r] = i;
                    }
                }
            }
            continue;
        }

        if (!dbt_tier0_defined(x->probe, x->defined, insn, va))
            return false;

        if (top == 0x4 || top == 0x6 || top == 0xc || top == 0xe) { /* Memory access */
            struct memop m;
            if (dbt_is_atomic(insn) ||
                !(BIT(insn, 26) ? dbt_decode_simd_memop(insn, va, &m) : dbt_decode_memop(insn, va, &m)))
                return false;
            if (m.prefetch)
                continue;
            if (m.mode == M_LITERAL) {
                uint64_t addr = (uint64_t)m.imm;
                unsigned bytes = memop_bytes(&m);
                if (((addr ^ (addr + bytes - 1)) >> APPINE_PAGE_SHIFT) != 0)
                    return false;
                unsigned li;
                for (li = 0; li < nlits && lit_addrs[li] != addr; li++)
                    ;
                if (li == nlits) {
                    if (nlits >= FAST_LOOP_MAX_LITS)
                        return false;
                    lit_addrs[nlits++] = addr;
                }
                body_words += 1 + (!m.simd && m.rt < 31 && is_stolen(m.rt) ? 1 : 0);
                if (!m.simd && m.rt < 31) {
                    sym_root[m.rt] = -1;
                    sym_sreg[m.rt] = -1;
                    sym_pidx[m.rt].root = -1;
                    sym_idx[m.rt].reg0 = -1;
                    u_bits[m.rt] = 0;
                    w32_valid &= ~(1u << m.rt);
                    written |= 1u << m.rt;
                    write_count[m.rt]++;
                    last_write_idx[m.rt] = i;
                }
                continue;
            }
            if (m.wb_reg || (m.rn == 31 && m.mode != M_OFFSET))
                return false;
            if ((m.mode == M_PRE || m.mode == M_POST) && !m.simd &&
                (m.rt == m.rn || (m.pair && m.rt2 == m.rn)))
                return false;
            if (m.pair && !m.simd && m.load && m.rt == m.rt2)
                return false;
            if (m.rn < 31 && (sym_root[m.rn] < 0 || sym_sreg[m.rn] >= 0))
                return false;

            unsigned idx_reg = 31, idx_reg2 = 31, idx_opt = 0, idx_shift = 0;
            int64_t idx_cur_off = 0;
            int64_t lut_extra_end = 0;
            int32_t w32_pre_off = 0;
            bool has_w32_pre = false;
            bool rn_has_pidx = m.rn < 31 && sym_pidx[m.rn].root >= 0;
            if (rn_has_pidx) {
                if (m.mode == M_REG)
                    return false;
                const struct fl_pidx *pi = &sym_pidx[m.rn];
                if (pi->lut_bits > 0) {
                    lut_extra_end = (((int64_t)1 << pi->lut_bits) - 1) << pi->idx_shift;
                } else {
                    idx_reg = (unsigned)pi->idx_reg;
                    idx_reg2 = pi->idx_reg2 >= 0 ? (unsigned)pi->idx_reg2 : 31;
                    if (idx_reg2 < 31 && idx_reg > idx_reg2) {
                        unsigned tmp = idx_reg;
                        idx_reg = idx_reg2;
                        idx_reg2 = tmp;
                    }
                    idx_opt = pi->idx_opt;
                    idx_shift = pi->idx_shift;
                    w32_pre_off = pi->w32_off;
                    has_w32_pre = (idx_opt == 2 || idx_opt == 6);
                    used_as_nonptr |= 1u << idx_reg;
                    if (idx_reg2 < 31)
                        used_as_nonptr |= 1u << idx_reg2;
                }
            } else if (m.mode == M_REG && m.rm < 31) {
                if (sym_root[m.rm] == (int8_t)m.rm && sym_sreg[m.rm] < 0) {
                    if (sym_off[m.rm] != 0 && m.option != 3 && m.option != 7)
                        return false;
                    idx_reg = m.rm;
                    idx_opt = m.option;
                    idx_shift = m.shift;
                    idx_cur_off = sym_off[m.rm] * (int64_t)(1u << m.shift);
                    if (idx_opt == 2 || idx_opt == 6) {
                        w32_pre_off = 0;
                        has_w32_pre = true;
                    }
                    fl_mark_read(m.rm, sym_root, written, &read_before_write, &used_as_nonptr);
                } else if ((m.option == 2 || m.option == 6) && ((w32_valid >> m.rm) & 1) &&
                           w32_step[m.rm] != 0) {
                    idx_reg = m.rm;
                    idx_opt = m.option;
                    idx_shift = m.shift;
                    idx_cur_off = (int64_t)w32_step[m.rm] * (int64_t)(1u << m.shift);
                    w32_pre_off = w32_step[m.rm];
                    has_w32_pre = true;
                    fl_mark_read(m.rm, sym_root, written, &read_before_write, &used_as_nonptr);
                } else if ((m.option == 3 || m.option == 7) && sym_idx[m.rm].reg0 >= 0 &&
                           sym_idx[m.rm].shift + m.shift <= 4) {
                    unsigned tot_shift = sym_idx[m.rm].shift + m.shift;
                    idx_reg = (unsigned)sym_idx[m.rm].reg0;
                    idx_reg2 = sym_idx[m.rm].reg1 >= 0 ? (unsigned)sym_idx[m.rm].reg1 : 31;
                    if (idx_reg2 < 31 && idx_reg > idx_reg2) {
                        unsigned tmp = idx_reg;
                        idx_reg = idx_reg2;
                        idx_reg2 = tmp;
                    }
                    idx_opt = m.option;
                    idx_shift = tot_shift;
                    idx_cur_off = sym_idx[m.rm].off * (int64_t)(1u << tot_shift);
                    fl_mark_read(m.rm, sym_root, written, &read_before_write, &used_as_nonptr);
                    used_as_nonptr |= 1u << idx_reg;
                    if (idx_reg2 < 31)
                        used_as_nonptr |= 1u << idx_reg2;
                } else if (u_bits[m.rm] > 0 && u_bits[m.rm] <= 12 &&
                           (m.option == 2 || m.option == 3 || m.option == 6 || m.option == 7) &&
                           ((1u << u_bits[m.rm]) << m.shift) <= APPINE_PAGE_SIZE) {
                    /* Bounded table-lookup M_REG stream: [base, rm, opt #shift] stays within
                     * [0, ((1 << u_bits[rm]) - 1) << shift + bytes). */
                    lut_extra_end = (((int64_t)1 << u_bits[m.rm]) - 1) << m.shift;
                    fl_mark_read(m.rm, sym_root, written, &read_before_write, &used_as_nonptr);
                } else {
                    return false;
                }
            }

            unsigned seen[2], ns = 0;
            if (m.rn < 31 && is_stolen(m.rn))
                seen[ns++] = m.rn;
            if (m.mode == M_REG && m.rm < 31 && is_stolen(m.rm) && (ns == 0 || seen[0] != m.rm)) {
                if (ns == 2)
                    return false;
                seen[ns++] = m.rm;
            }
            if (!m.simd) {
                if (m.rt < 31 && is_stolen(m.rt)) {
                    unsigned k;
                    for (k = 0; k < ns && seen[k] != m.rt; k++)
                        ;
                    if (k == ns) {
                        if (ns == 2)
                            return false;
                        seen[ns++] = m.rt;
                    }
                }
                if (m.pair && m.rt2 < 31 && is_stolen(m.rt2)) {
                    unsigned k;
                    for (k = 0; k < ns && seen[k] != m.rt2; k++)
                        ;
                    if (k == ns) {
                        if (ns == 2)
                            return false;
                        seen[ns++] = m.rt2;
                    }
                }
            }
            body_words +=
                1 + ns * 2 + ((m.mode == M_PRE || m.mode == M_POST) && m.rn < 31 && is_stolen(m.rn) ? 1 : 0);

            if (m.rn < 31 && !(written & (1u << m.rn)))
                read_before_write |= 1u << m.rn;
            unsigned root = m.rn == 31 ? 31 : (unsigned)sym_root[m.rn];
            int64_t base_off = m.rn == 31 ? 0 : (rn_has_pidx ? sym_pidx[m.rn].off : sym_off[m.rn]);
            int64_t start = base_off + idx_cur_off +
                            (m.mode == M_POST || m.mode == M_REG ? 0 : m.imm);
            int64_t end = start + lut_extra_end + (int64_t)memop_bytes(&m);
            if (start < -4095 || start > 4095 || end < -4095 || end > 8192)
                return false;
            used_as_ptr |= 1u << root;
            unsigned si;
            for (si = 0; si < nraw; si++) {
                if (raw[si].root == root && raw[si].idx_reg == idx_reg && raw[si].idx_reg2 == idx_reg2 &&
                    raw[si].idx_opt == idx_opt && raw[si].idx_shift == idx_shift)
                    break;
            }
            if (si == nraw) {
                if (nraw >= FAST_LOOP_MAX_STREAMS)
                    return false;
                raw[nraw++] = (struct raw_stream){
                    .root = (uint8_t)root,
                    .idx_reg = (uint8_t)idx_reg,
                    .idx_reg2 = (uint8_t)idx_reg2,
                    .idx_opt = (uint8_t)idx_opt,
                    .idx_shift = (uint8_t)idx_shift,
                    .write = !m.load,
                    .lo = (int32_t)start,
                    .hi = (int32_t)end,
                    .w32_pre_off = w32_pre_off,
                    .has_w32_pre = has_w32_pre,
                };
            } else {
                if (start < raw[si].lo)
                    raw[si].lo = (int32_t)start;
                if (end > raw[si].hi)
                    raw[si].hi = (int32_t)end;
                if (!m.load)
                    raw[si].write = true;
                if (has_w32_pre) {
                    if (!raw[si].has_w32_pre) {
                        raw[si].w32_pre_off = w32_pre_off;
                        raw[si].has_w32_pre = true;
                    } else if (w32_pre_off > 0 && w32_pre_off > raw[si].w32_pre_off) {
                        raw[si].w32_pre_off = w32_pre_off;
                    } else if (w32_pre_off < 0 && w32_pre_off < raw[si].w32_pre_off) {
                        raw[si].w32_pre_off = w32_pre_off;
                    }
                }
            }
            if (!m.load && !m.simd) {
                fl_mark_read(m.rt, sym_root, written, &read_before_write, &used_as_nonptr);
                if (m.pair)
                    fl_mark_read(m.rt2, sym_root, written, &read_before_write, &used_as_nonptr);
            }
            if (m.mode == M_PRE || m.mode == M_POST) {
                sym_off[m.rn] += m.imm;
                if (sym_pidx[m.rn].root >= 0)
                    sym_pidx[m.rn].off += m.imm;
                if (sym_idx[m.rn].reg0 >= 0 && sym_idx[m.rn].shift == 0)
                    sym_idx[m.rn].off += m.imm;
                else
                    sym_idx[m.rn].reg0 = -1;
                u_bits[m.rn] = 0;
                w32_valid &= ~(1u << m.rn);
                written |= 1u << m.rn;
                write_count[m.rn]++;
                last_write_idx[m.rn] = i;
            }
            if (m.load && !m.simd) {
                uint8_t load_ub = 0;
                if (!m.pair && FIELD(insn, 23, 22) == 1) { /* unsigned LDRB/LDRH */
                    if (m.size == 0)
                        load_ub = 8;
                    else if (m.size == 1)
                        load_ub = 16;
                }
                if (m.rt < 31) {
                    sym_root[m.rt] = -1;
                    sym_sreg[m.rt] = -1;
                    sym_pidx[m.rt].root = -1;
                    sym_idx[m.rt].reg0 = -1;
                    u_bits[m.rt] = load_ub;
                    w32_valid &= ~(1u << m.rt);
                    written |= 1u << m.rt;
                    write_count[m.rt]++;
                    last_write_idx[m.rt] = i;
                }
                if (m.pair && m.rt2 < 31) {
                    sym_root[m.rt2] = -1;
                    sym_sreg[m.rt2] = -1;
                    sym_pidx[m.rt2].root = -1;
                    sym_idx[m.rt2].reg0 = -1;
                    u_bits[m.rt2] = 0;
                    w32_valid &= ~(1u << m.rt2);
                    written |= 1u << m.rt2;
                    write_count[m.rt2]++;
                    last_write_idx[m.rt2] = i;
                }
            }
            continue;
        }

        if (top == 0x8 || top == 0x9 || top == 0x5 || top == 0xd) { /* Integer DP */
            if (FIELD(insn, 28, 24) == 0x10) { /* ADR / ADRP: compile-time constant VA */
                unsigned rd = insn & 31;
                extra_body_words += 3;
                body_words += 4 + (rd < 31 && is_stolen(rd) ? 1 : 0);
                if (rd < 31) {
                    sym_root[rd] = -1;
                    sym_sreg[rd] = -1;
                    sym_pidx[rd].root = -1;
                    sym_idx[rd].reg0 = -1;
                    u_bits[rd] = 0;
                    w32_valid &= ~(1u << rd);
                    written |= 1u << rd;
                    write_count[rd]++;
                    last_write_idx[rd] = i;
                }
                continue;
            }
            if (dp_is_flagm(insn))
                return false;
            struct operands o;
            if (!dbt_dp_operands(insn, &o))
                return false;
            bool reads_flags = dp_reads_nzcv(insn);
            bool writes_flags = dp_writes_nzcv(insn);
            if (reads_flags) {
                if (!nzcv_written)
                    return false;
                if (ncands > 0 && cands[0].has_flag && flag_writers == cands[0].flag_seq)
                    hdr_flag_read_by_other = true;
                nzcv_read_after_last_write = true;
            }
            for (unsigned k = 0; k < o.n; k++) {
                if (((insn >> o.pos[k]) & 31) == 31 && o.sp[k])
                    return false; /* do not touch SP */
            }
            unsigned ns = count_distinct_stolen(x, insn, &o);
            if (ns > 2)
                return false;
            body_words += 1 + ns * 2;

            if (writes_flags) {
                nzcv_written = true;
                flag_writers++;
                last_flag_idx = i;
                nzcv_read_after_last_write = false;
            }

            uint32_t op = insn & 0xff800000;
            unsigned rd = insn & 31, rn = FIELD(insn, 9, 5);
            /* 64-bit ADD/SUB immediate (non-flag-setting): preserves pointer affine relation */
            if (op == 0x91000000 || op == 0xd1000000) {
                if (!(written & (1u << rn)))
                    read_before_write |= 1u << rn;
                int64_t imm = (int64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0);
                if (op == 0xd1000000)
                    imm = -imm;
                struct sym_idx idx_in = sym_idx[rn];
                struct fl_pidx pidx_in = sym_pidx[rn];
                if (sym_root[rn] >= 0) {
                    sym_root[rd] = sym_root[rn];
                    sym_sreg[rd] = sym_sreg[rn];
                    sym_off[rd] = sym_off[rn] + imm;
                } else {
                    sym_root[rd] = -1;
                    sym_sreg[rd] = -1;
                }
                if (pidx_in.root >= 0) {
                    pidx_in.off += imm;
                    sym_pidx[rd] = pidx_in;
                } else {
                    sym_pidx[rd].root = -1;
                }
                if (idx_in.reg0 >= 0 && idx_in.shift == 0) {
                    idx_in.off += imm;
                    sym_idx[rd] = idx_in;
                } else {
                    sym_idx[rd].reg0 = -1;
                }
                u_bits[rd] = 0;
                w32_valid &= ~(1u << rd);
                written |= 1u << rd;
                write_count[rd]++;
                last_write_idx[rd] = i;
                continue;
            }
            /* 64-bit ADDS/SUBS Xd, Xd, #imm (rd < 31): tracks 64-bit non-pointer index/counter step */
            if ((op == 0xb1000000 || op == 0xf1000000) && rd == rn && rd < 31) {
                int64_t imm = (int64_t)FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0);
                if (op == 0xf1000000)
                    imm = -imm;
                fl_mark_read(rn, sym_root, written, &read_before_write, &used_as_nonptr);
                if (sym_root[rd] == (int8_t)rd && sym_sreg[rd] < 0 && sym_pidx[rd].root < 0)
                    sym_off[rd] += imm;
                else {
                    sym_root[rd] = -1;
                    sym_sreg[rd] = -1;
                    sym_pidx[rd].root = -1;
                }
                if (sym_idx[rd].reg0 >= 0 && sym_idx[rd].shift == 0)
                    sym_idx[rd].off += imm;
                else
                    sym_idx[rd].reg0 = -1;
                u_bits[rd] = 0;
                w32_valid &= ~(1u << rd);
                written |= 1u << rd;
                write_count[rd]++;
                last_write_idx[rd] = i;
                continue;
            }
            /* 64-bit MOV Xd, Xm (ORR Xd, XZR, Xm): preserves pointer affine relation & u_bits */
            if ((insn & 0xffe0ffe0) == 0xaa0003e0) {
                unsigned rm = FIELD(insn, 20, 16);
                if (rm < 31 && !(written & (1u << rm)))
                    read_before_write |= 1u << rm;
                if (rd < 31) {
                    struct sym_idx idx_in = rm < 31 ? sym_idx[rm] : (struct sym_idx){.reg0 = -1};
                    struct fl_pidx pidx_in = rm < 31 ? sym_pidx[rm] : (struct fl_pidx){.root = -1};
                    uint8_t ub_in = rm < 31 ? u_bits[rm] : 0;
                    if (rm < 31 && sym_root[rm] >= 0) {
                        sym_root[rd] = sym_root[rm];
                        sym_sreg[rd] = sym_sreg[rm];
                        sym_off[rd] = sym_off[rm];
                    } else {
                        sym_root[rd] = -1;
                        sym_sreg[rd] = -1;
                    }
                    sym_pidx[rd] = pidx_in;
                    sym_idx[rd] = idx_in;
                    u_bits[rd] = ub_in;
                    w32_valid &= ~(1u << rd);
                    written |= 1u << rd;
                    write_count[rd]++;
                    last_write_idx[rd] = i;
                }
                continue;
            }
            /* 64-bit ADD Xd, Xn, Xm{, LSL #0..4} or ADD Xd, Xn, Xm/Wm, UXTW/SXTW/UXTX/SXTX #0..4:
             * tracks in-place register stride, shifted derived pointer, or derived sum index. */
            bool is_add_lsl = (insn & 0xffe00000) == 0x8b000000 && FIELD(insn, 15, 10) <= 4;
            bool is_add_ext = (insn & 0xffe00000) == 0x8b200000 && FIELD(insn, 12, 10) <= 4 &&
                              (FIELD(insn, 15, 13) == 2 || FIELD(insn, 15, 13) == 3 ||
                               FIELD(insn, 15, 13) == 6 || FIELD(insn, 15, 13) == 7);
            if (is_add_lsl || is_add_ext) {
                unsigned rm = FIELD(insn, 20, 16);
                unsigned opt = is_add_ext ? FIELD(insn, 15, 13) : 3;
                unsigned sh = is_add_ext ? FIELD(insn, 12, 10) : FIELD(insn, 15, 10);
                /* In-place register stride (unshifted ADD Xd, Xd, Xm) */
                if (is_add_lsl && sh == 0) {
                    unsigned sreg = (rd == rn) ? rm : (rd == rm ? rn : 31);
                    if (rd < 31 && sreg < 31 && sreg != rd && sym_root[rd] == (int8_t)rd &&
                        sym_sreg[rd] == -1 && sym_pidx[rd].root < 0) {
                        if (!(written & (1u << rd)))
                            read_before_write |= 1u << rd;
                        fl_mark_read(sreg, sym_root, written, &read_before_write, &used_as_nonptr);
                        sym_sreg[rd] = (int8_t)sreg;
                        sym_idx[rd].reg0 = -1;
                        u_bits[rd] = 0;
                        w32_valid &= ~(1u << rd);
                        written |= 1u << rd;
                        write_count[rd]++;
                        last_write_idx[rd] = i;
                        continue;
                    }
                }
                /* Derived pointer Xd = Xn + ext(Xm) << sh (and for unshifted, also sym_idx) */
                if (rd < 31 && rn < 31 && rm < 31 && sym_root[rn] >= 0 && sym_sreg[rn] < 0 &&
                    sym_pidx[rn].root < 0 && sym_root[rn] != (int8_t)rd) {
                    struct fl_pidx np = {.root = -1, .idx_reg = -1, .idx_reg2 = -1};
                    struct sym_idx nidx = {.reg0 = -1, .reg1 = -1};
                    uint32_t extra_nonptr = 0;
                    if (sym_root[rm] == (int8_t)rm && sym_sreg[rm] < 0 &&
                        (sym_off[rm] == 0 || opt == 3 || opt == 7)) {
                        int64_t off = sym_off[rn] + sym_off[rm] * (int64_t)(1u << sh);
                        np = (struct fl_pidx){
                            .root = sym_root[rn],
                            .idx_reg = (int8_t)rm,
                            .idx_reg2 = -1,
                            .idx_opt = (uint8_t)opt,
                            .idx_shift = (uint8_t)sh,
                            .off = off,
                        };
                        if (is_add_lsl && sh == 0) {
                            nidx = (struct sym_idx){
                                .reg0 = sym_root[rn],
                                .reg1 = sym_root[rm],
                                .off = sym_off[rn] + sym_off[rm],
                                .shift = 0,
                            };
                        }
                    } else if ((opt == 2 || opt == 6) && ((w32_valid >> rm) & 1) &&
                               w32_step[rm] != 0) {
                        np = (struct fl_pidx){
                            .root = sym_root[rn],
                            .idx_reg = (int8_t)rm,
                            .idx_reg2 = -1,
                            .idx_opt = (uint8_t)opt,
                            .idx_shift = (uint8_t)sh,
                            .w32_off = w32_step[rm],
                            .off = sym_off[rn] + (int64_t)w32_step[rm] * (int64_t)(1u << sh),
                        };
                    } else if ((opt == 3 || opt == 7) && sym_idx[rm].reg0 >= 0 &&
                               sym_idx[rm].shift + sh <= 4) {
                        unsigned tot_sh = sym_idx[rm].shift + sh;
                        np = (struct fl_pidx){
                            .root = sym_root[rn],
                            .idx_reg = sym_idx[rm].reg0,
                            .idx_reg2 = sym_idx[rm].reg1,
                            .idx_opt = (uint8_t)opt,
                            .idx_shift = (uint8_t)tot_sh,
                            .off = sym_off[rn] + sym_idx[rm].off * (int64_t)(1u << tot_sh),
                        };
                        extra_nonptr |= 1u << (unsigned)sym_idx[rm].reg0;
                        if (sym_idx[rm].reg1 >= 0)
                            extra_nonptr |= 1u << (unsigned)sym_idx[rm].reg1;
                    } else if (u_bits[rm] > 0 && u_bits[rm] <= 12 &&
                               ((1u << u_bits[rm]) << sh) <= APPINE_PAGE_SIZE) {
                        np = (struct fl_pidx){
                            .root = sym_root[rn],
                            .idx_reg = -1,
                            .idx_reg2 = -1,
                            .idx_opt = 0,
                            .idx_shift = (uint8_t)sh,
                            .lut_bits = u_bits[rm],
                            .off = sym_off[rn],
                        };
                    } else if (is_add_lsl && sh == 0 && sym_root[rm] >= 0 && sym_sreg[rm] < 0) {
                        nidx = (struct sym_idx){
                            .reg0 = sym_root[rn],
                            .reg1 = sym_root[rm],
                            .off = sym_off[rn] + sym_off[rm],
                            .shift = 0,
                        };
                    }
                    if (np.root >= 0) {
                        if (!(written & (1u << rn)))
                            read_before_write |= 1u << rn;
                        fl_mark_read(rm, sym_root, written, &read_before_write, &used_as_nonptr);
                        used_as_nonptr |= extra_nonptr;
                        sym_root[rd] = np.root;
                        sym_sreg[rd] = -1;
                        sym_off[rd] = np.off;
                        sym_pidx[rd] = np;
                        sym_idx[rd] = nidx;
                        u_bits[rd] = 0;
                        w32_valid &= ~(1u << rd);
                        written |= 1u << rd;
                        write_count[rd]++;
                        last_write_idx[rd] = i;
                        continue;
                    }
                    if (nidx.reg0 >= 0) {
                        fl_mark_read(rn, sym_root, written, &read_before_write, &used_as_nonptr);
                        fl_mark_read(rm, sym_root, written, &read_before_write, &used_as_nonptr);
                        sym_root[rd] = -1;
                        sym_sreg[rd] = -1;
                        sym_pidx[rd].root = -1;
                        sym_idx[rd] = nidx;
                        u_bits[rd] = 0;
                        w32_valid &= ~(1u << rd);
                        written |= 1u << rd;
                        write_count[rd]++;
                        last_write_idx[rd] = i;
                        continue;
                    }
                }
            }
            /* 64-bit LSL Xd, Xn, #sh (UBFM Xd, Xn, #64-sh, #63-sh, 1 <= sh <= 4): shifts derived index */
            if ((insn & 0xffc00000) == 0xd3400000 && rd < 31 && rn < 31) {
                unsigned immr = FIELD(insn, 21, 16), imms = FIELD(insn, 15, 10);
                if (imms != 63 && immr == imms + 1) {
                    unsigned sh = 64 - immr;
                    if (sh >= 1 && sh <= 4 && sym_idx[rn].reg0 >= 0 && sym_idx[rn].shift + sh <= 4) {
                        struct sym_idx s = sym_idx[rn];
                        s.shift += (uint8_t)sh;
                        fl_mark_read(rn, sym_root, written, &read_before_write, &used_as_nonptr);
                        sym_root[rd] = -1;
                        sym_sreg[rd] = -1;
                        sym_pidx[rd].root = -1;
                        sym_idx[rd] = s;
                        u_bits[rd] = 0;
                        w32_valid &= ~(1u << rd);
                        written |= 1u << rd;
                        write_count[rd]++;
                        last_write_idx[rd] = i;
                        continue;
                    }
                }
            }
            /* 32-bit ADD/SUB/ADDS/SUBS Wd, Wd, #imm: tracks monotonic 32-bit induction step */
            if ((op == 0x11000000 || op == 0x31000000 || op == 0x51000000 || op == 0x71000000) &&
                rd == rn && rd < 31) {
                int32_t imm = (int32_t)(FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0));
                bool is_sub = op == 0x51000000 || op == 0x71000000;
                int32_t delta = is_sub ? -imm : imm;
                fl_mark_read(rn, sym_root, written, &read_before_write, &used_as_nonptr);
                sym_root[rd] = -1;
                sym_sreg[rd] = -1;
                sym_pidx[rd].root = -1;
                sym_idx[rd].reg0 = -1;
                u_bits[rd] = 0;
                int32_t next = w32_step[rd] + delta;
                bool same_dir = w32_step[rd] == 0 || (w32_step[rd] > 0) == (delta > 0);
                if (imm > 0 && same_dir && next >= -4096 && next <= 4096)
                    w32_step[rd] = next;
                else
                    w32_valid &= ~(1u << rd);
                written |= 1u << rd;
                write_count[rd]++;
                last_write_idx[rd] = i;
                continue;
            }
            /* CMP Xn, Xm / CMP Wn, Wm / CMP Xn, #imm / CMP Wn, #imm: may be omitted when it is
             * the loop-closing or header-testing CMP, so defer non-pointer tainting until after
             * control matching. */
            bool is_cmp = (insn & 0x7fe0fc1f) == 0x6b00001f || (insn & 0x7f80001f) == 0x7100001f;
            if (is_cmp && writes_flags) {
                int8_t r0 = rn < 31 ? sym_root[rn] : -1;
                int8_t r1 = -1;
                if (rn < 31 && !(written & (1u << rn)))
                    read_before_write |= 1u << rn;
                if ((insn & 0x7fe0fc1f) == 0x6b00001f) {
                    unsigned rm = FIELD(insn, 20, 16);
                    if (rm < 31) {
                        r1 = sym_root[rm];
                        if (!(written & (1u << rm)))
                            read_before_write |= 1u << rm;
                    }
                }
                if (ncmps < 8) {
                    cmps[ncmps++] = (struct cmp_rec){.idx = i, .root0 = r0, .root1 = r1};
                } else {
                    if (r0 >= 0)
                        used_as_nonptr |= 1u << (unsigned)r0;
                    if (r1 >= 0)
                        used_as_nonptr |= 1u << (unsigned)r1;
                }
                continue;
            }

            /* General DP instruction: reading any register taints its root as non-pointer */
            uint8_t new_ub = (o.writes_rd && rd < 31) ? u_bits_from_dp(insn, u_bits) : 0;
            for (unsigned k = 0; k < o.n; k++) {
                if (o.pos[k] == 0 && o.writes_rd && !o.reads_rd)
                    continue;
                fl_mark_read((insn >> o.pos[k]) & 31, sym_root, written, &read_before_write,
                             &used_as_nonptr);
            }
            if (o.writes_rd && rd < 31) {
                sym_root[rd] = -1;
                sym_sreg[rd] = -1;
                sym_pidx[rd].root = -1;
                sym_idx[rd].reg0 = -1;
                u_bits[rd] = new_ub;
                w32_valid &= ~(1u << rd);
                written |= 1u << rd;
                write_count[rd]++;
                last_write_idx[rd] = i;
            }
            continue;
        }

        return false;
    }

    if (!found_back || !used_as_ptr)
        return false;

    /* Every candidate side/header exit target must point strictly outside [x->pc, back_va + 4). */
    uint64_t loop_end = x->pc + 4ull * (back_idx + 1);
    for (unsigned k = 0; k < ncands; k++) {
        if (cands[k].target >= x->pc && cands[k].target < loop_end)
            return false;
    }

    /* Match the loop-closing (or header-testing) counter and condition */
    *fl = (struct fast_loop){.back_idx = back_idx};
    bool matched_ctrl = false;

    if (back_is_uncond) {
        /* Header-tested while loop: cands[0] must be a clean header exit (before any memory access) */
        const struct cand_se *hdr = &cands[0];
        if (!hdr->had_mem) {
            uint32_t hi = hdr->insn;
            if ((hi & 0x7f000000) == 0x34000000) {
                /* CBZ Xc/Wc, exit: exits when c == 0, continues while c != 0 */
                unsigned c = hi & 31;
                bool is64 = BIT(hi, 31);
                if (c < 31 && write_count[c] == 1 && last_write_idx[c] > hdr->idx &&
                    !(used_as_ptr & (1u << c))) {
                    unsigned ci = last_write_idx[c];
                    uint32_t ci_insn = x->words[ci];
                    uint32_t sub_op = is64 ? 0xd1000000u : 0x51000000u;
                    uint32_t subs_op = is64 ? 0xf1000000u : 0x71000000u;
                    uint32_t add_op = is64 ? 0x91000000u : 0x11000000u;
                    uint32_t adds_op = is64 ? 0xb1000000u : 0x31000000u;
                    uint32_t op = ci_insn & 0xff800000u;
                    if ((ci_insn & 31) == c && FIELD(ci_insn, 9, 5) == c &&
                        (op == sub_op || op == subs_op || op == add_op || op == adds_op)) {
                        uint32_t imm = FIELD(ci_insn, 21, 10) << (BIT(ci_insn, 22) ? 12 : 0);
                        if (imm >= 1 && imm <= 4096) {
                            fl->kind = (op == sub_op || op == subs_op) ? FL_COUNTDOWN : FL_COUNTUP_ZERO;
                            fl->ctrl_idx = ci;
                            fl->cnt_reg = c;
                            fl->is_64 = is64;
                            fl->step = imm;
                            fl->cd_cond = 1; /* NE */
                            fl->header_tested = true;
                            fl->hdr_br_idx = hdr->idx;
                            matched_ctrl = true;
                        }
                    }
                }
            } else if ((hi & 0x7e000000) == 0x36000000) {
                /* TBNZ #63/#31 (exit if <0, continue while >=0) + SUB/SUBS, or
                 * TBZ  #63/#31 (exit if >=0, continue while <0) + ADD/ADDS. */
                unsigned c = hi & 31;
                unsigned bit = (BIT(hi, 31) << 5) | FIELD(hi, 23, 19);
                bool is_tbnz = BIT(hi, 24);
                if (c < 31 && (bit == 63 || bit == 31) && write_count[c] == 1 &&
                    last_write_idx[c] > hdr->idx && !(used_as_ptr & (1u << c))) {
                    bool is64 = bit == 63;
                    unsigned ci = last_write_idx[c];
                    uint32_t ci_insn = x->words[ci];
                    uint32_t op = ci_insn & 0xff800000u;
                    bool ok_sub = is_tbnz && (op == (is64 ? 0xd1000000u : 0x51000000u) ||
                                              op == (is64 ? 0xf1000000u : 0x71000000u));
                    bool ok_add = !is_tbnz && (op == (is64 ? 0x91000000u : 0x11000000u) ||
                                               op == (is64 ? 0xb1000000u : 0x31000000u));
                    if ((ci_insn & 31) == c && FIELD(ci_insn, 9, 5) == c && (ok_sub || ok_add)) {
                        uint32_t imm = FIELD(ci_insn, 21, 10) << (BIT(ci_insn, 22) ? 12 : 0);
                        if (imm >= 1 && imm <= 4096) {
                            fl->kind = ok_sub ? FL_COUNTDOWN : FL_COUNTUP_ZERO;
                            fl->ctrl_idx = ci;
                            fl->cnt_reg = c;
                            fl->is_64 = is64;
                            fl->step = imm;
                            fl->cd_cond = ok_sub ? 5 : 4; /* PL / MI */
                            fl->header_tested = true;
                            fl->hdr_br_idx = hdr->idx;
                            matched_ctrl = true;
                        }
                    }
                }
            } else if ((hi & 0xff000010) == 0x54000000 && hdr->has_flag) {
                unsigned cont_cond = (hi & 15) ^ 1;
                uint32_t fi_insn = x->words[hdr->flag_idx];
                if (fl_match_cmp(fi_insn, hdr->flag_idx, cont_cond, true, hdr->idx,
                                 hdr->written_mask, hdr->sym_root, hdr->sym_sreg, hdr->pidx_root,
                                 hdr->sym_off, sym_root, sym_sreg, sym_pidx, sym_off, w32_valid,
                                 w32_step, written, last_write_idx, used_as_ptr, fl)) {
                    fl->header_tested = true;
                    fl->hdr_br_idx = hdr->idx;
                    fl->omit_ctrl = true;
                    matched_ctrl = true;
                }
            }
        }
    } else if ((back_insn & 0x7f000000) == 0x35000000) {
        /* CBNZ Xc/Wc: SUB/SUBS (countdown) or ADD/ADDS (negative counter counting up to 0) */
        unsigned c = back_insn & 31;
        bool is64 = BIT(back_insn, 31);
        if (c < 31 && write_count[c] == 1 && !(used_as_ptr & (1u << c))) {
            unsigned ci = last_write_idx[c];
            uint32_t ci_insn = x->words[ci];
            uint32_t sub_op = is64 ? 0xd1000000u : 0x51000000u;
            uint32_t subs_op = is64 ? 0xf1000000u : 0x71000000u;
            uint32_t add_op = is64 ? 0x91000000u : 0x11000000u;
            uint32_t adds_op = is64 ? 0xb1000000u : 0x31000000u;
            uint32_t op = ci_insn & 0xff800000u;
            if ((ci_insn & 31) == c && FIELD(ci_insn, 9, 5) == c &&
                (op == sub_op || op == subs_op || op == add_op || op == adds_op)) {
                uint32_t imm = FIELD(ci_insn, 21, 10) << (BIT(ci_insn, 22) ? 12 : 0);
                if (imm >= 1 && imm <= 4096) {
                    fl->kind = (op == sub_op || op == subs_op) ? FL_COUNTDOWN : FL_COUNTUP_ZERO;
                    fl->ctrl_idx = ci;
                    fl->cnt_reg = c;
                    fl->is_64 = is64;
                    fl->step = imm;
                    fl->cd_cond = 1; /* NE */
                    matched_ctrl = true;
                }
            }
        }
    } else if ((back_insn & 0x7e000000) == 0x36000000) {
        /* TBZ Xc/Wc, #63/#31 (continue while >=0) + SUB/SUBS, or
         * TBNZ Xc/Wc, #63/#31 (continue while <0) + ADD/ADDS. */
        unsigned c = back_insn & 31;
        unsigned bit = (BIT(back_insn, 31) << 5) | FIELD(back_insn, 23, 19);
        bool is_tbnz = BIT(back_insn, 24);
        if (c < 31 && (bit == 63 || bit == 31) && write_count[c] == 1 &&
            !(used_as_ptr & (1u << c))) {
            bool is64 = bit == 63;
            unsigned ci = last_write_idx[c];
            uint32_t ci_insn = x->words[ci];
            uint32_t op = ci_insn & 0xff800000u;
            bool ok_sub = !is_tbnz && (op == (is64 ? 0xd1000000u : 0x51000000u) ||
                                       op == (is64 ? 0xf1000000u : 0x71000000u));
            bool ok_add = is_tbnz && (op == (is64 ? 0x91000000u : 0x11000000u) ||
                                      op == (is64 ? 0xb1000000u : 0x31000000u));
            if ((ci_insn & 31) == c && FIELD(ci_insn, 9, 5) == c && (ok_sub || ok_add)) {
                uint32_t imm = FIELD(ci_insn, 21, 10) << (BIT(ci_insn, 22) ? 12 : 0);
                if (imm >= 1 && imm <= 4096) {
                    fl->kind = ok_sub ? FL_COUNTDOWN : FL_COUNTUP_ZERO;
                    fl->ctrl_idx = ci;
                    fl->cnt_reg = c;
                    fl->is_64 = is64;
                    fl->step = imm;
                    fl->cd_cond = ok_sub ? 5 : 4; /* PL / MI */
                    matched_ctrl = true;
                }
            }
        }
    } else if ((back_insn & 0xff000010) == 0x54000000 && flag_writers >= 1 &&
               !nzcv_read_after_last_write) {
        unsigned cond = back_insn & 15;
        unsigned flag_idx = last_flag_idx;
        uint32_t fi_insn = x->words[flag_idx];
        /* Pattern 1: SUBS Xc/Wc, Xc/Wc, #imm + B.NE/HI/GT/GE/PL */
        if (((fi_insn & 0x7f800000) == 0x71000000) && (fi_insn & 31) < 31 &&
            (fi_insn & 31) == FIELD(fi_insn, 9, 5) &&
            (cond == 1 || cond == 5 || cond == 8 || cond == 10 || cond == 12)) {
            unsigned c = fi_insn & 31;
            uint32_t imm = FIELD(fi_insn, 21, 10) << (BIT(fi_insn, 22) ? 12 : 0);
            if (write_count[c] == 1 && !(used_as_ptr & (1u << c)) && imm >= 1 && imm <= 4096) {
                fl->kind = FL_COUNTDOWN;
                fl->ctrl_idx = flag_idx;
                fl->cnt_reg = c;
                fl->is_64 = BIT(fi_insn, 31);
                fl->step = imm;
                fl->cd_cond = cond;
                matched_ctrl = true;
            }
        }
        /* Pattern 1b: ADDS Xc/Wc, Xc/Wc, #imm + B.NE/MI/LT (negative counter counting up to 0) */
        if (!matched_ctrl && ((fi_insn & 0x7f800000) == 0x31000000) && (fi_insn & 31) < 31 &&
            (fi_insn & 31) == FIELD(fi_insn, 9, 5) && (cond == 1 || cond == 4 || cond == 11)) {
            unsigned c = fi_insn & 31;
            uint32_t imm = FIELD(fi_insn, 21, 10) << (BIT(fi_insn, 22) ? 12 : 0);
            if (write_count[c] == 1 && !(used_as_ptr & (1u << c)) && imm >= 1 && imm <= 4096) {
                fl->kind = FL_COUNTUP_ZERO;
                fl->ctrl_idx = flag_idx;
                fl->cnt_reg = c;
                fl->is_64 = BIT(fi_insn, 31);
                fl->step = imm;
                fl->cd_cond = cond;
                matched_ctrl = true;
            }
        }
        /* Pattern 2: CMP reg, reg or CMP reg, #imm + B.cond (incrementing or decrementing) */
        if (!matched_ctrl &&
            fl_match_cmp(fi_insn, flag_idx, cond, false, 0, 0, NULL, NULL, NULL, NULL, sym_root,
                         sym_sreg, sym_pidx, sym_off, w32_valid, w32_step, written, last_write_idx,
                         used_as_ptr, fl)) {
            fl->omit_ctrl = true;
            matched_ctrl = true;
        }
    }

    if (!matched_ctrl)
        return false;

    /* Collect kept early-exit side branches (excluding cands[0] when it is hdr_br_idx) */
    unsigned se_start = fl->header_tested ? 1u : 0u;
    if (ncands - se_start > FAST_LOOP_MAX_SIDE_EXITS)
        return false;
    for (unsigned k = se_start; k < ncands; k++) {
        fl->side_exits[fl->nside_exits++] = (struct fast_side_exit){
            .idx = (uint16_t)cands[k].idx,
            .va = cands[k].va,
            .target = cands[k].target,
        };
    }

    /* Decide whether fl->ctrl_idx (CMP) can safely be omitted in the fast loop body without
     * breaking NZCV at any side exit or later NZCV-reading instruction. */
    if (fl->omit_ctrl) {
        if (fl->header_tested) {
            if (hdr_flag_read_by_other)
                fl->omit_ctrl = false;
            for (unsigned k = se_start; k < ncands && fl->omit_ctrl; k++) {
                if (cands[k].flag_seq == cands[0].flag_seq)
                    fl->omit_ctrl = false;
            }
        } else {
            for (unsigned k = se_start; k < ncands && fl->omit_ctrl; k++) {
                if (!cands[k].has_flag || cands[k].idx > fl->ctrl_idx)
                    fl->omit_ctrl = false;
            }
        }
    }

    /* Any CMP that is NOT omitted executes in the fast body and reads its operands as non-pointers. */
    for (unsigned k = 0; k < ncmps; k++) {
        if (fl->omit_ctrl && cmps[k].idx == fl->ctrl_idx)
            continue;
        if (cmps[k].root0 >= 0)
            used_as_nonptr |= 1u << (unsigned)cmps[k].root0;
        if (cmps[k].root1 >= 0)
            used_as_nonptr |= 1u << (unsigned)cmps[k].root1;
    }

    /* Validate every root base pointer (0..30 or 31 = SP) and collect live end-of-iteration aliases */
    int8_t ptr_map[32];
    for (unsigned r = 0; r < 32; r++)
        ptr_map[r] = -1;
    for (uint32_t bits = used_as_ptr; bits; bits &= bits - 1) {
        unsigned r = (unsigned)__builtin_ctz(bits);
        if (fl->nptrs >= FAST_LOOP_MAX_PTRS)
            return false;
        if (r == 31) {
            fl->uses_sp = true;
            ptr_map[31] = (int8_t)fl->nptrs;
            fl->ptrs[fl->nptrs++] = (struct fast_ptr){.reg = 31, .alias_mask = 0};
            continue;
        }
        if (used_as_nonptr & (1u << r))
            return false;
        if (sym_root[r] != (int8_t)r || sym_pidx[r].root >= 0)
            return false;
        if (sym_sreg[r] >= 0) {
            unsigned sr = (unsigned)sym_sreg[r];
            if (sym_off[r] != 0 || (written & (1u << sr)) || (used_as_ptr & (1u << sr)))
                return false;
        } else if (sym_off[r] < -4095 || sym_off[r] > 4095) {
            return false;
        }
        uint32_t alias_mask = 0;
        for (unsigned a = 0; a < 31; a++) {
            if (a != r && sym_root[a] == (int8_t)r) {
                /* An alias carrying addend_r at iteration end must not have been read before its
                 * first write (otherwise iteration 2 would observe the shifted pointer value). */
                if (read_before_write & (1u << a))
                    return false;
                alias_mask |= 1u << a;
            }
        }
        ptr_map[r] = (int8_t)fl->nptrs;
        fl->ptrs[fl->nptrs++] = (struct fast_ptr){
            .reg = (uint8_t)r,
            .alias_mask = alias_mask,
        };
    }

    if (fl->nptrs + nlits > FAST_LOOP_MAX_PTRS)
        return false;
    fl->nlits = nlits;
    for (unsigned l = 0; l < nlits; l++)
        fl->lit_addr[l] = lit_addrs[l];

    /* For every kept early-exit side branch, verify that the exact same set of roots and aliases
     * carry host addends at the side exit as at iteration end (so one shared epilogue unshifts
     * every live pointer/alias exactly, even on iteration 0). */
    for (unsigned k = se_start; k < ncands; k++) {
        const struct cand_se *ce = &cands[k];
        for (unsigned i = 0; i < fl->nptrs; i++) {
            unsigned r = fl->ptrs[i].reg;
            if (r < 31 && ce->sym_root[r] != (int8_t)r)
                return false;
        }
        for (unsigned a = 0; a < 31; a++) {
            int8_t r_se = (ce->sym_root[a] >= 0 && (used_as_ptr & (1u << (unsigned)ce->sym_root[a])) &&
                           a != (unsigned)ce->sym_root[a])
                              ? ce->sym_root[a]
                              : -1;
            int8_t r_end = (sym_root[a] >= 0 && (used_as_ptr & (1u << (unsigned)sym_root[a])) &&
                            a != (unsigned)sym_root[a])
                               ? sym_root[a]
                               : -1;
            if (r_se != r_end)
                return false;
            if (r_se >= 0 && !(ce->written_mask & (1u << a)))
                return false;
        }
    }

    /* Validate every access stream and resolve its per-iteration byte stride */
    bool has_nonzero_stride = false;
    uint32_t ptr_seen = 0;
    for (unsigned s = 0; s < nraw; s++) {
        unsigned r = raw[s].root;
        unsigned pi = (unsigned)ptr_map[r];
        uint32_t span = (uint32_t)(raw[s].hi - raw[s].lo);
        if (span == 0 || span > APPINE_PAGE_SIZE)
            return false;
        uint8_t sreg = (r < 31 && sym_sreg[r] >= 0) ? (uint8_t)sym_sreg[r] : 31;
        int64_t cstride = (r == 31 || sreg < 31) ? 0 : sym_off[r];
        int32_t idx_w32_step = 0, idx_w32_pre_off = 0;
        unsigned idx_regs[2] = {raw[s].idx_reg, raw[s].idx_reg2};
        for (unsigned k = 0; k < 2; k++) {
            unsigned ir = idx_regs[k];
            if (ir < 31 && (written & (1u << ir))) {
                if (sreg < 31)
                    return false;
                if (raw[s].idx_opt == 3 || raw[s].idx_opt == 7) {
                    if (sym_root[ir] != (int8_t)ir || sym_sreg[ir] >= 0 || sym_pidx[ir].root >= 0 ||
                        sym_off[ir] == 0 || sym_off[ir] < -4096 || sym_off[ir] > 4096)
                        return false;
                    cstride += sym_off[ir] * (int64_t)(1u << raw[s].idx_shift);
                } else if (raw[s].idx_opt == 2 || raw[s].idx_opt == 6) {
                    if (raw[s].idx_reg2 < 31 || !((w32_valid >> ir) & 1) || w32_step[ir] == 0)
                        return false;
                    cstride += (int64_t)w32_step[ir] * (int64_t)(1u << raw[s].idx_shift);
                    idx_w32_step = w32_step[ir];
                    idx_w32_pre_off = raw[s].has_w32_pre ? raw[s].w32_pre_off : 0;
                } else {
                    return false;
                }
            }
        }
        if (sreg < 31) {
            if (span >= APPINE_PAGE_SIZE)
                return false;
            has_nonzero_stride = true;
        } else if (cstride != 0) {
            uint64_t abs_stride = (uint64_t)(cstride < 0 ? -cstride : cstride);
            if (abs_stride > 4095 || abs_stride + span > APPINE_PAGE_SIZE)
                return false; /* fewer than 2 iterations could ever fit in one page, or >34b recip */
            has_nonzero_stride = true;
        }
        bool first = !(ptr_seen & (1u << pi));
        ptr_seen |= 1u << pi;
        fl->streams[fl->nstreams++] = (struct fast_stream){
            .ptr_idx = (uint8_t)pi,
            .idx_reg = raw[s].idx_reg,
            .idx_reg2 = raw[s].idx_reg2,
            .idx_opt = raw[s].idx_opt,
            .idx_shift = raw[s].idx_shift,
            .stride_reg = sreg,
            .write = raw[s].write,
            .first_for_ptr = first,
            .lo = raw[s].lo,
            .hi = raw[s].hi,
            .stride = (int32_t)cstride,
            .idx_w32_step = idx_w32_step,
            .idx_w32_pre_off = idx_w32_pre_off,
        };
    }

    if (!has_nonzero_stride)
        return false;

    /* Pick a power-of-2 KICK_BUDGET in [32, 4096] keeping <= 12,288 guest insns per fast chunk */
    unsigned budget_log2 = 12; /* 4096 */
    while (budget_log2 > 5 && ((1u << budget_log2) * (back_idx + 1) > 12288))
        budget_log2--;
    fl->kick_log2 = budget_log2;

    /* Pin all stolen GPRs touched in the fast body so the native inner body has zero CTX traffic:
     * the first 2 use T0/T1 directly, and any additional stolen GPRs (plus M_LITERAL host bases)
     * borrow unused non-stolen guest registers saved/restored in their dead cpu->x[] slots. */
    uint32_t body_stolen = 0, body_used = 0;
    for (unsigned i = 0; i < back_idx; i++) {
        uint32_t insn = x->words[i];
        if (insn == 0xd503201f)
            continue;
        if (fl->header_tested && i == fl->hdr_br_idx)
            continue;
        bool omitted_cmp = fl->omit_ctrl && i == fl->ctrl_idx;
        if ((insn & 0xff000010) == 0x54000000) /* B.cond side exit */
            continue;
        if ((insn & 0x7e000000) == 0x34000000 || (insn & 0x7e000000) == 0x36000000) {
            unsigned rt = insn & 31;
            if (rt < 31) {
                body_used |= 1u << rt;
                if (is_stolen(rt))
                    body_stolen |= 1u << rt;
            }
            continue;
        }
        unsigned top = FIELD(insn, 28, 25);
        if ((top & 7) == 7) {
            struct native_desc nd;
            appine_simd_dp_decode(insn, &nd);
            for (unsigned k = 0; k < nd.n; k++) {
                unsigned r = (insn >> nd.gpr[k].pos) & 31;
                if (r < 31) {
                    body_used |= 1u << r;
                    if (!omitted_cmp && is_stolen(r))
                        body_stolen |= 1u << r;
                }
            }
        } else if (top == 0x4 || top == 0x6 || top == 0xc || top == 0xe) {
            struct memop m;
            uint64_t va = x->pc + 4ull * i;
            if (BIT(insn, 26))
                dbt_decode_simd_memop(insn, va, &m);
            else
                dbt_decode_memop(insn, va, &m);
            if (m.prefetch)
                continue;
            if (m.mode == M_LITERAL) {
                if (!m.simd && m.rt < 31) {
                    body_used |= 1u << m.rt;
                    if (is_stolen(m.rt))
                        body_stolen |= 1u << m.rt;
                }
                continue;
            }
            if (m.rn < 31) {
                body_used |= 1u << m.rn;
                if (is_stolen(m.rn))
                    body_stolen |= 1u << m.rn;
            }
            if (m.mode == M_REG && m.rm < 31) {
                body_used |= 1u << m.rm;
                if (is_stolen(m.rm))
                    body_stolen |= 1u << m.rm;
            }
            if (!m.simd) {
                if (m.rt < 31) {
                    body_used |= 1u << m.rt;
                    if (is_stolen(m.rt))
                        body_stolen |= 1u << m.rt;
                }
                if (m.pair && m.rt2 < 31) {
                    body_used |= 1u << m.rt2;
                    if (is_stolen(m.rt2))
                        body_stolen |= 1u << m.rt2;
                }
            }
        } else if (FIELD(insn, 28, 24) == 0x10) {
            unsigned rd = insn & 31;
            if (rd < 31) {
                body_used |= 1u << rd;
                if (is_stolen(rd))
                    body_stolen |= 1u << rd;
            }
        } else {
            struct operands o;
            dbt_dp_operands(insn, &o);
            for (unsigned k = 0; k < o.n; k++) {
                unsigned r = (insn >> o.pos[k]) & 31;
                if (r < 31) {
                    body_used |= 1u << r;
                    if (!omitted_cmp && is_stolen(r))
                        body_stolen |= 1u << r;
                }
            }
        }
    }
    if (body_stolen || fl->nlits) {
        if (fl->cnt_reg < 31)
            body_used |= 1u << fl->cnt_reg;
        if ((fl->kind == FL_CMP_INC || fl->kind == FL_CMP_DEC) && !fl->bound_is_imm &&
            fl->bound_reg < 31)
            body_used |= 1u << fl->bound_reg;
        for (unsigned i = 0; i < fl->nptrs; i++) {
            if (fl->ptrs[i].reg < 31)
                body_used |= 1u << fl->ptrs[i].reg;
            body_used |= fl->ptrs[i].alias_mask;
        }
        const unsigned base_pool[2] = {T0, T1};
        unsigned base_used = 0;
        uint32_t avail = ((1u << 31) - 1) & ~body_used;
        unsigned n = 0, nb = 0;
        uint8_t preg[6], phost[6], bhost[8];
        bool pwr[6];
        bool ok = true;
        for (uint32_t bits = body_stolen; bits; bits &= bits - 1) {
            unsigned r = (unsigned)__builtin_ctz(bits);
            unsigned hr = 31;
            if (base_used < 2) {
                hr = base_pool[base_used++];
            } else {
                for (uint32_t cand = avail; cand; cand &= cand - 1) {
                    unsigned c = (unsigned)__builtin_ctz(cand);
                    if (!is_stolen(c)) {
                        hr = c;
                        avail &= ~(1u << c);
                        bhost[nb++] = (uint8_t)c;
                        break;
                    }
                }
            }
            if (hr >= 31 || n >= 6) {
                ok = false;
                break;
            }
            preg[n] = (uint8_t)r;
            phost[n] = (uint8_t)hr;
            pwr[n] = ((written >> r) & 1) != 0;
            n++;
        }
        uint8_t lhost[FAST_LOOP_MAX_LITS];
        for (unsigned l = 0; l < fl->nlits && ok; l++) {
            unsigned hr = 31;
            if (base_used < 2) {
                hr = base_pool[base_used++];
            } else if (nb < 8) {
                for (uint32_t cand = avail; cand; cand &= cand - 1) {
                    unsigned c = (unsigned)__builtin_ctz(cand);
                    if (!is_stolen(c)) {
                        hr = c;
                        avail &= ~(1u << c);
                        bhost[nb++] = (uint8_t)c;
                        break;
                    }
                }
            }
            if (hr >= 31) {
                ok = false;
                break;
            }
            lhost[l] = (uint8_t)hr;
        }
        if (ok) {
            fl->npin = n;
            for (unsigned k = 0; k < n; k++) {
                fl->pin_reg[k] = preg[k];
                fl->pin_host[k] = phost[k];
                fl->pin_written[k] = pwr[k];
            }
            fl->nborrow = nb;
            for (unsigned k = 0; k < nb; k++)
                fl->borrow_host[k] = bhost[k];
            for (unsigned l = 0; l < fl->nlits; l++)
                fl->lit_host[l] = lhost[l];
            body_words = back_idx + extra_body_words;
        } else if (fl->nlits) {
            return false; /* M_LITERAL requires dedicated host address registers */
        }
    }

    /* Worst-case host words for prologue + fast body + epilogues must leave room for slow_loop */
    unsigned se_words = fl->nside_exits
                            ? (fl->nside_exits * 8 + fl->nptrs * 6 + fl->npin * 4 + fl->nborrow + 20)
                            : 0;
    unsigned est_words = 40 + fl->nstreams * 56 + fl->nlits * 14 + fl->nptrs * 10 +
                         fl->npin * 6 + fl->nborrow * 2 + body_words + se_words + 64;
    if (est_words > FAST_LOOP_MAX_WORDS)
        return false;

    return true;
}

static bool fast_is_pinned(const struct fast_loop *fl, unsigned r)
{
    for (unsigned k = 0; k < fl->npin; k++) {
        if (fl->pin_reg[k] == r)
            return true;
    }
    return false;
}

static unsigned fast_pin_host(const struct fast_loop *fl, unsigned r)
{
    for (unsigned k = 0; k < fl->npin; k++) {
        if (fl->pin_reg[k] == r)
            return fl->pin_host[k];
    }
    return r;
}

static int fast_ptr_index(const struct fast_loop *fl, unsigned r)
{
    for (unsigned i = 0; i < fl->nptrs; i++) {
        if (fl->ptrs[i].reg == r)
            return (int)i;
    }
    return -1;
}

static int fast_ptr_or_alias_index(const struct fast_loop *fl, unsigned r)
{
    for (unsigned i = 0; i < fl->nptrs; i++) {
        if (fl->ptrs[i].reg == r || ((fl->ptrs[i].alias_mask >> r) & 1))
            return (int)i;
    }
    return -1;
}

/* Emit a data-processing / SIMD-DP instruction inside fast_loop_body using only T0 and T1 (T2 holds
 * the strip-mine trip counter) */
static void emit_fast_operands(struct xlate *x, uint32_t insn, const struct operands *o)
{
    const unsigned pool[2] = {T0, T1};
    unsigned guest[2], nmap = 0, rd_host = 0, rd_guest = 0;
    bool need_load[2] = {false, false};
    bool rd_stolen = false;
    uint32_t out = insn;
    for (unsigned i = 0; i < o->n; i++) {
        unsigned r = (insn >> o->pos[i]) & 31;
        if (!is_stolen(r))
            continue;
        unsigned k;
        for (k = 0; k < nmap && guest[k] != r; k++)
            ;
        if (k == nmap) {
            if (nmap == 2)
                appine_guard_fail();
            guest[nmap++] = r;
        }
        out = (out & ~(31u << o->pos[i])) | pool[k] << o->pos[i];
        if (o->pos[i] == 0 && o->writes_rd) {
            rd_stolen = true;
            rd_host = pool[k];
            rd_guest = r;
            if (o->reads_rd)
                need_load[k] = true;
        } else {
            need_load[k] = true;
        }
    }
    for (unsigned k = 0; k < nmap; k++) {
        if (need_load[k])
            emit_load_guest(x, pool[k], guest[k]);
    }
    emit(x, out);
    if (rd_stolen)
        emit_store_guest(x, rd_host, rd_guest);
}

/* Emit a direct host memory access inside fast_loop_body (Rn already holds a host address in its
 * register / SP_HOST, or is stolen in cpu->x[Rn]); uses only T0 and T1 so T2 stays intact */
static void emit_fast_memop(struct xlate *x, uint32_t insn, const struct memop *m)
{
    bool rn_sp = m->rn == 31;
    bool rn_s = !rn_sp && is_stolen(m->rn);
    bool rm_s = m->mode == M_REG && m->rm < 31 && is_stolen(m->rm);
    bool rt_s = !m->simd && m->rt < 31 && is_stolen(m->rt);
    bool rt2_s = m->pair && !m->simd && m->rt2 < 31 && is_stolen(m->rt2);
    if (!rn_sp && !rn_s && !rm_s && !rt_s && !rt2_s) {
        emit(x, insn);
        return;
    }
    if (m->mode == M_REG) {
        const unsigned pool[2] = {T0, T1};
        unsigned guest[2], nmap = 0;
        unsigned hrn = rn_sp ? SP_HOST : m->rn, hrm = m->rm, hrt = m->rt;
        if (rn_s) {
            guest[nmap++] = m->rn;
            hrn = pool[0];
            emit_load_guest(x, hrn, m->rn);
        }
        if (rm_s) {
            unsigned k;
            for (k = 0; k < nmap && guest[k] != m->rm; k++)
                ;
            if (k == nmap) {
                if (nmap == 2)
                    appine_guard_fail();
                guest[nmap++] = m->rm;
                emit_load_guest(x, pool[k], m->rm);
            }
            hrm = pool[k];
        }
        if (rt_s) {
            unsigned k;
            for (k = 0; k < nmap && guest[k] != m->rt; k++)
                ;
            if (k == nmap) {
                if (nmap == 2)
                    appine_guard_fail();
                guest[nmap++] = m->rt;
                if (!m->load)
                    emit_load_guest(x, pool[k], m->rt);
            }
            hrt = pool[k];
        }
        uint32_t out = (insn & ~(31u << 16 | 31u << 5)) | (hrm << 16) | (hrn << 5);
        if (!m->simd)
            out = (out & ~31u) | hrt;
        emit(x, out);
        if (rt_s && m->load)
            emit_store_guest(x, hrt, m->rt);
        return;
    }
    if (m->simd) {
        unsigned hrn = rn_sp ? SP_HOST : T0;
        if (rn_s)
            emit_load_guest(x, T0, m->rn);
        emit(x, (insn & ~(31u << 5)) | (hrn << 5));
        if (rn_s && (m->mode == M_PRE || m->mode == M_POST))
            emit_store_guest(x, T0, m->rn);
        return;
    }
    if (!m->pair) {
        unsigned hrn = rn_sp ? SP_HOST : rn_s ? T0 : m->rn;
        unsigned hrt = rt_s ? T1 : m->rt;
        if (rn_s)
            emit_load_guest(x, T0, m->rn);
        if (rt_s && !m->load)
            emit_load_guest(x, T1, m->rt);
        emit(x, (insn & ~(31u << 5 | 31u)) | (hrn << 5) | hrt);
        if (rn_s && (m->mode == M_PRE || m->mode == M_POST))
            emit_store_guest(x, T0, m->rn);
        if (rt_s && m->load)
            emit_store_guest(x, T1, m->rt);
        return;
    }
    /* Integer LDP/STP/LDPSW with <= 2 distinct stolen GPRs */
    unsigned hrn = rn_sp ? SP_HOST : rn_s ? T0 : m->rn;
    unsigned hrt = m->rt, hrt2 = m->rt2;
    if (rn_s) {
        emit_load_guest(x, T0, m->rn);
        if (rt_s)
            hrt = T1;
        if (rt2_s)
            hrt2 = T1;
    } else {
        if (rt_s)
            hrt = T0;
        if (rt2_s)
            hrt2 = (rt_s && m->rt2 == m->rt) ? T0 : (rt_s ? T1 : T0);
    }
    if (!m->load) {
        if (rt_s)
            emit_load_guest(x, hrt, m->rt);
        if (rt2_s && (!rt_s || m->rt2 != m->rt))
            emit_load_guest(x, hrt2, m->rt2);
    }
    emit(x, (insn & ~(31u << 10 | 31u << 5 | 31u)) | (hrt2 << 10) | (hrn << 5) | hrt);
    if (rn_s && (m->mode == M_PRE || m->mode == M_POST))
        emit_store_guest(x, T0, m->rn);
    if (m->load) {
        if (rt_s)
            emit_store_guest(x, hrt, m->rt);
        if (rt2_s)
            emit_store_guest(x, hrt2, m->rt2);
    }
}

/* Add or subtract T0 to/from guest register r (0..30 or 31 = SP) */
static void emit_adjust_reg(struct xlate *x, unsigned r, bool sub)
{
    if (r == 31) {
        emit(x, sub ? enc_sub_reg(SP_HOST, SP_HOST, T0) : enc_add_lsl(SP_HOST, SP_HOST, T0, 0));
    } else if (!is_stolen(r)) {
        emit(x, sub ? enc_sub_reg(r, r, T0) : enc_add_lsl(r, r, T0, 0));
    } else {
        emit_load_guest(x, T1, r);
        emit(x, sub ? enc_sub_reg(T1, T1, T0) : enc_add_lsl(T1, T1, T0, 0));
        emit_store_guest(x, T1, r);
    }
}

/* Emit a direct host load for an M_LITERAL instruction using its pre-validated host address reg */
static void emit_fast_literal(struct xlate *x, const struct fast_loop *fl, uint32_t insn,
                              const struct memop *m)
{
    uint64_t addr = (uint64_t)m->imm;
    unsigned li = 0;
    while (li < fl->nlits && fl->lit_addr[li] != addr)
        li++;
    if (li >= fl->nlits)
        appine_guard_fail();
    unsigned hbase = fl->lit_host[li];
    unsigned opc = FIELD(insn, 31, 30);
    if (m->simd) {
        static const uint32_t base_op[3] = {0xbd400000u, 0xfd400000u, 0x3dc00000u};
        emit(x, base_op[opc] | (hbase << 5) | m->rt);
    } else {
        static const uint32_t base_op[3] = {0xb9400000u, 0xf9400000u, 0xb9800000u};
        unsigned hrt = m->rt < 31 ? fast_pin_host(fl, m->rt) : 31;
        emit(x, base_op[opc] | (hbase << 5) | hrt);
    }
}

/* Strictly NZCV-free epilogue: flush written pinned stolen registers (unshifting pointers/aliases),
 * restore borrowed non-stolen host registers, and restore non-pinned root pointers and live aliases
 * to guest VAs. Preserves dbt_groups[7] (used by multi-target side exits). */
static void emit_fast_epilogue(struct xlate *x, const struct fast_loop *fl, size_t grp_off)
{
    for (unsigned k = 0; k < fl->npin; k++) {
        if (!fl->pin_written[k])
            continue;
        unsigned gr = fl->pin_reg[k], hr = fl->pin_host[k];
        int pi = fast_ptr_or_alias_index(fl, gr);
        if (pi >= 0) {
            emit(x, enc_ldr_x(T2, CTX, grp_off + 8 * (size_t)pi));
            emit(x, enc_sub_reg(hr, hr, T2));
        }
        emit_store_guest(x, hr, gr);
    }
    for (unsigned b = 0; b < fl->nborrow; b++)
        emit_load_guest(x, fl->borrow_host[b], fl->borrow_host[b]);
    for (unsigned i = 0; i < fl->nptrs; i++) {
        const struct fast_ptr *p = &fl->ptrs[i];
        bool load_needed = p->reg == 31 || !fast_is_pinned(fl, p->reg);
        uint32_t nonpin_aliases = 0;
        for (uint32_t aliases = p->alias_mask; aliases; aliases &= aliases - 1) {
            unsigned a = (unsigned)__builtin_ctz(aliases);
            if (!fast_is_pinned(fl, a))
                nonpin_aliases |= 1u << a;
        }
        if (!load_needed && !nonpin_aliases)
            continue;
        emit(x, enc_ldr_x(T0, CTX, grp_off + 8 * (size_t)i));
        if (load_needed)
            emit_adjust_reg(x, p->reg, true);
        for (uint32_t aliases = nonpin_aliases; aliases; aliases &= aliases - 1)
            emit_adjust_reg(x, (unsigned)__builtin_ctz(aliases), true);
    }
}

static void emit_fast_loop(struct xlate *x, const struct fast_loop *fl)
{
    uint32_t *bails[192];
    unsigned nbails = 0;
    size_t grp_off = offsetof(struct appine_cpu, dbt_groups);
    size_t iters_off = grp_off + 8 * 7;

    /* 1. Never bypass active hardware watchpoints; if SP is accessed, require 16-byte alignment */
    size_t nwatch = offsetof(struct appine_cpu, nwatch);
    emit_add_const(x, T0, CTX, (int64_t)(nwatch & ~(size_t)0xfff));
    emit(x, enc_ldr_w(T0, T0, nwatch & 0xfff));
    bails[nbails++] = here(x);
    emit(x, enc_cbnz_x(T0));
    if (fl->uses_sp) {
        emit(x, enc_and_low(T0, SP_HOST, 4));
        bails[nbails++] = here(x);
        emit(x, enc_cbnz_x(T0));
    }

    /* 2. Compute safe_iters into T2 (NZCV-free), clamp to KICK_BUDGET, require >= 2 */
    if (fl->kind == FL_COUNTDOWN || fl->kind == FL_COUNTUP_ZERO) {
        unsigned src = fl->cnt_reg;
        if (is_stolen(src)) {
            emit_load_guest(x, T2, src);
            src = T2;
        }
        if (fl->kind == FL_COUNTDOWN) {
            bool is_signed = fl->cd_cond == 5 || fl->cd_cond == 10 || fl->cd_cond == 12;
            if (!fl->is_64)
                emit(x, is_signed ? enc_sbfx(T2, src, 0, 32) : enc_ubfx(T2, src, 0, 32));
            else if (src != T2)
                emit(x, enc_mov(T2, src));
            if (is_signed) {
                bails[nbails++] = here(x);
                emit(x, enc_tbnz(T2, 63));
            }
            if (fl->cd_cond == 1 || fl->cd_cond == 8 || fl->cd_cond == 12) { /* NE/HI/GT */
                bails[nbails++] = here(x);
                emit(x, enc_cbz_x(T2));
                emit(x, enc_sub_imm(T2, T2, 1, 0));
            }
        } else { /* FL_COUNTUP_ZERO: negative counter advancing up to 0 */
            bool is_signed = fl->cd_cond == 4 || fl->cd_cond == 11; /* MI / LT */
            if (!fl->is_64)
                emit(x, is_signed ? enc_sbfx(T2, src, 0, 32) : enc_ubfx(T2, src, 0, 32));
            else if (src != T2)
                emit(x, enc_mov(T2, src));
            if (is_signed) {
                bails[nbails++] = here(x);
                emit(x, enc_tbz(T2, 63));
            }
            emit(x, enc_sub_reg(T2, 31, T2)); /* dist = -cnt (mod 2^W) */
            if (!fl->is_64 && !is_signed)
                emit(x, enc_ubfx(T2, T2, 0, 32));
            bails[nbails++] = here(x);
            emit(x, enc_cbz_x(T2));
            emit(x, enc_sub_imm(T2, T2, 1, 0));
        }
        if (fl->step > 1) {
            if ((fl->step & (fl->step - 1)) == 0) {
                unsigned sh = (unsigned)__builtin_ctz(fl->step);
                emit(x, enc_ubfx(T2, T2, sh, 64 - sh));
            } else {
                emit_mov_const(x, T0, fl->step);
                emit(x, enc_udiv(T2, T2, T0));
            }
        }
        if (fl->header_tested) {
            /* T2 = q = full_iters - 1: bail if q == 0 (full_iters < 2), clamp q, add 1 */
            bails[nbails++] = here(x);
            emit(x, enc_cbz_x(T2));
            emit(x, enc_ubfx(T0, T2, fl->kick_log2, 64 - fl->kick_log2));
            uint32_t *no_clamp = here(x);
            emit(x, enc_cbz_x(T0));
            emit(x, enc_movz(T2, (1u << fl->kick_log2) - 1, 0));
            patch_local(x, no_clamp, here(x));
            emit(x, enc_add_imm(T2, T2, 1, 0));
        } else {
            /* T2 = rem_backedges: clamp to 1 << kick_log2, bail if < 2 */
            emit(x, enc_ubfx(T0, T2, fl->kick_log2, 64 - fl->kick_log2));
            uint32_t *no_clamp = here(x);
            emit(x, enc_cbz_x(T0));
            emit(x, enc_movz(T2, 1u << fl->kick_log2, 0));
            patch_local(x, no_clamp, here(x));
            emit(x, enc_ubfx(T0, T2, 1, 63));
            bails[nbails++] = here(x);
            emit(x, enc_cbz_x(T0));
        }
    } else {
        /* FL_CMP_INC: diff = bound - v_0 (A=bound in T0, B=v_0 in T1).
         * FL_CMP_DEC: diff = v_0 - bound (A=v_0 in T0, B=bound in T1). */
        bool is_signed = fl->cmp == FL_SLT || fl->cmp == FL_SLE;
        unsigned reg_bound = fl->kind == FL_CMP_DEC ? T1 : T0;
        unsigned reg_v0 = fl->kind == FL_CMP_DEC ? T0 : T1;
        if (fl->bound_is_imm) {
            emit_mov_const(x, reg_bound, fl->bound_imm);
        } else {
            unsigned b = fl->bound_reg;
            if (is_stolen(b)) {
                emit_load_guest(x, reg_bound, b);
                b = reg_bound;
            }
            if (!fl->is_64)
                emit(x, is_signed ? enc_sbfx(reg_bound, b, 0, 32) : enc_ubfx(reg_bound, b, 0, 32));
            else if (b != reg_bound)
                emit(x, enc_mov(reg_bound, b));
        }
        unsigned c = fl->cnt_reg;
        if (is_stolen(c)) {
            emit_load_guest(x, reg_v0, c);
            c = reg_v0;
        }
        emit_add_const(x, reg_v0, c, fl->cmp_off);
        if (!fl->is_64)
            emit(x, is_signed ? enc_sbfx(reg_v0, reg_v0, 0, 32) : enc_ubfx(reg_v0, reg_v0, 0, 32));

        emit(x, enc_sub_reg(T2, T0, T1)); /* diff = A - B */
        if (!fl->is_64) {
            if (fl->cmp == FL_NE) {
                emit(x, enc_ubfx(T2, T2, 0, 32));
                bails[nbails++] = here(x);
                emit(x, enc_cbz_x(T2));
            } else {
                bails[nbails++] = here(x);
                emit(x, enc_tbnz(T2, 63));
                bails[nbails++] = here(x);
                emit(x, enc_cbz_x(T2));
            }
        } else {
            bails[nbails++] = here(x);
            emit(x, enc_cbz_x(T2));
            if (fl->cmp == FL_ULT || fl->cmp == FL_ULE) {
                /* unsigned borrow = d ^ ((a ^ b) & (b ^ d)) */
                emit(x, enc_eor_lsr(T0, T0, T1, 0));
                emit(x, enc_eor_lsr(T1, T1, T2, 0));
                emit(x, enc_and_reg(T0, T0, T1));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                bails[nbails++] = here(x);
                emit(x, enc_tbnz(T0, 63));
            } else if (fl->cmp == FL_SLT || fl->cmp == FL_SLE) {
                /* signed_lt = d ^ ((a ^ b) & (a ^ d)) */
                emit(x, enc_eor_lsr(T1, T0, T1, 0));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                emit(x, enc_and_reg(T0, T0, T1));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                bails[nbails++] = here(x);
                emit(x, enc_tbnz(T0, 63));
            }
        }
        if (fl->cmp == FL_NE || fl->cmp == FL_SLT || fl->cmp == FL_ULT)
            emit(x, enc_sub_imm(T2, T2, 1, 0));
        if (fl->step > 1) {
            if ((fl->step & (fl->step - 1)) == 0) {
                unsigned sh = (unsigned)__builtin_ctz(fl->step);
                emit(x, enc_ubfx(T2, T2, sh, 64 - sh));
            } else {
                emit_mov_const(x, T0, fl->step);
                emit(x, enc_udiv(T2, T2, T0));
            }
        }
        /* Now T2 = q = safe_iters_bound - 1. Bail if q == 0 (< 2 iters), clamp q, then add 1. */
        bails[nbails++] = here(x);
        emit(x, enc_cbz_x(T2));
        emit(x, enc_ubfx(T0, T2, fl->kick_log2, 64 - fl->kick_log2));
        uint32_t *no_clamp = here(x);
        emit(x, enc_cbz_x(T0));
        emit(x, enc_movz(T2, (1u << fl->kick_log2) - 1, 0));
        patch_local(x, no_clamp, here(x));
        emit(x, enc_add_imm(T2, T2, 1, 0));
    }
    emit(x, enc_str_x(T2, CTX, iters_off));

    /* 3. For each access stream: verify TLB/STLB hits, shared addends for multi-stream bases, and
     *    span bounds (shrinking safe_iters for strided streams; dual-endpoint 4 KiB TLB check for
     *    zero-stride streams including SP frames and bounded table-lookup M_REG streams). */
    for (unsigned i = 0; i < fl->nstreams; i++) {
        const struct fast_stream *s = &fl->streams[i];
        unsigned base = fl->ptrs[s->ptr_idx].reg;
        if (s->idx_reg2 < 31) {
            unsigned ir1 = s->idx_reg, ir2 = s->idx_reg2;
            if (is_stolen(ir1)) {
                emit_load_guest(x, T1, ir1);
                ir1 = T1;
            }
            if (is_stolen(ir2)) {
                emit_load_guest(x, T2, ir2);
                ir2 = T2;
            }
            emit(x, enc_add_lsl(T1, ir1, ir2, 0));
            if (base == 31) {
                base = SP_HOST;
            } else if (is_stolen(base)) {
                emit_load_guest(x, T0, base);
                base = T0;
            }
            emit(x, enc_add_lsl(T0, base, T1, s->idx_shift));
            emit_add_const(x, T0, T0, s->lo);
        } else {
            if (base == 31) {
                base = SP_HOST;
            } else if (is_stolen(base)) {
                emit_load_guest(x, T0, base);
                base = T0;
            }
            if (s->idx_reg < 31) {
                unsigned idx = s->idx_reg;
                if (is_stolen(idx)) {
                    emit_load_guest(x, T1, idx);
                    idx = T1;
                }
                emit(x, enc_add_ext(T0, base, idx, s->idx_opt, s->idx_shift));
                base = T0;
            }
            emit_add_const(x, T0, base, s->lo); /* T0 = first = G_eff + lo */
        }
        uint32_t span = (uint32_t)(s->hi - s->lo);

        if (s->stride_reg == 31 && s->stride == 0) {
            /* Zero-stride stream (span <= 4 KiB): check first byte and last byte in 4 KiB TLB and
             * require identical addends so even cross-page stack frames / LUTs run natively. */
            emit(x, enc_ubfx(T1, T0, APPINE_PAGE_SHIFT, TLB_BITS));
            emit(x, enc_add_lsl(T1, CTX, T1, 5));
            emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (s->write ? 16 : 0)));
            emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT));
            bails[nbails++] = here(x);
            emit(x, enc_cbnz_x(T1));
            if (s->first_for_ptr) {
                emit(x, enc_str_x(T2, CTX, grp_off + 8 * (size_t)s->ptr_idx));
            } else {
                emit(x, enc_ldr_x(T1, CTX, grp_off + 8 * (size_t)s->ptr_idx));
                emit(x, enc_eor_lsr(T1, T1, T2, 0));
                bails[nbails++] = here(x);
                emit(x, enc_cbnz_x(T1));
            }
            if (span > 1) {
                emit_add_const(x, T0, T0, (int64_t)(span - 1));
                emit(x, enc_ubfx(T1, T0, APPINE_PAGE_SHIFT, TLB_BITS));
                emit(x, enc_add_lsl(T1, CTX, T1, 5));
                emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (s->write ? 16 : 0)));
                emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT));
                bails[nbails++] = here(x);
                emit(x, enc_cbnz_x(T1));
                emit(x, enc_ldr_x(T0, CTX, grp_off + 8 * (size_t)s->ptr_idx));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                bails[nbails++] = here(x);
                emit(x, enc_cbnz_x(T0));
            }
            continue;
        }

        bool neg_stride = s->stride_reg == 31 && s->stride < 0;

        /* Fast path: 2 MiB superpage TLB (cpu->stlb) */
        emit(x, enc_add_imm(T1, CTX, (uint32_t)(STLB_OFF >> 12), 1));
        emit(x, enc_ubfx(T2, T0, MMU_SUPER_SHIFT, STLB_BITS));
        emit(x, enc_add_lsl(T1, T1, T2, 5));
        emit(x, enc_ldp_x(T1, T2, T1, s->write ? 16 : 0));
        emit(x, enc_eon_lsr(T1, T1, T0, MMU_SUPER_SHIFT));
        uint32_t *try_4k = here(x);
        emit(x, enc_cbnz_x(T1));
        emit(x, enc_and_low(T1, T0, MMU_SUPER_SHIFT));
        emit_mov_const(x, T0, MMU_SUPER_SIZE - span);
        if (neg_stride)
            emit(x, enc_sub_reg(T0, T0, T1)); /* T0 = (2 MiB - span) - super_off; T1 = avail */
        else
            emit(x, enc_sub_reg(T1, T0, T1)); /* T1 = avail = (2 MiB - span) - super_off */
        uint32_t *got_super = here(x);
        emit(x, enc_b(0));

        /* Fallback: 4 KiB data TLB (cpu->tlb) */
        patch_local(x, try_4k, here(x));
        emit(x, enc_ubfx(T1, T0, APPINE_PAGE_SHIFT, TLB_BITS));
        emit(x, enc_add_lsl(T1, CTX, T1, 5));
        emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (s->write ? 16 : 0)));
        emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT));
        bails[nbails++] = here(x);
        emit(x, enc_cbnz_x(T1));
        emit(x, enc_and_low(T1, T0, APPINE_PAGE_SHIFT));
        emit(x, enc_movz(T0, (uint32_t)(APPINE_PAGE_SIZE - span), 0));
        if (neg_stride)
            emit(x, enc_sub_reg(T0, T0, T1)); /* T0 = (4 KiB - span) - page_off; T1 = avail */
        else
            emit(x, enc_sub_reg(T1, T0, T1)); /* T1 = avail = (4 KiB - span) - page_off */

        patch_local(x, got_super, here(x));
        bails[nbails++] = here(x);
        emit(x, enc_tbnz(neg_stride ? T0 : T1, 63));
        if (s->first_for_ptr) {
            emit(x, enc_str_x(T2, CTX, grp_off + 8 * (size_t)s->ptr_idx));
        } else {
            emit(x, enc_ldr_x(T0, CTX, grp_off + 8 * (size_t)s->ptr_idx));
            emit(x, enc_eor_lsr(T0, T0, T2, 0));
            bails[nbails++] = here(x);
            emit(x, enc_cbnz_x(T0));
        }

        if (s->stride_reg < 31) {
            /* Runtime register stride: reject negative or >= 2 MiB strides, then check if all
             * safe_iters fit via MUL (avoiding UDIV on the common 2 MiB superpage fast path).
             * Only on a boundary crossing fall through to UDIV to shrink safe_iters. */
            unsigned sr = s->stride_reg;
            if (is_stolen(sr)) {
                emit_load_guest(x, T0, sr);
                sr = T0;
            }
            emit(x, enc_ubfx(T2, sr, MMU_SUPER_SHIFT, 64 - MMU_SUPER_SHIFT));
            bails[nbails++] = here(x);
            emit(x, enc_cbnz_x(T2));
            emit(x, enc_ldr_x(T2, CTX, iters_off));
            emit(x, enc_sub_imm(T2, T2, 1, 0));
            emit(x, enc_mul(T2, T2, sr));
            emit(x, enc_sub_reg(T2, T1, T2)); /* avail - (safe_iters - 1) * sr */
            uint32_t *fits_all = here(x);
            emit(x, enc_tbz(T2, 63));
            emit(x, enc_udiv(T1, T1, sr));
            bails[nbails++] = here(x);
            emit(x, enc_cbz_x(T1));
            emit(x, enc_add_imm(T1, T1, 1, 0));
            emit(x, enc_str_x(T1, CTX, iters_off));
            patch_local(x, fits_all, here(x));
        } else {
            uint32_t abs_stride = (uint32_t)(neg_stride ? -s->stride : s->stride);
            if ((abs_stride & (abs_stride - 1)) == 0) {
                unsigned sh = (unsigned)__builtin_ctz(abs_stride);
                emit(x, enc_ubfx(T1, T1, sh, 64 - sh));
            } else {
                /* Exact floor(avail / abs_stride) for 0 <= avail < 2 MiB, 1 <= abs_stride <= 4095 */
                uint64_t mul = ((UINT64_C(1) << 34) + abs_stride - 1) / abs_stride;
                emit_mov_const(x, T2, mul);
                emit(x, enc_mul(T1, T1, T2));
                emit(x, enc_ubfx(T1, T1, 34, 30));
            }
            /* page_iters = avail / abs_stride + 1; if avail / abs_stride == 0 then page_iters == 1 < 2 */
            bails[nbails++] = here(x);
            emit(x, enc_cbz_x(T1));
            emit(x, enc_add_imm(T1, T1, 1, 0));
            emit(x, enc_ldr_x(T2, CTX, iters_off));
            emit(x, enc_sub_reg(T2, T1, T2));
            uint32_t *keep = here(x);
            emit(x, enc_tbz(T2, 63));
            emit(x, enc_str_x(T1, CTX, iters_off));
            patch_local(x, keep, here(x));
        }
    }

    /* 3b. Guard 32-bit M_REG induction indices (UXTW / SXTW) against 32-bit wrap across safe_iters */
    for (unsigned i = 0; i < fl->nstreams; i++) {
        const struct fast_stream *s = &fl->streams[i];
        if (s->idx_w32_step == 0)
            continue;
        unsigned ir = s->idx_reg;
        unsigned src = ir;
        if (is_stolen(ir)) {
            emit_load_guest(x, T0, ir);
            src = T0;
        }
        bool neg_step = s->idx_w32_step < 0;
        uint32_t abs_step = (uint32_t)(neg_step ? -s->idx_w32_step : s->idx_w32_step);
        if (!neg_step) {
            if (s->idx_opt == 2) { /* UXTW: room = 0xffffffff - (uint32_t)w */
                emit(x, enc_ubfx(T0, src, 0, 32));
                emit_mov_const(x, T1, UINT64_C(0xffffffff));
                emit(x, enc_sub_reg(T0, T1, T0));
            } else { /* SXTW: room = 0x7fffffff - (int32_t)w */
                emit(x, enc_sbfx(T0, src, 0, 32));
                emit_mov_const(x, T1, UINT64_C(0x7fffffff));
                emit(x, enc_sub_reg(T0, T1, T0));
            }
            if (s->idx_w32_pre_off > 0) {
                emit_add_const(x, T0, T0, -(int64_t)s->idx_w32_pre_off);
                bails[nbails++] = here(x);
                emit(x, enc_tbnz(T0, 63));
            }
        } else {
            if (s->idx_opt == 2) { /* UXTW: room = (uint32_t)w */
                emit(x, enc_ubfx(T0, src, 0, 32));
            } else { /* SXTW: room = (int32_t)w + 0x80000000 */
                emit(x, enc_sbfx(T0, src, 0, 32));
                emit_mov_const(x, T1, UINT64_C(0x80000000));
                emit(x, enc_add_lsl(T0, T0, T1, 0));
            }
            if (s->idx_w32_pre_off < 0) {
                emit_add_const(x, T0, T0, (int64_t)s->idx_w32_pre_off);
                bails[nbails++] = here(x);
                emit(x, enc_tbnz(T0, 63));
            }
        }
        /* Fast path: check (safe_iters - 1) * abs_step <= room; only divide if approaching wrap */
        emit(x, enc_ldr_x(T2, CTX, iters_off));
        emit(x, enc_sub_imm(T1, T2, 1, 0));
        if (abs_step > 1) {
            if ((abs_step & (abs_step - 1)) == 0) {
                unsigned sh = (unsigned)__builtin_ctz(abs_step);
                emit(x, enc_add_lsl(T1, 31, T1, sh));
            } else {
                emit_mov_const(x, T2, abs_step);
                emit(x, enc_mul(T1, T1, T2));
            }
        }
        emit(x, enc_sub_reg(T1, T0, T1)); /* room - (safe_iters - 1) * abs_step */
        uint32_t *no_wrap = here(x);
        emit(x, enc_tbz(T1, 63));
        if (abs_step > 1) {
            if ((abs_step & (abs_step - 1)) == 0) {
                unsigned sh = (unsigned)__builtin_ctz(abs_step);
                emit(x, enc_ubfx(T0, T0, sh, 64 - sh));
            } else {
                emit_mov_const(x, T2, abs_step);
                emit(x, enc_udiv(T0, T0, T2));
            }
        }
        bails[nbails++] = here(x);
        emit(x, enc_cbz_x(T0));
        emit(x, enc_add_imm(T0, T0, 1, 0));
        emit(x, enc_str_x(T0, CTX, iters_off));
        patch_local(x, no_wrap, here(x));
    }

    /* 3c. Probe 4 KiB read TLB for each single-page M_LITERAL and save its host address */
    for (unsigned l = 0; l < fl->nlits; l++) {
        emit_mov_const(x, T0, fl->lit_addr[l]);
        emit(x, enc_ubfx(T1, T0, APPINE_PAGE_SHIFT, TLB_BITS));
        emit(x, enc_add_lsl(T1, CTX, T1, 5));
        emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF));
        emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT));
        bails[nbails++] = here(x);
        emit(x, enc_cbnz_x(T1));
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        emit(x, enc_str_x(T2, CTX, grp_off + 8 * (size_t)(fl->nptrs + l)));
    }
    if (nbails > sizeof(bails) / sizeof(bails[0]))
        appine_guard_fail();

    /* 4. Commit: convert induction pointers to host pointers, save borrowed non-stolen host regs,
     *    load pinned stolen regs and literal host bases, and load safe_iters into T2 */
    for (unsigned i = 0; i < fl->nptrs; i++) {
        if (fl->ptrs[i].reg < 31 && fast_is_pinned(fl, fl->ptrs[i].reg))
            continue;
        emit(x, enc_ldr_x(T0, CTX, grp_off + 8 * (size_t)i));
        emit_adjust_reg(x, fl->ptrs[i].reg, false);
    }
    for (unsigned b = 0; b < fl->nborrow; b++)
        emit_store_guest(x, fl->borrow_host[b], fl->borrow_host[b]);
    for (unsigned k = 0; k < fl->npin; k++) {
        unsigned gr = fl->pin_reg[k], hr = fl->pin_host[k];
        emit_load_guest(x, hr, gr);
        int pi = fast_ptr_index(fl, gr);
        if (pi >= 0) {
            emit(x, enc_ldr_x(T2, CTX, grp_off + 8 * (size_t)pi));
            emit(x, enc_add_lsl(hr, hr, T2, 0));
        }
    }
    for (unsigned l = 0; l < fl->nlits; l++)
        emit(x, enc_ldr_x(fl->lit_host[l], CTX, grp_off + 8 * (size_t)(fl->nptrs + l)));
    emit(x, enc_ldr_x(T2, CTX, iters_off));

    /* Deduplicate side-exit target PCs before emitting the body */
    uint64_t utarget[FAST_LOOP_MAX_SIDE_EXITS];
    unsigned ureason[FAST_LOOP_MAX_SIDE_EXITS];
    unsigned se_uidx[FAST_LOOP_MAX_SIDE_EXITS];
    uint32_t *se_sites[FAST_LOOP_MAX_SIDE_EXITS];
    unsigned nu = 0, nse_sites = 0;
    for (unsigned k = 0; k < fl->nside_exits; k++) {
        uint64_t tgt = fl->side_exits[k].target;
        unsigned rsn = tgt > fl->side_exits[k].va ? R_CHAIN_FWD : R_CHAIN;
        unsigned j;
        for (j = 0; j < nu && utarget[j] != tgt; j++)
            ;
        if (j == nu) {
            utarget[nu] = tgt;
            ureason[nu] = rsn;
            nu++;
        } else if (rsn == R_CHAIN) {
            ureason[j] = R_CHAIN;
        }
        se_uidx[k] = j;
    }

    /* 5. Native inner loop body (zero TLB checks, zero kick checks) */
    uint32_t *loop_head = here(x);
    unsigned next_se = 0;
    for (unsigned i = 0; i < fl->back_idx; i++) {
        uint32_t insn = x->words[i];
        if (insn == 0xd503201f)
            continue;
        if (fl->header_tested && i == fl->hdr_br_idx)
            continue; /* header exit proved not-taken for all safe_iters iterations */
        if (fl->omit_ctrl && i == fl->ctrl_idx)
            continue; /* omitted CMP (re-emitted once in normal epilogue if bottom-tested) */
        if (next_se < fl->nside_exits && fl->side_exits[next_se].idx == i) {
            if ((insn & 0xff000010) == 0x54000000) { /* B.cond */
                se_sites[nse_sites++] = here(x);
                emit(x, insn & ~(0x7ffffu << 5));
            } else { /* CBZ / CBNZ / TBZ / TBNZ */
                bool is_tb = (insn & 0x7e000000) == 0x36000000;
                unsigned rt = insn & 31;
                unsigned hrt = rt;
                if (rt < 31) {
                    if (fl->npin) {
                        hrt = fast_pin_host(fl, rt);
                    } else if (is_stolen(rt)) {
                        emit_load_guest(x, T0, rt);
                        hrt = T0;
                    }
                }
                se_sites[nse_sites++] = here(x);
                uint32_t mask = is_tb ? (0x3fffu << 5) : (0x7ffffu << 5);
                emit(x, (insn & ~(mask | 31u)) | hrt);
            }
            next_se++;
            continue;
        }
        unsigned top = FIELD(insn, 28, 25);
        if (top == 0x4 || top == 0x6 || top == 0xc || top == 0xe) {
            struct memop m;
            uint64_t va = x->pc + 4ull * i;
            if (BIT(insn, 26))
                dbt_decode_simd_memop(insn, va, &m);
            else
                dbt_decode_memop(insn, va, &m);
            if (!m.prefetch) {
                if (m.mode == M_LITERAL) {
                    emit_fast_literal(x, fl, insn, &m);
                } else if (fl->npin) {
                    unsigned hrn = m.rn == 31 ? SP_HOST : fast_pin_host(fl, m.rn);
                    uint32_t out = (insn & ~(31u << 5)) | (hrn << 5);
                    if (m.mode == M_REG && m.rm < 31)
                        out = (out & ~(31u << 16)) | (fast_pin_host(fl, m.rm) << 16);
                    if (!m.simd) {
                        if (m.rt < 31)
                            out = (out & ~31u) | fast_pin_host(fl, m.rt);
                        if (m.pair && m.rt2 < 31)
                            out = (out & ~(31u << 10)) | (fast_pin_host(fl, m.rt2) << 10);
                    }
                    emit(x, out);
                } else {
                    emit_fast_memop(x, insn, &m);
                }
            }
        } else if (FIELD(insn, 28, 24) == 0x10) { /* ADR / ADRP */
            unsigned rd = insn & 31;
            if (rd < 31) {
                uint64_t va = x->pc + 4ull * i;
                uint64_t imm = ((uint64_t)FIELD(insn, 23, 5) << 2) | FIELD(insn, 30, 29);
                int64_t off = (int64_t)(imm << 43) >> 43;
                uint64_t v = BIT(insn, 31) ? (va & ~UINT64_C(0xfff)) + ((uint64_t)off << 12)
                                           : va + (uint64_t)off;
                if (fl->npin) {
                    emit_mov_const(x, fast_pin_host(fl, rd), v);
                } else if (is_stolen(rd)) {
                    emit_mov_const(x, T0, v);
                    emit_store_guest(x, T0, rd);
                } else {
                    emit_mov_const(x, rd, v);
                }
            }
        } else {
            struct operands o;
            if ((top & 7) == 7) {
                struct native_desc nd;
                appine_simd_dp_decode(insn, &nd);
                simd_dp_operands(&nd, &o);
            } else {
                dbt_dp_operands(insn, &o);
            }
            if (fl->npin) {
                uint32_t out = insn;
                for (unsigned k = 0; k < o.n; k++) {
                    unsigned r = (insn >> o.pos[k]) & 31;
                    out = (out & ~(31u << o.pos[k])) | (fast_pin_host(fl, r) << o.pos[k]);
                }
                emit(x, out);
            } else {
                emit_fast_operands(x, insn, &o);
            }
        }
    }
    emit(x, enc_sub_imm(T2, T2, 1, 0));
    uint32_t *back = here(x);
    emit(x, enc_cbnz_x(T2));
    patch_local(x, back, loop_head);

    /* 6. Normal epilogue: restore guest pointers/regs and re-emit omitted bottom-tested CMP once */
    emit_fast_epilogue(x, fl, grp_off);
    if ((fl->kind == FL_CMP_INC || fl->kind == FL_CMP_DEC) && fl->omit_ctrl && !fl->header_tested) {
        uint32_t cmp_insn = x->words[fl->ctrl_idx];
        struct operands o;
        dbt_dp_operands(cmp_insn, &o);
        emit_fast_operands(x, cmp_insn, &o);
    }

    /* 6b. Shared side-exit epilogue (NZCV-free) jumping to each side exit's R_CHAIN stub */
    if (fl->nside_exits > 0) {
        uint32_t *to_slow = here(x);
        emit(x, enc_b(0));
        if (nu == 1) {
            uint32_t *se_epi = here(x);
            for (unsigned k = 0; k < nse_sites; k++)
                patch_local(x, se_sites[k], se_epi);
            emit_fast_epilogue(x, fl, grp_off);
            emit_exit_stub(x, ureason[0], utarget[0]);
        } else {
            uint32_t *stub_addr[FAST_LOOP_MAX_SIDE_EXITS];
            uint32_t *tramp_addr[FAST_LOOP_MAX_SIDE_EXITS];
            uint32_t *tramp_b[FAST_LOOP_MAX_SIDE_EXITS];
            for (unsigned j = 0; j < nu; j++) {
                stub_addr[j] = here(x);
                emit_exit_stub(x, ureason[j], utarget[j]);
            }
            for (unsigned j = 0; j < nu; j++) {
                tramp_addr[j] = here(x);
                emit(x, enc_adr(T2, (intptr_t)stub_addr[j] - (intptr_t)here(x)));
                tramp_b[j] = here(x);
                emit(x, enc_b(0));
            }
            for (unsigned k = 0; k < nse_sites; k++)
                patch_local(x, se_sites[k], tramp_addr[se_uidx[k]]);
            uint32_t *se_epi = here(x);
            for (unsigned j = 0; j < nu; j++)
                patch_local(x, tramp_b[j], se_epi);
            emit(x, enc_str_x(T2, CTX, iters_off));
            emit_fast_epilogue(x, fl, grp_off);
            emit(x, enc_ldr_x(T2, CTX, iters_off));
            emit(x, enc_br(T2));
        }
        patch_local(x, to_slow, here(x));
    }

    uint32_t *slow = here(x);
    for (unsigned i = 0; i < nbails; i++)
        patch_local(x, bails[i], slow);
    x->st->fast_loops++;
}

uint32_t dbt_try_fast_loop(struct xlate *x)
{
    struct fast_loop fl;
    if (!analyze_fast_loop(x, &fl))
        return 0;
    emit_fast_loop(x, &fl);
    return fl.back_idx >= x->limit ? fl.back_idx + 1 : 0;
}

#endif /* DBT_SUPPORTED */
