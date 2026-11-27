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
 * Recognize canonical single-block (or 1-boundary cross-page) self-loops (prefix 0..back_idx with
 * back-edge targeting x->pc) and emit a guarded native fast loop ahead of the normal translated
 * body:
 *   1. Verify cpu->nwatch == 0 (never bypass hardware watchpoints) and 16-byte SP alignment if SP
 *      is used as a base.
 *   2. Compute a conservative lower bound on iterations guaranteed to take the back-edge before
 *      exiting, clamped to `1 << kick_log2` (keeping <= 12,288 guest instructions per chunk) and
 *      to any 32-bit M_REG index wrap boundary.
 *   3. For each access stream (up to 7 root pointers + single-page M_LITERALs in dbt_groups[0..6]
 *      and safe_iters in dbt_groups[7]), probe 2 MiB STLB then 4 KiB TLB (or dual-endpoint 4 KiB
 *      TLB for zero-stride/LUT streams), shrinking safe_iters to fit inside the hit page.
 *   4. If safe_iters >= 2, add each root's TLB addend (converting guest pointers to host pointers),
 *      pin touched stolen GPRs into T0/T1 and borrowed unused non-stolen registers when possible,
 *      run the body natively for safe_iters iterations (counted in T2, with shared NZCV-free
 *      epilogue for up to 4 early side exits), restore registers/pointers, and fall through into
 *      the standard checked loop for page crossings, tail iterations, and kicks.
 *
 * All inserted prologue/counter/epilogue code is strictly NZCV-free (D6) and leaves X18 untouched.
 * Because safe_iters leaves at least one iteration (or a verified early exit), the checked loop
 * produces exact exit NZCV and precise faults on any page boundary or mismatch.
 */
#define FAST_LOOP_MAX_PTRS 7
#define FAST_LOOP_MAX_STREAMS 16
#define FAST_LOOP_MAX_SIDE_EXITS 4
#define FAST_LOOP_MAX_LITS 4
#define FAST_LOOP_MAX_PINS 6
#define FAST_LOOP_MAX_BORROWS 8
#define FAST_LOOP_MAX_BAILS 192
#define FAST_LOOP_MAX_WORDS 1600
_Static_assert(FAST_LOOP_MAX_PTRS < GROUP_SLOTS, "dbt_groups[7] holds safe_iters during prologue");

/* ARM64 condition codes used by fast-loop control matching and prologue trip calculation */
enum {
    COND_NE = 1,
    COND_HS = 2,
    COND_LO = 3,
    COND_MI = 4,
    COND_PL = 5,
    COND_HI = 8,
    COND_LS = 9,
    COND_GE = 10,
    COND_LT = 11,
    COND_GT = 12,
    COND_LE = 13,
};

/* ARM64 extend-option codes accepted for M_REG / shifted-ADD index operands */
enum {
    EXT_UXTW = 2,
    EXT_LSL = 3,
    EXT_SXTW = 6,
    EXT_SXTX = 7,
};

#define INSN_NOP 0xd503201fu

static inline bool is_ext32(unsigned opt)
{
    return opt == EXT_UXTW || opt == EXT_SXTW;
}

static inline bool is_ext64(unsigned opt)
{
    return opt == EXT_LSL || opt == EXT_SXTX;
}

static inline bool is_bcond(uint32_t insn)
{
    return (insn & 0xff000010u) == 0x54000000u;
}

static inline bool is_cbz_cbnz(uint32_t insn)
{
    return (insn & 0x7e000000u) == 0x34000000u;
}

static inline bool is_tbz_tbnz(uint32_t insn)
{
    return (insn & 0x7e000000u) == 0x36000000u;
}

static inline bool is_memop_top(unsigned top)
{
    return top == 0x4 || top == 0x6 || top == 0xc || top == 0xe;
}

static inline bool is_adr_adrp(uint32_t insn)
{
    return FIELD(insn, 28, 24) == 0x10;
}

static inline bool is_cmp_reg(uint32_t insn)
{
    return (insn & 0x7fe0fc1fu) == 0x6b00001fu;
}

static inline bool is_cmp_imm(uint32_t insn)
{
    return (insn & 0x7f80001fu) == 0x7100001fu;
}

static inline uint32_t addsub_imm(uint32_t insn)
{
    return FIELD(insn, 21, 10) << (BIT(insn, 22) ? 12 : 0);
}

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
    uint8_t pin_reg[FAST_LOOP_MAX_PINS];
    uint8_t pin_host[FAST_LOOP_MAX_PINS];
    bool pin_written[FAST_LOOP_MAX_PINS];
    uint8_t borrow_host[FAST_LOOP_MAX_BORROWS];
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
    uint8_t idx_opt;  /* EXT_UXTW, EXT_LSL, EXT_SXTW, EXT_SXTX */
    uint8_t idx_shift;/* 0..4 */
    uint8_t lut_bits; /* 0 = strided/invariant idx_reg; 1..12 = bounded LUT */
    int32_t w32_off;  /* pre-access 32-bit index offset when UXTW/SXTW w32_valid */
    int64_t off;
};

/* Block-local derived 64-bit index expression: (reg0 + reg1 + off) << shift (reg1 == -1 if single). */
struct fl_sym_idx {
    int8_t reg0, reg1;
    int64_t off;
    uint8_t shift;
};

/* Per-register symbolic analysis state tracked across a candidate fast loop body */
struct fl_state {
    int8_t root[31];
    int8_t sreg[31];
    int64_t off[31];
    struct fl_pidx pidx[31];
    struct fl_sym_idx idx[31];
    uint8_t u_bits[31];
    uint32_t w32_valid;
    int32_t w32_step[31];
    uint32_t written;
    uint32_t rbw;
    uint32_t nonptr;
    unsigned write_count[31];
    unsigned last_write_idx[31];
};

/* Snapshot at a candidate early side exit (or header exit of a bottom-B while loop) */
struct fl_cand_se {
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
};

/* Record that guest register r (0..30) was read by a non-pointer-preserving operation */
static inline void fl_mark_read(struct fl_state *st, unsigned r)
{
    if (r >= 31)
        return;
    if (!(st->written & (1u << r)))
        st->rbw |= 1u << r;
    if (st->root[r] >= 0)
        st->nonptr |= 1u << (unsigned)st->root[r];
}

static inline void fl_note_write(struct fl_state *st, unsigned r, unsigned insn_idx)
{
    st->written |= 1u << r;
    st->write_count[r]++;
    st->last_write_idx[r] = insn_idx;
}

static inline void fl_clobber_reg(struct fl_state *st, unsigned r, unsigned insn_idx, uint8_t ub)
{
    if (r >= 31)
        return;
    st->root[r] = -1;
    st->sreg[r] = -1;
    st->pidx[r].root = -1;
    st->idx[r].reg0 = -1;
    st->u_bits[r] = ub;
    st->w32_valid &= ~(1u << r);
    fl_note_write(st, r, insn_idx);
}

static inline bool fl_has_const_step(const struct fl_state *st, unsigned r)
{
    return st->root[r] == (int8_t)r && st->sreg[r] < 0 && st->pidx[r].root < 0 &&
           st->off[r] != 0 && st->off[r] >= -4096 && st->off[r] <= 4096;
}

static inline bool fl_step_w32(const struct fl_state *st, unsigned r, int64_t *step_out)
{
    if (((st->w32_valid >> r) & 1) && st->w32_step[r] != 0) {
        *step_out = st->w32_step[r];
        return true;
    }
    if (fl_has_const_step(st, r)) {
        *step_out = st->off[r];
        return true;
    }
    return false;
}

static inline void fl_canonical_idx_pair(unsigned *r0, unsigned *r1)
{
    if (*r1 < 31 && *r0 > *r1) {
        unsigned tmp = *r0;
        *r0 = *r1;
        *r1 = tmp;
    }
}

static inline bool fl_lut_fits(uint8_t ub, unsigned shift)
{
    return ub > 0 && ub <= 12 && ((1u << ub) << shift) <= APPINE_PAGE_SIZE;
}

static inline int8_t fl_alias_root(const int8_t root[31], unsigned a, uint32_t used_as_ptr)
{
    return (root[a] >= 0 && (used_as_ptr & (1u << (unsigned)root[a])) && a != (unsigned)root[a])
               ? root[a]
               : -1;
}

static inline bool fl_add_memop_stolen(const struct xlate *x, unsigned seen[2], unsigned *ns,
                                       unsigned r)
{
    if (r >= 31 || !is_stolen(r))
        return true;
    for (unsigned k = 0; k < *ns; k++) {
        if (seen[k] == r)
            return true;
    }
    if (*ns == 2)
        return false;
    seen[(*ns)++] = r;
    return true;
}

static bool fl_decode_cmp_cond(unsigned cont_cond, bool eff_cnt_first, enum fast_cmp_kind *ck)
{
    if (cont_cond == COND_NE) {
        *ck = FL_NE;
        return true;
    }
    if (!eff_cnt_first) {
        switch (cont_cond) {
        case COND_GT:
            *ck = FL_SLT;
            return true;
        case COND_GE:
            *ck = FL_SLE;
            return true;
        case COND_HI:
            *ck = FL_ULT;
            return true;
        case COND_HS:
            *ck = FL_ULE;
            return true;
        default:
            return false;
        }
    }
    switch (cont_cond) {
    case COND_LT:
        *ck = FL_SLT;
        return true;
    case COND_LE:
        *ck = FL_SLE;
        return true;
    case COND_LO:
        *ck = FL_ULT;
        return true;
    case COND_LS:
        *ck = FL_ULE;
        return true;
    default:
        return false;
    }
}

static bool fl_match_cmp(uint32_t fi_insn, unsigned flag_idx, unsigned cont_cond,
                         const struct fl_cand_se *hdr, const struct fl_state *st,
                         uint32_t used_as_ptr, struct fast_loop *fl)
{
    if (!is_cmp_reg(fi_insn) && !is_cmp_imm(fi_insn))
        return false;
    bool is64 = BIT(fi_insn, 31);
    bool is_imm = is_cmp_imm(fi_insn);
    unsigned rn = FIELD(fi_insn, 9, 5);
    unsigned rm = is_imm ? 31 : FIELD(fi_insn, 20, 16);

    for (unsigned side = 0; side < (is_imm ? 1u : 2u); side++) {
        bool cnt_first = is_imm || side == 1;
        unsigned d = cnt_first ? rn : rm;
        unsigned b = cnt_first ? rm : rn;
        if (d >= 31)
            continue;
        if (!is_imm &&
            (b >= 31 || b == d || (st->written & (1u << b)) || (used_as_ptr & (1u << b))))
            continue;

        unsigned c = d;
        int64_t signed_step = 0;
        int64_t cmp_off = 0;
        if (!hdr) {
            if (!(st->written & (1u << d)) || st->last_write_idx[d] >= flag_idx)
                continue;
            if (is64) {
                if (st->root[d] < 0 || st->sreg[d] >= 0 || st->pidx[d].root >= 0)
                    continue;
                c = (unsigned)st->root[d];
                if ((!is_imm && c == b) || !fl_has_const_step(st, c))
                    continue;
                if (d != c && ((used_as_ptr & (1u << c)) || (used_as_ptr & (1u << d)) ||
                               st->off[d] < -4095 || st->off[d] > 4095))
                    continue;
                signed_step = st->off[c];
                cmp_off = st->off[d];
            } else {
                if ((used_as_ptr & (1u << d)) || !fl_step_w32(st, d, &signed_step))
                    continue;
                cmp_off = signed_step;
            }
        } else {
            if (is64) {
                if (hdr->sym_root[d] < 0 || hdr->sym_sreg[d] >= 0 || hdr->pidx_root[d] >= 0)
                    continue;
                c = (unsigned)hdr->sym_root[d];
                if ((hdr->written_mask & (1u << c)) || (!is_imm && c == b) ||
                    !(st->written & (1u << c)) || st->last_write_idx[c] <= hdr->idx ||
                    !fl_has_const_step(st, c))
                    continue;
                if (d != c && ((used_as_ptr & (1u << c)) || (used_as_ptr & (1u << d)) ||
                               hdr->sym_off[d] < -4095 || hdr->sym_off[d] > 4095))
                    continue;
                signed_step = st->off[c];
                cmp_off = hdr->sym_off[d];
            } else {
                if ((hdr->written_mask & (1u << d)) || (used_as_ptr & (1u << d)) ||
                    !(st->written & (1u << d)) || st->last_write_idx[d] <= hdr->idx ||
                    !fl_step_w32(st, d, &signed_step))
                    continue;
                cmp_off = 0;
            }
        }

        bool is_dec = signed_step < 0;
        enum fast_cmp_kind ck;
        if (!fl_decode_cmp_cond(cont_cond, is_dec ? !cnt_first : cnt_first, &ck))
            continue;
        fl->kind = is_dec ? FL_CMP_DEC : FL_CMP_INC;
        fl->ctrl_idx = flag_idx;
        fl->cnt_reg = c;
        fl->bound_reg = b;
        fl->bound_is_imm = is_imm;
        fl->bound_imm = is_imm ? addsub_imm(fi_insn) : 0;
        fl->is_64 = is64;
        fl->step = (uint32_t)(is_dec ? -signed_step : signed_step);
        fl->cmp_off = cmp_off;
        fl->cmp = ck;
        return true;
    }
    return false;
}

/* Match a single-write immediate counter update (SUB/SUBS or ADD/ADDS Xc/Wc, Xc/Wc, #1..4096)
 * for CBZ/CBNZ/TBZ/TBNZ loop closers (header-tested when hdr != NULL, else bottom-tested). */
static bool fl_match_imm_step(const struct xlate *x, const struct fl_state *st,
                              uint32_t used_as_ptr, unsigned c, bool is64, bool allow_sub,
                              bool allow_add, unsigned sub_cond, unsigned add_cond,
                              const struct fl_cand_se *hdr, struct fast_loop *fl)
{
    if (c >= 31 || st->write_count[c] != 1 || (used_as_ptr & (1u << c)))
        return false;
    unsigned ci = st->last_write_idx[c];
    if (hdr && ci <= hdr->idx)
        return false;
    uint32_t ci_insn = x->words[ci];
    if ((ci_insn & 31) != c || FIELD(ci_insn, 9, 5) != c)
        return false;
    uint32_t op = ci_insn & 0xff800000u;
    bool is_sub = allow_sub && (op == (is64 ? 0xd1000000u : 0x51000000u) ||
                                op == (is64 ? 0xf1000000u : 0x71000000u));
    bool is_add = allow_add && (op == (is64 ? 0x91000000u : 0x11000000u) ||
                                op == (is64 ? 0xb1000000u : 0x31000000u));
    if (!is_sub && !is_add)
        return false;
    uint32_t imm = addsub_imm(ci_insn);
    if (imm < 1 || imm > 4096)
        return false;
    fl->kind = is_sub ? FL_COUNTDOWN : FL_COUNTUP_ZERO;
    fl->ctrl_idx = ci;
    fl->cnt_reg = c;
    fl->is_64 = is64;
    fl->step = imm;
    fl->cd_cond = is_sub ? sub_cond : add_cond;
    if (hdr) {
        fl->header_tested = true;
        fl->hdr_br_idx = hdr->idx;
    }
    return true;
}

static inline void fl_note_body_reg(const struct xlate *x, unsigned r, bool track_stolen,
                                    uint32_t *used, uint32_t *stolen)
{
    if (r < 31) {
        *used |= 1u << r;
        if (track_stolen && is_stolen(r))
            *stolen |= 1u << r;
    }
}

static unsigned fl_alloc_pin_host(const struct xlate *x, const unsigned base_pool[2],
                                  unsigned *base_used, uint32_t *avail,
                                  uint8_t bhost[FAST_LOOP_MAX_BORROWS], unsigned *nb)
{
    if (*base_used < 2)
        return base_pool[(*base_used)++];
    if (*nb >= FAST_LOOP_MAX_BORROWS)
        return 31;
    for (uint32_t cand = *avail; cand; cand &= cand - 1) {
        unsigned c = (unsigned)__builtin_ctz(cand);
        if (!is_stolen(c)) {
            *avail &= ~(1u << c);
            bhost[(*nb)++] = (uint8_t)c;
            return c;
        }
    }
    return 31;
}

static bool analyze_fast_loop(struct xlate *x, struct fast_loop *fl)
{
    if (x->limit == 0)
        return false;

    struct fl_state st = {.w32_valid = (1u << 31) - 1};
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
    struct fl_cand_se cands[FAST_LOOP_MAX_SIDE_EXITS + 1];
    unsigned ncands = 0;
    uint32_t used_as_ptr = 0;
    unsigned flag_writers = 0, last_flag_idx = 0;
    bool nzcv_written = false, nzcv_read_after_last_write = false;
    bool hdr_flag_read_by_other = false;
    unsigned body_words = 0, extra_body_words = 0;

    for (unsigned r = 0; r < 31; r++) {
        st.root[r] = (int8_t)r;
        st.sreg[r] = -1;
        st.pidx[r] = (struct fl_pidx){.root = -1, .idx_reg = -1, .idx_reg2 = -1};
        st.idx[r] = (struct fl_sym_idx){.reg0 = (int8_t)r, .reg1 = -1, .off = 0, .shift = 0};
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
        if (is_bcond(insn) || is_cbz_cbnz(insn) || is_tbz_tbnz(insn)) {
            bool is_tb = is_tbz_tbnz(insn);
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
            if (is_bcond(insn)) {
                if (!nzcv_written)
                    return false;
                if (ncands == 0 && nzcv_read_after_last_write)
                    hdr_flag_read_by_other = true;
                if (ncands > 0 && cands[0].has_flag && flag_writers == cands[0].flag_seq)
                    hdr_flag_read_by_other = true;
                nzcv_read_after_last_write = true;
            } else {
                unsigned rt = insn & 31;
                fl_mark_read(&st, rt);
                if (rt < 31 && is_stolen(rt))
                    body_words += 1;
            }
            struct fl_cand_se *ce = &cands[ncands++];
            ce->idx = i;
            ce->insn = insn;
            ce->va = va;
            ce->target = target;
            ce->flag_idx = last_flag_idx;
            ce->flag_seq = flag_writers;
            ce->has_flag = nzcv_written;
            ce->had_mem = nraw > 0 || nlits > 0;
            ce->written_mask = st.written;
            memcpy(ce->sym_root, st.root, sizeof(st.root));
            memcpy(ce->sym_sreg, st.sreg, sizeof(st.sreg));
            for (unsigned r = 0; r < 31; r++)
                ce->pidx_root[r] = st.pidx[r].root;
            memcpy(ce->sym_off, st.off, sizeof(st.off));
            body_words += 1;
            continue;
        }
        if (insn == INSN_NOP)
            continue;

        if ((top & 7) == 7) { /* SIMD&FP data processing */
            struct native_desc nd;
            if (!appine_has(APPINE_HWCAP_FP | APPINE_HWCAP_ASIMD) || !appine_native_available() ||
                !appine_simd_dp_decode(insn, &nd))
                return false;
            if (simd_reads_nzcv(insn)) {
                if (!nzcv_written)
                    return false;
                if (ncands > 0 && cands[0].has_flag && flag_writers == cands[0].flag_seq)
                    hdr_flag_read_by_other = true;
                nzcv_read_after_last_write = true;
            }
            if (simd_writes_nzcv(insn)) {
                nzcv_written = true;
                flag_writers++;
                last_flag_idx = i;
                nzcv_read_after_last_write = false;
            }
            struct operands o = {.n = 0, .writes_rd = false};
            for (unsigned k = 0; k < nd.n; k++) {
                add_operand(&o, nd.gpr[k].pos, false);
                if (nd.gpr[k].load)
                    fl_mark_read(&st, (insn >> nd.gpr[k].pos) & 31);
            }
            unsigned ns = count_distinct_stolen(x, insn, &o);
            if (ns > 2)
                return false;
            body_words += 1 + ns * 2;
            for (unsigned k = 0; k < nd.n; k++) {
                if (nd.gpr[k].store)
                    fl_clobber_reg(&st, (insn >> nd.gpr[k].pos) & 31, i, 0);
            }
            continue;
        }

        if (!dbt_tier0_defined(x->probe, x->defined, insn, va))
            return false;

        if (is_memop_top(top)) { /* Memory access */
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
                if (!m.simd)
                    fl_clobber_reg(&st, m.rt, i, 0);
                continue;
            }
            if (m.wb_reg || (m.rn == 31 && m.mode != M_OFFSET))
                return false;
            if ((m.mode == M_PRE || m.mode == M_POST) && !m.simd &&
                (m.rt == m.rn || (m.pair && m.rt2 == m.rn)))
                return false;
            if (m.pair && !m.simd && m.load && m.rt == m.rt2)
                return false;
            if (m.rn < 31 && (st.root[m.rn] < 0 || st.sreg[m.rn] >= 0))
                return false;

            unsigned idx_reg = 31, idx_reg2 = 31, idx_opt = 0, idx_shift = 0;
            int64_t idx_cur_off = 0;
            int64_t lut_extra_end = 0;
            int32_t w32_pre_off = 0;
            bool has_w32_pre = false;
            bool rn_has_pidx = m.rn < 31 && st.pidx[m.rn].root >= 0;
            if (rn_has_pidx) {
                if (m.mode == M_REG)
                    return false;
                const struct fl_pidx *pi = &st.pidx[m.rn];
                if (pi->lut_bits > 0) {
                    lut_extra_end = (((int64_t)1 << pi->lut_bits) - 1) << pi->idx_shift;
                } else {
                    idx_reg = (unsigned)pi->idx_reg;
                    idx_reg2 = pi->idx_reg2 >= 0 ? (unsigned)pi->idx_reg2 : 31;
                    fl_canonical_idx_pair(&idx_reg, &idx_reg2);
                    idx_opt = pi->idx_opt;
                    idx_shift = pi->idx_shift;
                    w32_pre_off = pi->w32_off;
                    has_w32_pre = is_ext32(idx_opt);
                    st.nonptr |= 1u << idx_reg;
                    if (idx_reg2 < 31)
                        st.nonptr |= 1u << idx_reg2;
                }
            } else if (m.mode == M_REG && m.rm < 31) {
                if (st.root[m.rm] == (int8_t)m.rm && st.sreg[m.rm] < 0) {
                    if (st.off[m.rm] != 0 && !is_ext64(m.option))
                        return false;
                    idx_reg = m.rm;
                    idx_opt = m.option;
                    idx_shift = m.shift;
                    idx_cur_off = st.off[m.rm] * (int64_t)(1u << m.shift);
                    if (is_ext32(idx_opt)) {
                        w32_pre_off = 0;
                        has_w32_pre = true;
                    }
                    fl_mark_read(&st, m.rm);
                } else if (is_ext32(m.option) && ((st.w32_valid >> m.rm) & 1) &&
                           st.w32_step[m.rm] != 0) {
                    idx_reg = m.rm;
                    idx_opt = m.option;
                    idx_shift = m.shift;
                    idx_cur_off = (int64_t)st.w32_step[m.rm] * (int64_t)(1u << m.shift);
                    w32_pre_off = st.w32_step[m.rm];
                    has_w32_pre = true;
                    fl_mark_read(&st, m.rm);
                } else if (is_ext64(m.option) && st.idx[m.rm].reg0 >= 0 &&
                           st.idx[m.rm].shift + m.shift <= 4) {
                    unsigned tot_shift = st.idx[m.rm].shift + m.shift;
                    idx_reg = (unsigned)st.idx[m.rm].reg0;
                    idx_reg2 = st.idx[m.rm].reg1 >= 0 ? (unsigned)st.idx[m.rm].reg1 : 31;
                    fl_canonical_idx_pair(&idx_reg, &idx_reg2);
                    idx_opt = m.option;
                    idx_shift = tot_shift;
                    idx_cur_off = st.idx[m.rm].off * (int64_t)(1u << tot_shift);
                    fl_mark_read(&st, m.rm);
                    st.nonptr |= 1u << idx_reg;
                    if (idx_reg2 < 31)
                        st.nonptr |= 1u << idx_reg2;
                } else if (fl_lut_fits(st.u_bits[m.rm], m.shift) &&
                           (is_ext32(m.option) || is_ext64(m.option))) {
                    /* Bounded table-lookup M_REG stream: [base, rm, opt #shift] stays within
                     * [0, ((1 << u_bits[rm]) - 1) << shift + bytes). */
                    lut_extra_end = (((int64_t)1 << st.u_bits[m.rm]) - 1) << m.shift;
                    fl_mark_read(&st, m.rm);
                } else {
                    return false;
                }
            }

            unsigned seen[2], ns = 0;
            if (!fl_add_memop_stolen(x, seen, &ns, m.rn))
                return false;
            if (m.mode == M_REG && !fl_add_memop_stolen(x, seen, &ns, m.rm))
                return false;
            if (!m.simd) {
                if (!fl_add_memop_stolen(x, seen, &ns, m.rt))
                    return false;
                if (m.pair && !fl_add_memop_stolen(x, seen, &ns, m.rt2))
                    return false;
            }
            body_words +=
                1 + ns * 2 + ((m.mode == M_PRE || m.mode == M_POST) && m.rn < 31 && is_stolen(m.rn) ? 1 : 0);

            if (m.rn < 31 && !(st.written & (1u << m.rn)))
                st.rbw |= 1u << m.rn;
            unsigned root = m.rn == 31 ? 31 : (unsigned)st.root[m.rn];
            int64_t base_off = m.rn == 31 ? 0 : (rn_has_pidx ? st.pidx[m.rn].off : st.off[m.rn]);
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
                fl_mark_read(&st, m.rt);
                if (m.pair)
                    fl_mark_read(&st, m.rt2);
            }
            if (m.mode == M_PRE || m.mode == M_POST) {
                st.off[m.rn] += m.imm;
                if (st.pidx[m.rn].root >= 0)
                    st.pidx[m.rn].off += m.imm;
                if (st.idx[m.rn].reg0 >= 0 && st.idx[m.rn].shift == 0)
                    st.idx[m.rn].off += m.imm;
                else
                    st.idx[m.rn].reg0 = -1;
                st.u_bits[m.rn] = 0;
                st.w32_valid &= ~(1u << m.rn);
                fl_note_write(&st, m.rn, i);
            }
            if (m.load && !m.simd) {
                uint8_t load_ub = 0;
                if (!m.pair && FIELD(insn, 23, 22) == 1) { /* unsigned LDRB/LDRH */
                    if (m.size == 0)
                        load_ub = 8;
                    else if (m.size == 1)
                        load_ub = 16;
                }
                fl_clobber_reg(&st, m.rt, i, load_ub);
                if (m.pair)
                    fl_clobber_reg(&st, m.rt2, i, 0);
            }
            continue;
        }

        if (top == 0x8 || top == 0x9 || top == 0x5 || top == 0xd) { /* Integer DP */
            if (is_adr_adrp(insn)) { /* ADR / ADRP: compile-time constant VA */
                unsigned rd = insn & 31;
                extra_body_words += 3;
                body_words += 4 + (rd < 31 && is_stolen(rd) ? 1 : 0);
                fl_clobber_reg(&st, rd, i, 0);
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
                if (!(st.written & (1u << rn)))
                    st.rbw |= 1u << rn;
                int64_t imm = (int64_t)addsub_imm(insn);
                if (op == 0xd1000000)
                    imm = -imm;
                struct fl_sym_idx idx_in = st.idx[rn];
                struct fl_pidx pidx_in = st.pidx[rn];
                if (st.root[rn] >= 0) {
                    st.root[rd] = st.root[rn];
                    st.sreg[rd] = st.sreg[rn];
                    st.off[rd] = st.off[rn] + imm;
                } else {
                    st.root[rd] = -1;
                    st.sreg[rd] = -1;
                }
                if (pidx_in.root >= 0) {
                    pidx_in.off += imm;
                    st.pidx[rd] = pidx_in;
                } else {
                    st.pidx[rd].root = -1;
                }
                if (idx_in.reg0 >= 0 && idx_in.shift == 0) {
                    idx_in.off += imm;
                    st.idx[rd] = idx_in;
                } else {
                    st.idx[rd].reg0 = -1;
                }
                st.u_bits[rd] = 0;
                st.w32_valid &= ~(1u << rd);
                fl_note_write(&st, rd, i);
                continue;
            }
            /* 64-bit ADDS/SUBS Xd, Xd, #imm (rd < 31): tracks 64-bit non-pointer index/counter step */
            if ((op == 0xb1000000 || op == 0xf1000000) && rd == rn && rd < 31) {
                int64_t imm = (int64_t)addsub_imm(insn);
                if (op == 0xf1000000)
                    imm = -imm;
                fl_mark_read(&st, rn);
                if (st.root[rd] == (int8_t)rd && st.sreg[rd] < 0 && st.pidx[rd].root < 0)
                    st.off[rd] += imm;
                else {
                    st.root[rd] = -1;
                    st.sreg[rd] = -1;
                    st.pidx[rd].root = -1;
                }
                if (st.idx[rd].reg0 >= 0 && st.idx[rd].shift == 0)
                    st.idx[rd].off += imm;
                else
                    st.idx[rd].reg0 = -1;
                st.u_bits[rd] = 0;
                st.w32_valid &= ~(1u << rd);
                fl_note_write(&st, rd, i);
                continue;
            }
            /* 64-bit MOV Xd, Xm (ORR Xd, XZR, Xm): preserves pointer affine relation & u_bits */
            if ((insn & 0xffe0ffe0) == 0xaa0003e0) {
                unsigned rm = FIELD(insn, 20, 16);
                if (rm < 31 && !(st.written & (1u << rm)))
                    st.rbw |= 1u << rm;
                if (rd < 31) {
                    struct fl_sym_idx idx_in =
                        rm < 31 ? st.idx[rm] : (struct fl_sym_idx){.reg0 = -1};
                    struct fl_pidx pidx_in = rm < 31 ? st.pidx[rm] : (struct fl_pidx){.root = -1};
                    uint8_t ub_in = rm < 31 ? st.u_bits[rm] : 0;
                    if (rm < 31 && st.root[rm] >= 0) {
                        st.root[rd] = st.root[rm];
                        st.sreg[rd] = st.sreg[rm];
                        st.off[rd] = st.off[rm];
                    } else {
                        st.root[rd] = -1;
                        st.sreg[rd] = -1;
                    }
                    st.pidx[rd] = pidx_in;
                    st.idx[rd] = idx_in;
                    st.u_bits[rd] = ub_in;
                    st.w32_valid &= ~(1u << rd);
                    fl_note_write(&st, rd, i);
                }
                continue;
            }
            /* 64-bit ADD Xd, Xn, Xm{, LSL #0..4} or ADD Xd, Xn, Xm/Wm, UXTW/SXTW/UXTX/SXTX #0..4:
             * tracks in-place register stride, shifted derived pointer, or derived sum index. */
            unsigned ext_opt = FIELD(insn, 15, 13);
            bool is_add_lsl = (insn & 0xffe00000) == 0x8b000000 && FIELD(insn, 15, 10) <= 4;
            bool is_add_ext = (insn & 0xffe00000) == 0x8b200000 && FIELD(insn, 12, 10) <= 4 &&
                              (is_ext32(ext_opt) || is_ext64(ext_opt));
            if (is_add_lsl || is_add_ext) {
                unsigned rm = FIELD(insn, 20, 16);
                unsigned opt = is_add_ext ? ext_opt : EXT_LSL;
                unsigned sh = is_add_ext ? FIELD(insn, 12, 10) : FIELD(insn, 15, 10);
                /* In-place register stride (unshifted ADD Xd, Xd, Xm) */
                if (is_add_lsl && sh == 0) {
                    unsigned sreg = (rd == rn) ? rm : (rd == rm ? rn : 31);
                    if (rd < 31 && sreg < 31 && sreg != rd && st.root[rd] == (int8_t)rd &&
                        st.sreg[rd] == -1 && st.pidx[rd].root < 0) {
                        if (!(st.written & (1u << rd)))
                            st.rbw |= 1u << rd;
                        fl_mark_read(&st, sreg);
                        st.sreg[rd] = (int8_t)sreg;
                        st.idx[rd].reg0 = -1;
                        st.u_bits[rd] = 0;
                        st.w32_valid &= ~(1u << rd);
                        fl_note_write(&st, rd, i);
                        continue;
                    }
                }
                /* Derived pointer Xd = Xn + ext(Xm) << sh (and for unshifted, also sym_idx) */
                if (rd < 31 && rn < 31 && rm < 31 && st.root[rn] >= 0 && st.sreg[rn] < 0 &&
                    st.pidx[rn].root < 0 && st.root[rn] != (int8_t)rd) {
                    struct fl_pidx np = {.root = -1, .idx_reg = -1, .idx_reg2 = -1};
                    struct fl_sym_idx nidx = {.reg0 = -1, .reg1 = -1};
                    uint32_t extra_nonptr = 0;
                    if (st.root[rm] == (int8_t)rm && st.sreg[rm] < 0 &&
                        (st.off[rm] == 0 || is_ext64(opt))) {
                        int64_t off = st.off[rn] + st.off[rm] * (int64_t)(1u << sh);
                        np = (struct fl_pidx){
                            .root = st.root[rn],
                            .idx_reg = (int8_t)rm,
                            .idx_reg2 = -1,
                            .idx_opt = (uint8_t)opt,
                            .idx_shift = (uint8_t)sh,
                            .off = off,
                        };
                        if (is_add_lsl && sh == 0) {
                            nidx = (struct fl_sym_idx){
                                .reg0 = st.root[rn],
                                .reg1 = st.root[rm],
                                .off = st.off[rn] + st.off[rm],
                                .shift = 0,
                            };
                        }
                    } else if (is_ext32(opt) && ((st.w32_valid >> rm) & 1) &&
                               st.w32_step[rm] != 0) {
                        np = (struct fl_pidx){
                            .root = st.root[rn],
                            .idx_reg = (int8_t)rm,
                            .idx_reg2 = -1,
                            .idx_opt = (uint8_t)opt,
                            .idx_shift = (uint8_t)sh,
                            .w32_off = st.w32_step[rm],
                            .off = st.off[rn] + (int64_t)st.w32_step[rm] * (int64_t)(1u << sh),
                        };
                    } else if (is_ext64(opt) && st.idx[rm].reg0 >= 0 &&
                               st.idx[rm].shift + sh <= 4) {
                        unsigned tot_sh = st.idx[rm].shift + sh;
                        np = (struct fl_pidx){
                            .root = st.root[rn],
                            .idx_reg = st.idx[rm].reg0,
                            .idx_reg2 = st.idx[rm].reg1,
                            .idx_opt = (uint8_t)opt,
                            .idx_shift = (uint8_t)tot_sh,
                            .off = st.off[rn] + st.idx[rm].off * (int64_t)(1u << tot_sh),
                        };
                        extra_nonptr |= 1u << (unsigned)st.idx[rm].reg0;
                        if (st.idx[rm].reg1 >= 0)
                            extra_nonptr |= 1u << (unsigned)st.idx[rm].reg1;
                    } else if (fl_lut_fits(st.u_bits[rm], sh)) {
                        np = (struct fl_pidx){
                            .root = st.root[rn],
                            .idx_reg = -1,
                            .idx_reg2 = -1,
                            .idx_opt = 0,
                            .idx_shift = (uint8_t)sh,
                            .lut_bits = st.u_bits[rm],
                            .off = st.off[rn],
                        };
                    } else if (is_add_lsl && sh == 0 && st.root[rm] >= 0 && st.sreg[rm] < 0) {
                        nidx = (struct fl_sym_idx){
                            .reg0 = st.root[rn],
                            .reg1 = st.root[rm],
                            .off = st.off[rn] + st.off[rm],
                            .shift = 0,
                        };
                    }
                    if (np.root >= 0) {
                        if (!(st.written & (1u << rn)))
                            st.rbw |= 1u << rn;
                        fl_mark_read(&st, rm);
                        st.nonptr |= extra_nonptr;
                        st.root[rd] = np.root;
                        st.sreg[rd] = -1;
                        st.off[rd] = np.off;
                        st.pidx[rd] = np;
                        st.idx[rd] = nidx;
                        st.u_bits[rd] = 0;
                        st.w32_valid &= ~(1u << rd);
                        fl_note_write(&st, rd, i);
                        continue;
                    }
                    if (nidx.reg0 >= 0) {
                        fl_mark_read(&st, rn);
                        fl_mark_read(&st, rm);
                        fl_clobber_reg(&st, rd, i, 0);
                        st.idx[rd] = nidx;
                        continue;
                    }
                }
            }
            /* 64-bit LSL Xd, Xn, #sh (UBFM Xd, Xn, #64-sh, #63-sh, 1 <= sh <= 4): shifts derived index */
            if ((insn & 0xffc00000) == 0xd3400000 && rd < 31 && rn < 31) {
                unsigned immr = FIELD(insn, 21, 16), imms = FIELD(insn, 15, 10);
                if (imms != 63 && immr == imms + 1) {
                    unsigned sh = 64 - immr;
                    if (sh >= 1 && sh <= 4 && st.idx[rn].reg0 >= 0 && st.idx[rn].shift + sh <= 4) {
                        struct fl_sym_idx s = st.idx[rn];
                        s.shift += (uint8_t)sh;
                        fl_mark_read(&st, rn);
                        fl_clobber_reg(&st, rd, i, 0);
                        st.idx[rd] = s;
                        continue;
                    }
                }
            }
            /* 32-bit ADD/SUB/ADDS/SUBS Wd, Wd, #imm: tracks monotonic 32-bit induction step */
            if ((op == 0x11000000 || op == 0x31000000 || op == 0x51000000 || op == 0x71000000) &&
                rd == rn && rd < 31) {
                int32_t imm = (int32_t)addsub_imm(insn);
                bool is_sub = op == 0x51000000 || op == 0x71000000;
                int32_t delta = is_sub ? -imm : imm;
                fl_mark_read(&st, rn);
                st.root[rd] = -1;
                st.sreg[rd] = -1;
                st.pidx[rd].root = -1;
                st.idx[rd].reg0 = -1;
                st.u_bits[rd] = 0;
                int32_t next = st.w32_step[rd] + delta;
                bool same_dir = st.w32_step[rd] == 0 || (st.w32_step[rd] > 0) == (delta > 0);
                if (imm > 0 && same_dir && next >= -4096 && next <= 4096)
                    st.w32_step[rd] = next;
                else
                    st.w32_valid &= ~(1u << rd);
                fl_note_write(&st, rd, i);
                continue;
            }
            /* CMP Xn, Xm / CMP Wn, Wm / CMP Xn, #imm / CMP Wn, #imm: may be omitted when it is
             * the loop-closing or header-testing CMP, so defer non-pointer tainting until after
             * control matching. */
            if ((is_cmp_reg(insn) || is_cmp_imm(insn)) && writes_flags) {
                int8_t r0 = rn < 31 ? st.root[rn] : -1;
                int8_t r1 = -1;
                if (rn < 31 && !(st.written & (1u << rn)))
                    st.rbw |= 1u << rn;
                if (is_cmp_reg(insn)) {
                    unsigned rm = FIELD(insn, 20, 16);
                    if (rm < 31) {
                        r1 = st.root[rm];
                        if (!(st.written & (1u << rm)))
                            st.rbw |= 1u << rm;
                    }
                }
                if (ncmps < 8) {
                    cmps[ncmps++] = (struct cmp_rec){.idx = i, .root0 = r0, .root1 = r1};
                } else {
                    if (r0 >= 0)
                        st.nonptr |= 1u << (unsigned)r0;
                    if (r1 >= 0)
                        st.nonptr |= 1u << (unsigned)r1;
                }
                continue;
            }

            /* General DP instruction: reading any register taints its root as non-pointer */
            uint8_t new_ub = (o.writes_rd && rd < 31) ? u_bits_from_dp(insn, st.u_bits) : 0;
            for (unsigned k = 0; k < o.n; k++) {
                if (o.pos[k] == 0 && o.writes_rd && !o.reads_rd)
                    continue;
                fl_mark_read(&st, (insn >> o.pos[k]) & 31);
            }
            if (o.writes_rd)
                fl_clobber_reg(&st, rd, i, new_ub);
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
        const struct fl_cand_se *hdr = &cands[0];
        if (!hdr->had_mem) {
            uint32_t hi = hdr->insn;
            if ((hi & 0x7f000000u) == 0x34000000u) {
                /* CBZ Xc/Wc, exit: exits when c == 0, continues while c != 0 */
                matched_ctrl = fl_match_imm_step(x, &st, used_as_ptr, hi & 31, BIT(hi, 31), true,
                                                 true, COND_NE, COND_NE, hdr, fl);
            } else if (is_tbz_tbnz(hi)) {
                /* TBNZ #63/#31 (continue while >=0) + SUB/SUBS, or
                 * TBZ  #63/#31 (continue while <0)  + ADD/ADDS. */
                unsigned bit = (BIT(hi, 31) << 5) | FIELD(hi, 23, 19);
                bool is_tbnz = BIT(hi, 24);
                if (bit == 63 || bit == 31)
                    matched_ctrl = fl_match_imm_step(x, &st, used_as_ptr, hi & 31, bit == 63,
                                                     is_tbnz, !is_tbnz, COND_PL, COND_MI, hdr, fl);
            } else if (is_bcond(hi) && hdr->has_flag) {
                unsigned cont_cond = (hi & 15) ^ 1;
                uint32_t fi_insn = x->words[hdr->flag_idx];
                if (fl_match_cmp(fi_insn, hdr->flag_idx, cont_cond, hdr, &st, used_as_ptr, fl)) {
                    fl->header_tested = true;
                    fl->hdr_br_idx = hdr->idx;
                    fl->omit_ctrl = true;
                    matched_ctrl = true;
                }
            }
        }
    } else if ((back_insn & 0x7f000000u) == 0x35000000u) {
        /* CBNZ Xc/Wc: SUB/SUBS (countdown) or ADD/ADDS (negative counter counting up to 0) */
        matched_ctrl = fl_match_imm_step(x, &st, used_as_ptr, back_insn & 31, BIT(back_insn, 31),
                                         true, true, COND_NE, COND_NE, NULL, fl);
    } else if (is_tbz_tbnz(back_insn)) {
        /* TBZ  Xc/Wc, #63/#31 (continue while >=0) + SUB/SUBS, or
         * TBNZ Xc/Wc, #63/#31 (continue while <0)  + ADD/ADDS. */
        unsigned bit = (BIT(back_insn, 31) << 5) | FIELD(back_insn, 23, 19);
        bool is_tbnz = BIT(back_insn, 24);
        if (bit == 63 || bit == 31)
            matched_ctrl = fl_match_imm_step(x, &st, used_as_ptr, back_insn & 31, bit == 63,
                                             !is_tbnz, is_tbnz, COND_PL, COND_MI, NULL, fl);
    } else if (is_bcond(back_insn) && flag_writers >= 1 && !nzcv_read_after_last_write) {
        unsigned cond = back_insn & 15;
        unsigned flag_idx = last_flag_idx;
        uint32_t fi_insn = x->words[flag_idx];
        /* Pattern 1/1b: SUBS + B.NE/PL/HI/GE/GT or ADDS + B.NE/MI/LT */
        bool is_subs = (fi_insn & 0x7f800000u) == 0x71000000u;
        bool is_adds = (fi_insn & 0x7f800000u) == 0x31000000u;
        bool cond_ok = (is_subs && (cond == COND_NE || cond == COND_PL || cond == COND_HI ||
                                    cond == COND_GE || cond == COND_GT)) ||
                       (is_adds && (cond == COND_NE || cond == COND_MI || cond == COND_LT));
        if (cond_ok && (fi_insn & 31) < 31 && (fi_insn & 31) == FIELD(fi_insn, 9, 5)) {
            unsigned c = fi_insn & 31;
            uint32_t imm = addsub_imm(fi_insn);
            if (st.write_count[c] == 1 && !(used_as_ptr & (1u << c)) && imm >= 1 && imm <= 4096) {
                fl->kind = is_subs ? FL_COUNTDOWN : FL_COUNTUP_ZERO;
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
            fl_match_cmp(fi_insn, flag_idx, cond, NULL, &st, used_as_ptr, fl)) {
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
            st.nonptr |= 1u << (unsigned)cmps[k].root0;
        if (cmps[k].root1 >= 0)
            st.nonptr |= 1u << (unsigned)cmps[k].root1;
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
        if (st.nonptr & (1u << r))
            return false;
        if (st.root[r] != (int8_t)r || st.pidx[r].root >= 0)
            return false;
        if (st.sreg[r] >= 0) {
            unsigned sr = (unsigned)st.sreg[r];
            if (st.off[r] != 0 || (st.written & (1u << sr)) || (used_as_ptr & (1u << sr)))
                return false;
        } else if (st.off[r] < -4095 || st.off[r] > 4095) {
            return false;
        }
        uint32_t alias_mask = 0;
        for (unsigned a = 0; a < 31; a++) {
            if (a != r && st.root[a] == (int8_t)r) {
                /* An alias carrying addend_r at iteration end must not have been read before its
                 * first write (otherwise iteration 2 would observe the shifted pointer value). */
                if (st.rbw & (1u << a))
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
        const struct fl_cand_se *ce = &cands[k];
        for (unsigned i = 0; i < fl->nptrs; i++) {
            unsigned r = fl->ptrs[i].reg;
            if (r < 31 && ce->sym_root[r] != (int8_t)r)
                return false;
        }
        for (unsigned a = 0; a < 31; a++) {
            int8_t r_se = fl_alias_root(ce->sym_root, a, used_as_ptr);
            int8_t r_end = fl_alias_root(st.root, a, used_as_ptr);
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
        uint8_t sreg = (r < 31 && st.sreg[r] >= 0) ? (uint8_t)st.sreg[r] : 31;
        int64_t cstride = (r == 31 || sreg < 31) ? 0 : st.off[r];
        int32_t idx_w32_step = 0, idx_w32_pre_off = 0;
        unsigned idx_regs[2] = {raw[s].idx_reg, raw[s].idx_reg2};
        for (unsigned k = 0; k < 2; k++) {
            unsigned ir = idx_regs[k];
            if (ir < 31 && (st.written & (1u << ir))) {
                if (sreg < 31)
                    return false;
                if (is_ext64(raw[s].idx_opt)) {
                    if (!fl_has_const_step(&st, ir))
                        return false;
                    cstride += st.off[ir] * (int64_t)(1u << raw[s].idx_shift);
                } else if (is_ext32(raw[s].idx_opt)) {
                    if (raw[s].idx_reg2 < 31 || !((st.w32_valid >> ir) & 1) || st.w32_step[ir] == 0)
                        return false;
                    cstride += (int64_t)st.w32_step[ir] * (int64_t)(1u << raw[s].idx_shift);
                    idx_w32_step = st.w32_step[ir];
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
        if (insn == INSN_NOP || (fl->header_tested && i == fl->hdr_br_idx) || is_bcond(insn))
            continue;
        bool track_stolen = !(fl->omit_ctrl && i == fl->ctrl_idx);
        if (is_cbz_cbnz(insn) || is_tbz_tbnz(insn)) {
            fl_note_body_reg(x, insn & 31, true, &body_used, &body_stolen);
            continue;
        }
        unsigned top = FIELD(insn, 28, 25);
        if ((top & 7) == 7) {
            struct native_desc nd;
            appine_simd_dp_decode(insn, &nd);
            for (unsigned k = 0; k < nd.n; k++)
                fl_note_body_reg(x, (insn >> nd.gpr[k].pos) & 31, track_stolen, &body_used,
                                 &body_stolen);
        } else if (is_memop_top(top)) {
            struct memop m;
            uint64_t va = x->pc + 4ull * i;
            if (BIT(insn, 26))
                dbt_decode_simd_memop(insn, va, &m);
            else
                dbt_decode_memop(insn, va, &m);
            if (m.prefetch)
                continue;
            if (m.mode == M_LITERAL) {
                if (!m.simd)
                    fl_note_body_reg(x, m.rt, true, &body_used, &body_stolen);
                continue;
            }
            fl_note_body_reg(x, m.rn, true, &body_used, &body_stolen);
            if (m.mode == M_REG)
                fl_note_body_reg(x, m.rm, true, &body_used, &body_stolen);
            if (!m.simd) {
                fl_note_body_reg(x, m.rt, true, &body_used, &body_stolen);
                if (m.pair)
                    fl_note_body_reg(x, m.rt2, true, &body_used, &body_stolen);
            }
        } else if (is_adr_adrp(insn)) {
            fl_note_body_reg(x, insn & 31, true, &body_used, &body_stolen);
        } else {
            struct operands o;
            dbt_dp_operands(insn, &o);
            for (unsigned k = 0; k < o.n; k++)
                fl_note_body_reg(x, (insn >> o.pos[k]) & 31, track_stolen, &body_used,
                                 &body_stolen);
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
        uint8_t preg[FAST_LOOP_MAX_PINS], phost[FAST_LOOP_MAX_PINS], bhost[FAST_LOOP_MAX_BORROWS];
        bool pwr[FAST_LOOP_MAX_PINS];
        bool ok = true;
        for (uint32_t bits = body_stolen; bits; bits &= bits - 1) {
            unsigned r = (unsigned)__builtin_ctz(bits);
            unsigned hr = fl_alloc_pin_host(x, base_pool, &base_used, &avail, bhost, &nb);
            if (hr >= 31 || n >= FAST_LOOP_MAX_PINS) {
                ok = false;
                break;
            }
            preg[n] = (uint8_t)r;
            phost[n] = (uint8_t)hr;
            pwr[n] = ((st.written >> r) & 1) != 0;
            n++;
        }
        uint8_t lhost[FAST_LOOP_MAX_LITS];
        for (unsigned l = 0; l < fl->nlits && ok; l++) {
            unsigned hr = fl_alloc_pin_host(x, base_pool, &base_used, &avail, bhost, &nb);
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

struct fast_bails {
    uint32_t *sites[FAST_LOOP_MAX_BAILS];
    unsigned n;
};

static inline void emit_bail(struct xlate *x, struct fast_bails *b, uint32_t insn)
{
    if (b->n >= FAST_LOOP_MAX_BAILS)
        appine_guard_fail();
    b->sites[b->n++] = here(x);
    emit(x, insn);
}

/* Load guest_reg (stolen or live) into dst, sign/zero-extending 32-bit values to 64 bits. */
static void emit_load_ext_reg(struct xlate *x, unsigned dst, unsigned guest_reg, bool is_64,
                              bool is_signed)
{
    unsigned src = guest_reg;
    if (is_stolen(src)) {
        emit_load_guest(x, dst, src);
        src = dst;
    }
    if (!is_64)
        emit(x, is_signed ? enc_sbfx(dst, src, 0, 32) : enc_ubfx(dst, src, 0, 32));
    else if (src != dst)
        emit(x, enc_mov(dst, src));
}

/* Unsigned divide reg by constant step > 1 (shift for power-of-2, UDIV using scratch otherwise). */
static void emit_div_step(struct xlate *x, unsigned reg, unsigned scratch, uint32_t step)
{
    if (step <= 1)
        return;
    if ((step & (step - 1)) == 0) {
        unsigned sh = (unsigned)__builtin_ctz(step);
        emit(x, enc_ubfx(reg, reg, sh, 64 - sh));
    } else {
        emit_mov_const(x, scratch, step);
        emit(x, enc_udiv(reg, reg, scratch));
    }
}

/* T2 = q = safe_iters - 1: bail if q == 0 (< 2 iters), clamp q to (1<<kick_log2)-1, add 1. */
static void emit_clamp_iters_from_q(struct xlate *x, unsigned kick_log2, struct fast_bails *bails)
{
    emit_bail(x, bails, enc_cbz_x(T2));
    emit(x, enc_ubfx(T0, T2, kick_log2, 64 - kick_log2));
    uint32_t *no_clamp = here(x);
    emit(x, enc_cbz_x(T0));
    emit(x, enc_movz(T2, (1u << kick_log2) - 1, 0));
    patch_local(x, no_clamp, here(x));
    emit(x, enc_add_imm(T2, T2, 1, 0));
}

static inline unsigned emit_load_base(struct xlate *x, unsigned reg)
{
    if (reg == 31)
        return SP_HOST;
    if (is_stolen(reg)) {
        emit_load_guest(x, T0, reg);
        return T0;
    }
    return reg;
}

/* Probe 4 KiB data TLB for guest address in T0; leaves addend in T2, clobbers T1, bails on miss. */
static void emit_fast_tlb4k_probe(struct xlate *x, bool write, struct fast_bails *bails)
{
    emit(x, enc_ubfx(T1, T0, APPINE_PAGE_SHIFT, TLB_BITS));
    emit(x, enc_add_lsl(T1, CTX, T1, 5));
    emit(x, enc_ldp_x(T1, T2, T1, (int)TLB_OFF + (write ? 16 : 0)));
    emit(x, enc_eon_lsr(T1, T1, T0, APPINE_PAGE_SHIFT));
    emit_bail(x, bails, enc_cbnz_x(T1));
}

/* Save addend T2 to dbt_groups[ptr_idx] (first stream for root) or verify equality using scratch. */
static void emit_fast_check_addend(struct xlate *x, size_t grp_off, unsigned ptr_idx, bool save,
                                   unsigned scratch, struct fast_bails *bails)
{
    size_t slot_off = grp_off + 8 * (size_t)ptr_idx;
    if (save) {
        emit(x, enc_str_x(T2, CTX, slot_off));
    } else {
        emit(x, enc_ldr_x(scratch, CTX, slot_off));
        emit(x, enc_eor_lsr(scratch, scratch, T2, 0));
        emit_bail(x, bails, enc_cbnz_x(scratch));
    }
}

/* Compute in-page runway from guest address in T0 for a page of size (1 << page_shift).
 * Positive stride: T1 = avail = (page_size - span) - page_off (negative if iter 0 crosses page).
 * Negative stride: T0 = (page_size - span) - page_off (upper span check), T1 = avail = page_off. */
static void emit_fast_page_avail(struct xlate *x, unsigned page_shift, uint64_t page_size,
                                 uint32_t span, bool neg_stride)
{
    emit(x, enc_and_low(T1, T0, page_shift));
    emit_mov_const(x, T0, page_size - span);
    if (neg_stride)
        emit(x, enc_sub_reg(T0, T0, T1));
    else
        emit(x, enc_sub_reg(T1, T0, T1));
}

static void emit_fast_loop(struct xlate *x, const struct fast_loop *fl)
{
    struct fast_bails bails = {.n = 0};
    size_t grp_off = offsetof(struct appine_cpu, dbt_groups);
    size_t iters_off = grp_off + 8 * 7;

    /* 1. Never bypass active hardware watchpoints; if SP is accessed, require 16-byte alignment */
    size_t nwatch = offsetof(struct appine_cpu, nwatch);
    emit_add_const(x, T0, CTX, (int64_t)(nwatch & ~(size_t)0xfff));
    emit(x, enc_ldr_w(T0, T0, nwatch & 0xfff));
    emit_bail(x, &bails, enc_cbnz_x(T0));
    if (fl->uses_sp) {
        emit(x, enc_and_low(T0, SP_HOST, 4));
        emit_bail(x, &bails, enc_cbnz_x(T0));
    }

    /* 2. Compute safe_iters into T2 (NZCV-free), clamp to KICK_BUDGET, require >= 2 */
    if (fl->kind == FL_COUNTDOWN || fl->kind == FL_COUNTUP_ZERO) {
        if (fl->kind == FL_COUNTDOWN) {
            bool is_signed =
                fl->cd_cond == COND_PL || fl->cd_cond == COND_GE || fl->cd_cond == COND_GT;
            emit_load_ext_reg(x, T2, fl->cnt_reg, fl->is_64, is_signed);
            if (is_signed)
                emit_bail(x, &bails, enc_tbnz(T2, 63));
            if (fl->cd_cond == COND_NE || fl->cd_cond == COND_HI || fl->cd_cond == COND_GT) {
                emit_bail(x, &bails, enc_cbz_x(T2));
                emit(x, enc_sub_imm(T2, T2, 1, 0));
            }
        } else { /* FL_COUNTUP_ZERO: negative counter advancing up to 0 */
            bool is_signed = fl->cd_cond == COND_MI || fl->cd_cond == COND_LT;
            emit_load_ext_reg(x, T2, fl->cnt_reg, fl->is_64, is_signed);
            if (is_signed)
                emit_bail(x, &bails, enc_tbz(T2, 63));
            emit(x, enc_sub_reg(T2, 31, T2)); /* dist = -cnt (mod 2^W) */
            if (!fl->is_64 && !is_signed)
                emit(x, enc_ubfx(T2, T2, 0, 32));
            emit_bail(x, &bails, enc_cbz_x(T2));
            emit(x, enc_sub_imm(T2, T2, 1, 0));
        }
        emit_div_step(x, T2, T0, fl->step);
        if (fl->header_tested) {
            emit_clamp_iters_from_q(x, fl->kick_log2, &bails);
        } else {
            /* T2 = rem_backedges: clamp to 1 << kick_log2, bail if < 2 */
            emit(x, enc_ubfx(T0, T2, fl->kick_log2, 64 - fl->kick_log2));
            uint32_t *no_clamp = here(x);
            emit(x, enc_cbz_x(T0));
            emit(x, enc_movz(T2, 1u << fl->kick_log2, 0));
            patch_local(x, no_clamp, here(x));
            emit(x, enc_ubfx(T0, T2, 1, 63));
            emit_bail(x, &bails, enc_cbz_x(T0));
        }
    } else {
        /* FL_CMP_INC: diff = bound - v_0 (A=bound in T0, B=v_0 in T1).
         * FL_CMP_DEC: diff = v_0 - bound (A=v_0 in T0, B=bound in T1). */
        bool is_signed = fl->cmp == FL_SLT || fl->cmp == FL_SLE;
        unsigned reg_bound = fl->kind == FL_CMP_DEC ? T1 : T0;
        unsigned reg_v0 = fl->kind == FL_CMP_DEC ? T0 : T1;
        if (fl->bound_is_imm)
            emit_mov_const(x, reg_bound, fl->bound_imm);
        else
            emit_load_ext_reg(x, reg_bound, fl->bound_reg, fl->is_64, is_signed);
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
                emit_bail(x, &bails, enc_cbz_x(T2));
            } else {
                emit_bail(x, &bails, enc_tbnz(T2, 63));
                emit_bail(x, &bails, enc_cbz_x(T2));
            }
        } else {
            emit_bail(x, &bails, enc_cbz_x(T2));
            if (fl->cmp == FL_ULT || fl->cmp == FL_ULE) {
                /* unsigned borrow = d ^ ((a ^ b) & (b ^ d)) */
                emit(x, enc_eor_lsr(T0, T0, T1, 0));
                emit(x, enc_eor_lsr(T1, T1, T2, 0));
                emit(x, enc_and_reg(T0, T0, T1));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                emit_bail(x, &bails, enc_tbnz(T0, 63));
            } else if (fl->cmp == FL_SLT || fl->cmp == FL_SLE) {
                /* signed_lt = d ^ ((a ^ b) & (a ^ d)) */
                emit(x, enc_eor_lsr(T1, T0, T1, 0));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                emit(x, enc_and_reg(T0, T0, T1));
                emit(x, enc_eor_lsr(T0, T0, T2, 0));
                emit_bail(x, &bails, enc_tbnz(T0, 63));
            }
        }
        if (fl->cmp == FL_NE || fl->cmp == FL_SLT || fl->cmp == FL_ULT)
            emit(x, enc_sub_imm(T2, T2, 1, 0));
        emit_div_step(x, T2, T0, fl->step);
        emit_clamp_iters_from_q(x, fl->kick_log2, &bails);
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
            base = emit_load_base(x, base);
            emit(x, enc_add_lsl(T0, base, T1, s->idx_shift));
            emit_add_const(x, T0, T0, s->lo);
        } else {
            base = emit_load_base(x, base);
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
            emit_fast_tlb4k_probe(x, s->write, &bails);
            emit_fast_check_addend(x, grp_off, s->ptr_idx, s->first_for_ptr, T1, &bails);
            if (span > 1) {
                emit_add_const(x, T0, T0, (int64_t)(span - 1));
                emit_fast_tlb4k_probe(x, s->write, &bails);
                emit_fast_check_addend(x, grp_off, s->ptr_idx, false, T0, &bails);
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
        emit_fast_page_avail(x, MMU_SUPER_SHIFT, MMU_SUPER_SIZE, span, neg_stride);
        uint32_t *got_super = here(x);
        emit(x, enc_b(0));

        /* Fallback: 4 KiB data TLB (cpu->tlb) */
        patch_local(x, try_4k, here(x));
        emit_fast_tlb4k_probe(x, s->write, &bails);
        emit_fast_page_avail(x, APPINE_PAGE_SHIFT, APPINE_PAGE_SIZE, span, neg_stride);

        patch_local(x, got_super, here(x));
        emit_bail(x, &bails, enc_tbnz(neg_stride ? T0 : T1, 63));
        emit_fast_check_addend(x, grp_off, s->ptr_idx, s->first_for_ptr, T0, &bails);

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
            emit_bail(x, &bails, enc_cbnz_x(T2));
            emit(x, enc_ldr_x(T2, CTX, iters_off));
            emit(x, enc_sub_imm(T2, T2, 1, 0));
            emit(x, enc_mul(T2, T2, sr));
            emit(x, enc_sub_reg(T2, T1, T2)); /* avail - (safe_iters - 1) * sr */
            uint32_t *fits_all = here(x);
            emit(x, enc_tbz(T2, 63));
            emit(x, enc_udiv(T1, T1, sr));
            emit_bail(x, &bails, enc_cbz_x(T1));
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
            emit_bail(x, &bails, enc_cbz_x(T1));
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
            if (s->idx_opt == EXT_UXTW) { /* UXTW: room = 0xffffffff - (uint32_t)w */
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
                emit_bail(x, &bails, enc_tbnz(T0, 63));
            }
        } else {
            if (s->idx_opt == EXT_UXTW) { /* UXTW: room = (uint32_t)w */
                emit(x, enc_ubfx(T0, src, 0, 32));
            } else { /* SXTW: room = (int32_t)w + 0x80000000 */
                emit(x, enc_sbfx(T0, src, 0, 32));
                emit_mov_const(x, T1, UINT64_C(0x80000000));
                emit(x, enc_add_lsl(T0, T0, T1, 0));
            }
            if (s->idx_w32_pre_off < 0) {
                emit_add_const(x, T0, T0, (int64_t)s->idx_w32_pre_off);
                emit_bail(x, &bails, enc_tbnz(T0, 63));
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
        emit_div_step(x, T0, T2, abs_step);
        emit_bail(x, &bails, enc_cbz_x(T0));
        emit(x, enc_add_imm(T0, T0, 1, 0));
        emit(x, enc_str_x(T0, CTX, iters_off));
        patch_local(x, no_wrap, here(x));
    }

    /* 3c. Probe 4 KiB read TLB for each single-page M_LITERAL and save its host address */
    for (unsigned l = 0; l < fl->nlits; l++) {
        emit_mov_const(x, T0, fl->lit_addr[l]);
        emit_fast_tlb4k_probe(x, false, &bails);
        emit(x, enc_add_lsl(T2, T2, T0, 0));
        emit(x, enc_str_x(T2, CTX, grp_off + 8 * (size_t)(fl->nptrs + l)));
    }

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
        if (insn == INSN_NOP)
            continue;
        if (fl->header_tested && i == fl->hdr_br_idx)
            continue; /* header exit proved not-taken for all safe_iters iterations */
        if (fl->omit_ctrl && i == fl->ctrl_idx)
            continue; /* omitted CMP (re-emitted once in normal epilogue if bottom-tested) */
        if (next_se < fl->nside_exits && fl->side_exits[next_se].idx == i) {
            if (is_bcond(insn)) {
                se_sites[nse_sites++] = here(x);
                emit(x, insn & ~(0x7ffffu << 5));
            } else { /* CBZ / CBNZ / TBZ / TBNZ */
                bool is_tb = is_tbz_tbnz(insn);
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
        if (is_memop_top(top)) {
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
        } else if (is_adr_adrp(insn)) { /* ADR / ADRP */
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
    for (unsigned i = 0; i < bails.n; i++)
        patch_local(x, bails.sites[i], slow);
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
