//! Vector ALU lowering: 32-bit integer and float operations, compares,
//! source / output modifiers, SDWA and DPP (port of `recomp/emit.rs` `valu`).

use super::lower::{err, ext_encoding, Dst, LResult, Lw, Mods, VAR_VGPR};
use super::module::{BinOp, Pred, Value};
use super::ops_core::op;
use crate::gpu::rdna2::{self, Decoded, Encoding, Operand, Variant};

/// A source operand: one dword, or a mask pair (SGPR pair / VCC).
#[derive(Clone, Copy)]
pub enum Src {
    W(Value),
    Pair(Value, Value),
}

impl Src {
    pub fn w(self) -> Value {
        match self {
            Src::W(v) => v,
            Src::Pair(l, _) => l,
        }
    }
}

/// Returns the per-quad lane map `[p0, p1, p2, p3]` when a DPP16 `ctrl` stays strictly
/// inside each 4-lane quad (`quad_perm` `0x000..=0x0FF`, or `row_xmask` `0x160..=0x163`).
fn dpp16_quad_map(ctrl: u64) -> Option<[u8; 4]> {
    match ctrl {
        0x000..=0x0FF => Some([
            (ctrl & 3) as u8,
            ((ctrl >> 2) & 3) as u8,
            ((ctrl >> 4) & 3) as u8,
            ((ctrl >> 6) & 3) as u8,
        ]),
        0x160..=0x163 => {
            let n = (ctrl & 3) as u8;
            Some([n, 1 ^ n, 2 ^ n, 3 ^ n])
        }
        _ => None,
    }
}

/// Returns the per-quad lane map when a DPP8 selector maps lanes `0..4` into `0..4` and
/// applies the same relative map to lanes `4..8` into `4..8`.
fn dpp8_quad_map(sel: u64) -> Option<[u8; 4]> {
    let s: [u8; 8] = std::array::from_fn(|i| ((sel >> (3 * i)) & 7) as u8);
    if (0..4).all(|i| s[i] < 4 && s[i + 4] == s[i] + 4) {
        Some([s[0], s[1], s[2], s[3]])
    } else {
        None
    }
}

impl Lw {
    pub fn mods(d: &Decoded) -> Mods {
        let fv = |e: &Encoding, n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
        let bit = |e: &Encoding, n: &str| fv(e, n) == 1;
        match d.variant {
            Variant::Sdwa => match ext_encoding(d) {
                Some(e) => Mods {
                    abs: [bit(e, "SRC0_ABS"), bit(e, "SRC1_ABS"), false],
                    neg: [bit(e, "SRC0_NEG"), bit(e, "SRC1_NEG"), false],
                    omod: fv(e, "OMOD"),
                    clamp: bit(e, "CLAMP"),
                    opsel: 0,
                },
                None => Mods::default(),
            },
            Variant::Dpp16 => match ext_encoding(d) {
                Some(e) => Mods {
                    abs: [bit(e, "SRC0_ABS"), bit(e, "SRC1_ABS"), false],
                    neg: [bit(e, "SRC0_NEG"), bit(e, "SRC1_NEG"), false],
                    ..Default::default()
                },
                None => Mods::default(),
            },
            Variant::Dpp8 => Mods::default(),
            Variant::Plain => {
                let e = d.enc;
                let (abs, neg) = (fv(e, "ABS"), fv(e, "NEG"));
                Mods {
                    abs: [abs & 1 == 1, abs >> 1 & 1 == 1, abs >> 2 & 1 == 1],
                    neg: [neg & 1 == 1, neg >> 1 & 1 == 1, neg >> 2 & 1 == 1],
                    omod: fv(e, "OMOD"),
                    clamp: bit(e, "CLAMP"),
                    opsel: fv(e, "OP_SEL"),
                }
            }
        }
    }

    /// Float view of a dword with abs / neg of source `i`.
    pub fn fsrc(&mut self, m: &Mods, i: usize, u: Value) -> Value {
        let mut s = self.f(u);
        if m.abs[i] {
            s = self.fabs(s);
        }
        if m.neg[i] {
            s = self.fneg(s);
        }
        s
    }

    /// Float result with output modifiers, as dword bits.
    pub fn fres(&mut self, m: &Mods, e: Value) -> Value {
        let mut s = match m.omod {
            1 => {
                let k = self.fc(2.0);
                self.fmul(e, k)
            }
            2 => {
                let k = self.fc(4.0);
                self.fmul(e, k)
            }
            3 => {
                let k = self.fc(0.5);
                self.fmul(e, k)
            }
            _ => e,
        };
        if m.clamp {
            s = self.un(op::SATURATE, s);
        }
        self.bits(s)
    }

    /// DPP16 source lane (i32, −1 = none) of lane `lane`.
    fn dpp16_src(&mut self, ctrl: u64) -> Option<Value> {
        let n = (ctrl & 15) as u32;
        let l = self.lane;
        Some(match ctrl {
            0x000..=0x0FF => {
                let a = self.andc(l, !3);
                let q = self.andc(l, 3);
                let sh = self.shlc(q, 1);
                let k = self.c(ctrl as u32);
                let s = self.bin(BinOp::LShr, k, sh);
                let s = self.andc(s, 3);
                self.or(a, s)
            }
            0x101..=0x10F => {
                let r = self.andc(l, 15);
                let r = self.addc(r, n);
                let ok = self.cmpc(Pred::ULt, r, 16);
                let t = self.addc(l, n);
                let m1 = self.c(u32::MAX);
                self.sel(ok, t, m1)
            }
            0x111..=0x11F => {
                let r = self.andc(l, 15);
                let ok = self.cmpc(Pred::UGe, r, n);
                let k = self.c(n);
                let t = self.sub(l, k);
                let m1 = self.c(u32::MAX);
                self.sel(ok, t, m1)
            }
            0x121..=0x12F => {
                let a = self.andc(l, !15);
                let r = self.andc(l, 15);
                let k = self.c(n);
                let r = self.sub(r, k);
                let r = self.andc(r, 15);
                self.or(a, r)
            }
            0x140 => {
                let a = self.andc(l, !15);
                let r = self.andc(l, 15);
                let k = self.c(15);
                let r = self.sub(k, r);
                self.or(a, r)
            }
            0x141 => {
                let a = self.andc(l, !15);
                let b8 = self.andc(l, 8);
                let r = self.andc(l, 7);
                let k = self.c(7);
                let r = self.sub(k, r);
                let x = self.or(a, b8);
                self.or(x, r)
            }
            0x150..=0x15F => {
                let a = self.andc(l, !15);
                let k = self.c(n);
                self.or(a, k)
            }
            0x160..=0x16F => {
                let k = self.c(n);
                self.xor(l, k)
            }
            _ => return None,
        })
    }

    /// Source dwords in operand order; SGPR-pair masks as `Src::Pair`. SDWA
    /// selects / extends its two sources, DPP reads src0 from another lane.
    pub fn sources(&mut self, d: &Decoded, ins: &[&Operand], src_f16: bool) -> LResult<Vec<Src>> {
        let mut u = Vec::new();
        for o in ins {
            if d.variant != Variant::Plain && o.field == Some("SRC0") {
                u.push(Src::W(self.c(0)));
                continue;
            }
            match o.ty {
                "OPR_SREG" | "OPR_VCC" if o.size == 64 => {
                    // wave32 lane masks (`v_cndmask_b32`, `v_*_co_ci_u32`, `v_div_fmas_f32`)
                    // read one 32-bit scalar source (`m0`, `exec_hi`, `f32` inline constants),
                    // never a 64-bit SGPR pair or `vcc_hi` / `exec_hi`
                    let (l, h) = if self.wave64 {
                        self.read64(d, o, false)
                    } else {
                        self.read(d, o, false).map(|l| (l, self.c(0)))
                    }
                    .ok_or_else(|| format!("{}: op", rdna2::format(d)))?;
                    u.push(Src::Pair(l, h));
                }
                _ if o.size == 64 => return err(d, "operand 64-bit"),
                _ => u.push(Src::W(self.read(d, o, src_f16).ok_or_else(|| format!("{}: op", rdna2::format(d)))?)),
            }
        }
        match d.variant {
            Variant::Plain => {}
            Variant::Sdwa => {
                let e = ext_encoding(d).ok_or_else(|| format!("{}: SDWA", rdna2::format(d)))?;
                let fv = |n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
                let s0 = if fv("S0") == 1 { self.code32(fv("VSRC0"), None, src_f16).ok_or("op")? } else { self.vreg(fv("VSRC0")) };
                if !u.is_empty() {
                    u[0] = Src::W(self.sdwa_sel(s0, fv("SRC0_SEL") as u32, fv("SRC0_SEXT") == 1));
                }
                if e.field("VSRC1").is_some() && u.len() > 1 {
                    let v1 = fv("VSRC1");
                    let s1 = if fv("S1") == 1 { self.code32(v1, None, src_f16).ok_or("op")? } else { self.vreg(v1) };
                    u[1] = Src::W(self.sdwa_sel(s1, fv("SRC1_SEL") as u32, fv("SRC1_SEXT") == 1));
                }
            }
            Variant::Dpp16 | Variant::Dpp8 => {
                let e = ext_encoding(d).ok_or_else(|| format!("{}: DPP", rdna2::format(d)))?;
                let fv = |n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
                let v = self.vreg(fv("VSRC0"));
                let (el, eh) = self.exec_mask();
                let t;
                if d.variant == Variant::Dpp16 {
                    let ctrl = fv("DPP_CTRL");
                    let (fi, bound) = (fv("FI") == 1, fv("BOUND_CTRL") == 1);
                    let l = self.dpp16_src(ctrl).ok_or_else(|| format!("{}: DPP_CTRL", rdna2::format(d)))?;
                    let z = self.c(0);
                    let valid = self.cmp(Pred::SGe, l, z);
                    let lc = self.imax(l, z);
                    let on = self.lanebit_at(el, eh, lc);
                    let fib = self.b(fi);
                    let ok0 = self.lor(fib, on);
                    let ok = self.land(valid, ok0);
                    let r = self.read_lane_half(v, lc);
                    let (rm, bm) = (fv("ROW_MASK") as u32, fv("BANK_MASK") as u32);
                    let lane = self.lane;
                    let row = self.lshrc(lane, 4);
                    let k = self.c(rm);
                    let rb = self.bin(BinOp::LShr, k, row);
                    let rb = self.andc(rb, 1);
                    let rok = self.nz(rb);
                    let bank = self.lshrc(lane, 2);
                    let bank = self.andc(bank, 3);
                    let k = self.c(bm);
                    let bb = self.bin(BinOp::LShr, k, bank);
                    let bb = self.andc(bb, 1);
                    let bok = self.nz(bb);
                    let mut en = self.land(rok, bok);
                    if bound {
                        t = self.sel(ok, r, z);
                    } else {
                        en = self.land(en, ok);
                        t = r;
                    }
                    self.dpp_enable = Some(en);
                } else {
                    let sel: u64 = (0..8).map(|i| fv(&format!("LANE_SEL_{i}")) << (3 * i)).sum();
                    let fi = rdna2::field_value(d.enc, &d.words, "SRC0").unwrap_or(0) == 0xEA;
                    let lane = self.lane;
                    let a = self.andc(lane, !7);
                    let q = self.andc(lane, 7);
                    let k3 = self.c(3);
                    let sh = self.mul(q, k3);
                    let k = self.c(sel as u32);
                    let s = self.bin(BinOp::LShr, k, sh);
                    let s = self.andc(s, 7);
                    let l = self.or(a, s);
                    let r = self.read_lane_half(v, l);
                    t = if !fi {
                        let on = self.lanebit_at(el, eh, l);
                        let z = self.c(0);
                        self.sel(on, r, z)
                    } else {
                        r
                    };
                }
                if !u.is_empty() {
                    u[0] = Src::W(t);
                }
            }
        }
        Ok(u)
    }

    pub fn sdwa_sel(&mut self, x: Value, sel: u32, sext: bool) -> Value {
        if sel >= 6 {
            return x;
        }
        let (sh, w) = if sel < 4 { (8 * sel, 8) } else { (16 * (sel - 4), 16) };
        let v = self.lshrc(x, sh);
        let v = self.andc(v, (1u32 << w) - 1);
        if sext {
            let s = self.shlc(v, 32 - w);
            self.ashrc(s, 32 - w)
        } else {
            v
        }
    }

    fn sdwa_dst(&mut self, r: Value, old: Value, sel: u32, unused: u32) -> Value {
        if sel >= 6 {
            return r;
        }
        let (sh, w) = if sel < 4 { (8 * sel, 8u32) } else { (16 * (sel - 4), 16u32) };
        let m = (((1u64 << w) - 1) as u32) << sh;
        let s = self.shlc(r, sh);
        let v = self.andc(s, m);
        match unused {
            2 => {
                let o = self.andc(old, !m);
                self.or(o, v)
            }
            1 => {
                let top = sh + w;
                if top >= 32 {
                    return v;
                }
                let b = self.lshrc(r, w - 1);
                let b = self.andc(b, 1);
                let neg = self.nz(b);
                let z = self.c(0);
                let ext = self.c(u32::MAX << top);
                let e = self.sel(neg, ext, z);
                self.or(v, e)
            }
            _ => v,
        }
    }

    /// Writes a VALU result, honouring SDWA dst_sel / dst_unused and DPP write enables.
    pub fn vwrite(&mut self, d: &Decoded, dst: &Dst, val: Value) {
        let mut val = val;
        if d.variant == Variant::Sdwa {
            if let (Some(e), Dst::V(n)) = (ext_encoding(d), dst) {
                let fv = |k: &str| rdna2::field_value(e, &d.words, k).unwrap_or(0);
                if e.field("DST_SEL").is_some() {
                    let cur = self.vreg(*n);
                    val = self.sdwa_dst(val, cur, fv("DST_SEL") as u32, fv("DST_UNUSED") as u32);
                }
            }
        }
        match (self.dpp_enable.take(), dst) {
            (Some(en), Dst::V(n)) => {
                let old = self.vreg(*n);
                let a = self.act();
                let c = self.land(a, en);
                let v = self.sel(c, val, old);
                self.wr(VAR_VGPR + *n as u32, v);
            }
            _ => self.write32(dst, val),
        }
    }

    pub fn valu(&mut self, d: &Decoded, name: &str) -> LResult<()> {
        if self.f32_rm && super::lower_round::f32_round_unsupported(name) {
            return err(d, "directed f32 rounding of this instruction");
        }
        if name.starts_with("v_interp_") {
            return self.interp_gfx(d, name);
        }
        let (outs, ins) = Self::ops(d);
        const WIDE16: &[&str] = &[
            "v_cvt_pknorm_i16_f32", "v_cvt_pknorm_u16_f32", "v_cvt_pknorm_i16_f16", "v_cvt_pknorm_u16_f16", "v_cvt_pk_u16_u32",
            "v_cvt_pk_i16_i32", "v_mad_u32_u16", "v_mad_i32_i16", "v_sad_u16", "v_sat_pk_u8_i16",
        ];
        let f16 = (name.contains("f16") || name.contains("_i16") || name.contains("_u16") || name.contains("_b16")) && !WIDE16.contains(&name);
        let cmp = name.starts_with("v_cmp");
        let src_f16 = matches!(name, "v_cvt_f32_f16" | "v_pack_b32_f16" | "v_cvt_pknorm_i16_f16" | "v_cvt_pknorm_u16_f16") || (cmp && name.ends_with("_f16"));
        let mix = name.starts_with("v_fma_mix");
        if name.starts_with("v_pk_") || name.starts_with("v_dot") {
            return self.valu_pk(d, name);
        }
        if matches!(name, "v_qsad_pk_u16_u8" | "v_mqsad_pk_u16_u8" | "v_mqsad_u32_u8") {
            return self.valu_qsad(d, name);
        }
        if name.starts_with("v_movrel") {
            return self.movrel(d, name);
        }
        if name == "v_swaprel_b32" {
            return self.swaprel(d);
        }
        if name == "v_swap_b32" && d.variant == Variant::Plain {
            let s0 = Self::fv(d, "SRC0");
            if s0 < 256 {
                return err(d, "v_swap_b32: SRC0 is not a VGPR");
            }
            let (xn, yn) = (Self::fv(d, "VDST"), s0 - 256);
            let (x, y) = (self.vreg(xn), self.vreg(yn));
            let a = self.act();
            let nx = self.sel(a, y, x);
            let ny = self.sel(a, x, y);
            self.wr(VAR_VGPR + xn as u32, nx);
            self.wr(VAR_VGPR + yn as u32, ny);
            return Ok(());
        }
        if d.enc.name == "ENC_VOP3P" && !mix {
            return err(d, "VOP3P");
        }
        if d.variant != Variant::Plain && ext_encoding(d).is_none() {
            return err(d, "SDWA/DPP in this encoding");
        }
        self.dpp_enable = None;
        if name.contains("f64") || name.contains("_i64") || name.contains("_u64") || name.contains("_b64") {
            return self.valu64(d, name);
        }
        if f16 && !mix && !cmp && !matches!(name, "v_cvt_f32_f16" | "v_cvt_f16_f32" | "v_cvt_pkrtz_f16_f32" | "v_pack_b32_f16") {
            return self.valu16(d, name);
        }
        let m = Self::mods(d);
        let u = self.sources(d, &ins, src_f16)?;
        if let Some(rest) = name.strip_prefix("v_cmpx_").or_else(|| name.strip_prefix("v_cmp_")) {
            return self.vopc(d, rest, name.starts_with("v_cmpx_"), &ins, &outs, &u, &m);
        }
        let dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::Null);
        let z = self.c(0);
        let cur = match &dst {
            Dst::V(n) => self.vreg(*n),
            _ => z,
        };
        let a = u.first().map(|s| s.w()).unwrap_or(z);
        let b = u.get(1).map(|s| s.w()).unwrap_or(z);
        let c = u.get(2).map(|s| s.w()).unwrap_or(z);
        let wmask = if self.wave64 { 63 } else { 31 };
        macro_rules! fs {
            ($i:expr) => {{
                let x = u.get($i).map(|s| s.w()).unwrap_or(z);
                self.fsrc(&m, $i, x)
            }};
        }
        macro_rules! fr {
            ($e:expr) => {{
                let e = $e;
                self.fres(&m, e)
            }};
        }
        if self.f32_rm {
            if let Some(e) = self.valu_f32_rm(name, &m, &u, cur)? {
                let res = self.fres(&m, e);
                self.vwrite(d, &dst, res);
                return Ok(());
            }
        }
        let res: Value = match name {
            "v_mov_b32" | "v_mov_fed_b32" => a,
            "v_not_b32" => self.not(a),
            "v_bfrev_b32" => self.reversebits(a),
            "v_ffbh_u32" => self.lzcnt(a),
            "v_ffbl_b32" => self.firstbitlow(a),
            "v_ffbh_i32" => self.ffbh_i(a),
            "v_cvt_f32_i32" => fr!(self.sitof(a)),
            "v_cvt_f32_u32" => fr!(self.uitof(a)),
            "v_cvt_u32_f32" => {
                let x = fs!(0);
                self.cvt_u32(x)
            }
            "v_cvt_i32_f32" => {
                let x = fs!(0);
                self.cvt_i32(x)
            }
            "v_cvt_rpi_i32_f32" => {
                let x = fs!(0);
                let h = self.fc(0.5);
                let x = self.fadd(x, h);
                let x = self.un(op::ROUND_NI, x);
                self.cvt_i32(x)
            }
            "v_cvt_flr_i32_f32" => {
                let x = fs!(0);
                let x = self.un(op::ROUND_NI, x);
                self.cvt_i32(x)
            }
            "v_cvt_f32_ubyte0" | "v_cvt_f32_ubyte1" | "v_cvt_f32_ubyte2" | "v_cvt_f32_ubyte3" => {
                let k = (name.as_bytes()[name.len() - 1] - b'0') as u32;
                let x = self.lshrc(a, 8 * k);
                let x = self.andc(x, 0xFF);
                fr!(self.uitof(x))
            }
            "v_cvt_f32_f16" => {
                let h = if m.opsel & 1 == 1 { self.lshrc(a, 16) } else { self.andc(a, 0xFFFF) };
                fr!(self.s.f16_to_f32(h))
            }
            "v_cvt_f16_f32" => {
                let x = fs!(0);
                // output modifier and clamp in f32, before the conversion
                let x = if m.omod != 0 || m.clamp {
                    let b = self.fres(&m, x);
                    self.f(b)
                } else {
                    x
                };
                let h = self.to_f16_rounded(x);
                if m.opsel >> 3 & 1 == 1 {
                    let l = self.andc(cur, 0xFFFF);
                    let hh = self.shlc(h, 16);
                    self.or(l, hh)
                } else if d.variant == Variant::Sdwa {
                    h
                } else {
                    let hi = self.andc(cur, 0xFFFF_0000);
                    self.or(hi, h)
                }
            }
            "v_cvt_pkrtz_f16_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                let l = self.f16_rtz(x);
                let h = self.f16_rtz(y);
                let h = self.shlc(h, 16);
                self.or(l, h)
            }
            "v_pack_b32_f16" => {
                let lo = if m.opsel & 1 == 1 { self.lshrc(a, 16) } else { self.andc(a, 0xFFFF) };
                let hi = if m.opsel & 2 == 2 { self.lshrc(b, 16) } else { self.andc(b, 0xFFFF) };
                let hi = self.shlc(hi, 16);
                self.or(lo, hi)
            }
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                let fv = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
                let (sel, hi2) = (fv("OP_SEL"), fv("OP_SEL_HI") | fv("OP_SEL_HI_2") << 2);
                let (neg, abs) = (fv("NEG"), fv("NEG_HI"));
                let mut srcs = Vec::new();
                for i in 0..3 {
                    let raw = u.get(i).map(|s| s.w()).unwrap_or(z);
                    let mut s = if hi2 >> i & 1 == 1 {
                        let h = if sel >> i & 1 == 1 { self.lshrc(raw, 16) } else { self.andc(raw, 0xFFFF) };
                        self.s.f16_to_f32(h)
                    } else {
                        self.f(raw)
                    };
                    if abs >> i & 1 == 1 {
                        s = self.fabs(s);
                    }
                    if neg >> i & 1 == 1 {
                        s = self.fneg(s);
                    }
                    srcs.push(s);
                }
                let mut e = self.fmad(srcs[0], srcs[1], srcs[2]);
                if fv("CLAMP") == 1 {
                    e = self.un(op::SATURATE, e);
                }
                match name {
                    "v_fma_mix_f32" => self.bits(e),
                    "v_fma_mixlo_f16" => {
                        let h = self.to_f16(e);
                        let hi = self.andc(cur, 0xFFFF_0000);
                        self.or(hi, h)
                    }
                    _ => {
                        let h = self.to_f16(e);
                        let h = self.shlc(h, 16);
                        let lo = self.andc(cur, 0xFFFF);
                        self.or(lo, h)
                    }
                }
            }
            "v_rcp_f32" | "v_rcp_iflag_f32" => {
                let x = fs!(0);
                let one = self.fc(1.0);
                fr!(self.fdiv(one, x))
            }
            "v_rsq_f32" => fr!({
                let x = fs!(0);
                self.un(op::RSQRT, x)
            }),
            "v_sqrt_f32" => fr!({
                let x = fs!(0);
                self.un(op::SQRT, x)
            }),
            "v_exp_f32" => fr!({
                let x = fs!(0);
                self.un(op::EXP, x)
            }),
            "v_log_f32" => fr!({
                let x = fs!(0);
                self.un(op::LOG, x)
            }),
            "v_sin_f32" | "v_cos_f32" => {
                let x = fs!(0);
                let k = self.fc(std::f32::consts::TAU);
                let x = self.fmul(x, k);
                fr!(self.un(if name == "v_sin_f32" { op::SIN } else { op::COS }, x))
            }
            "v_fract_f32" => {
                let x = fs!(0);
                fr!(self.fract_f(x))
            }
            "v_trunc_f32" => fr!({
                let x = fs!(0);
                self.un(op::ROUND_Z, x)
            }),
            "v_floor_f32" => fr!({
                let x = fs!(0);
                self.un(op::ROUND_NI, x)
            }),
            "v_ceil_f32" => fr!({
                let x = fs!(0);
                self.un(op::ROUND_PI, x)
            }),
            "v_rndne_f32" => fr!({
                let x = fs!(0);
                self.un(op::ROUND_NE, x)
            }),
            "v_frexp_mant_f32" => {
                let x = fs!(0);
                fr!(self.frexp_mant(x))
            }
            "v_frexp_exp_i32_f32" => self.frexp_exp_f32(a, |lw| lw.fsrc(&m, 0, a)),
            "v_ldexp_f32" => {
                let x = fs!(0);
                fr!(self.ldexp_f(x, b))
            }
            "v_add_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.fadd(x, y))
            }
            "v_sub_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.fsub(x, y))
            }
            "v_subrev_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.fsub(y, x))
            }
            "v_mul_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.fmul(x, y))
            }
            "v_mul_legacy_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.mul_legacy(x, y))
            }
            "v_min_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.fmin(x, y))
            }
            "v_max_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                fr!(self.fmax(x, y))
            }
            "v_fmac_f32" | "v_mac_f32" | "v_fmac_legacy_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                let cf = self.f(cur);
                fr!(match name {
                    "v_fmac_f32" => self.fmad(x, y, cf),
                    "v_mac_f32" => self.mad_nf(x, y, cf),
                    _ => self.fma_legacy(x, y, cf),
                })
            }
            "v_fma_f32" | "v_mad_f32" | "v_fma_legacy_f32" | "v_div_fmas_f32" => {
                let (x, y, w) = (fs!(0), fs!(1), fs!(2));
                fr!(match name {
                    "v_fma_f32" | "v_div_fmas_f32" => self.fmad(x, y, w),
                    "v_mad_f32" => self.mad_nf(x, y, w),
                    _ => self.fma_legacy(x, y, w),
                })
            }
            "v_fmamk_f32" | "v_madmk_f32" => {
                let (x, w) = (fs!(0), fs!(2));
                let k = self.f(b);
                fr!(if name == "v_fmamk_f32" { self.fmad(x, k, w) } else { self.mad_nf(x, k, w) })
            }
            "v_fmaak_f32" | "v_madak_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                let k = self.f(c);
                fr!(if name == "v_fmaak_f32" { self.fmad(x, y, k) } else { self.mad_nf(x, y, k) })
            }
            "v_min3_f32" => {
                let (x, y, w) = (fs!(0), fs!(1), fs!(2));
                let t = self.fmin(x, y);
                fr!(self.fmin(t, w))
            }
            "v_max3_f32" => {
                let (x, y, w) = (fs!(0), fs!(1), fs!(2));
                let t = self.fmax(x, y);
                fr!(self.fmax(t, w))
            }
            "v_med3_f32" => {
                let (x, y, w) = (fs!(0), fs!(1), fs!(2));
                fr!(self.med3f(x, y, w))
            }
            "v_cubeid_f32" | "v_cubesc_f32" | "v_cubetc_f32" | "v_cubema_f32" => {
                let (x, y, w) = (fs!(0), fs!(1), fs!(2));
                fr!(self.cube(name, x, y, w))
            }
            "v_div_fixup_f32" => {
                let (q, dd, nn) = (fs!(0), fs!(1), fs!(2));
                fr!(self.div_fixup(q, dd, nn))
            }
            // clamp: unsigned saturation
            "v_add_nc_u32" | "v_add3_u32" if m.clamp => {
                let t = self.add(a, b);
                let over = self.cmp(Pred::ULt, t, a);
                let (t, over) = if name == "v_add3_u32" {
                    let u = self.add(t, c);
                    let o2 = self.cmp(Pred::ULt, u, t);
                    (u, self.lor(over, o2))
                } else {
                    (t, over)
                };
                let m1 = self.c(u32::MAX);
                self.sel(over, m1, t)
            }
            "v_sub_nc_u32" | "v_subrev_nc_u32" if m.clamp => {
                let (x, y) = if name == "v_sub_nc_u32" { (a, b) } else { (b, a) };
                let under = self.cmp(Pred::ULt, x, y);
                let t = self.sub(x, y);
                let z = self.c(0);
                self.sel(under, z, t)
            }
            "v_add_nc_u32" => self.add(a, b),
            "v_sub_nc_u32" => self.sub(a, b),
            "v_subrev_nc_u32" => self.sub(b, a),
            "v_mul_lo_u32" => self.mul(a, b),
            "v_mul_hi_u32" => self.mul_hi(a, b, false),
            "v_mul_hi_i32" => self.mul_hi(a, b, true),
            "v_mul_u32_u24" => {
                let (x, y) = (self.andc(a, 0xFF_FFFF), self.andc(b, 0xFF_FFFF));
                self.mul(x, y)
            }
            "v_mul_hi_u32_u24" => {
                let (x, y) = (self.andc(a, 0xFF_FFFF), self.andc(b, 0xFF_FFFF));
                self.mul_hi(x, y, false)
            }
            "v_mul_i32_i24" => {
                let (x, y) = (self.i24(a), self.i24(b));
                self.mul(x, y)
            }
            "v_mul_hi_i32_i24" => {
                let (x, y) = (self.i24(a), self.i24(b));
                self.mul_hi(x, y, true)
            }
            "v_mad_u32_u24" => {
                let (x, y) = (self.andc(a, 0xFF_FFFF), self.andc(b, 0xFF_FFFF));
                let p = self.mul(x, y);
                self.add(p, c)
            }
            "v_mad_i32_i24" => {
                let (x, y) = (self.i24(a), self.i24(b));
                let p = self.mul(x, y);
                self.add(p, c)
            }
            "v_and_b32" => self.and(a, b),
            "v_or_b32" => self.or(a, b),
            "v_xor_b32" => self.xor(a, b),
            "v_xnor_b32" => {
                let x = self.xor(a, b);
                self.not(x)
            }
            "v_lshlrev_b32" => self.shl(b, a),
            "v_lshrrev_b32" => self.lshr(b, a),
            "v_ashrrev_i32" => self.ashr(b, a),
            "v_lshl_add_u32" => {
                let x = self.shl(a, b);
                self.add(x, c)
            }
            "v_add_lshl_u32" => {
                let x = self.add(a, b);
                self.shl(x, c)
            }
            "v_lshl_or_b32" => {
                let x = self.shl(a, b);
                self.or(x, c)
            }
            "v_and_or_b32" => {
                let x = self.and(a, b);
                self.or(x, c)
            }
            "v_or3_b32" => {
                let x = self.or(a, b);
                self.or(x, c)
            }
            "v_xor3_b32" => {
                let x = self.xor(a, b);
                self.xor(x, c)
            }
            "v_add3_u32" => {
                let x = self.add(a, b);
                self.add(x, c)
            }
            "v_xad_u32" => {
                let x = self.xor(a, b);
                self.add(x, c)
            }
            "v_bfe_u32" => self.bfe_u(a, b, c),
            "v_bfe_i32" => self.bfe_i(a, b, c),
            "v_bfi_b32" => {
                let x = self.and(a, b);
                let n = self.not(a);
                let y = self.and(n, c);
                self.or(x, y)
            }
            "v_bfm_b32" => self.bfm(a, b),
            "v_bcnt_u32_b32" => {
                let n = self.countbits(a);
                self.add(n, b)
            }
            "v_perm_b32" => self.perm_b32(a, b, c),
            "v_alignbit_b32" => self.alignbit(a, b, c),
            "v_alignbyte_b32" => {
                let s = self.andc(c, 3);
                let s = self.shlc(s, 3);
                self.alignbit(a, b, s)
            }
            "v_min_u32" => self.umin(a, b),
            "v_max_u32" => self.umax(a, b),
            "v_min_i32" => self.imin(a, b),
            "v_max_i32" => self.imax(a, b),
            "v_min3_u32" => {
                let t = self.umin(a, b);
                self.umin(t, c)
            }
            "v_max3_u32" => {
                let t = self.umax(a, b);
                self.umax(t, c)
            }
            "v_med3_u32" => {
                let (x, y) = (self.umin(a, b), self.umax(a, b));
                let y = self.umin(y, c);
                self.umax(x, y)
            }
            "v_min3_i32" => {
                let t = self.imin(a, b);
                self.imin(t, c)
            }
            "v_max3_i32" => {
                let t = self.imax(a, b);
                self.imax(t, c)
            }
            "v_med3_i32" => {
                let (x, y) = (self.imin(a, b), self.imax(a, b));
                let y = self.imin(y, c);
                self.imax(x, y)
            }
            "v_sad_u32" => {
                let (x, y) = (self.umax(a, b), self.umin(a, b));
                let t = self.sub(x, y);
                self.add(t, c)
            }
            "v_sad_u8" | "v_sad_hi_u8" => {
                let s = self.sad_u8(a, b);
                let s = if name == "v_sad_hi_u8" { self.shlc(s, 16) } else { s };
                self.add(s, c)
            }
            "v_sad_u16" => {
                let x = self.absd(a, b, 0, 0xFFFF);
                let y = self.absd(a, b, 16, 0xFFFF);
                let t = self.add(x, y);
                self.add(t, c)
            }
            "v_msad_u8" => {
                let s = self.msad_u8(a, b);
                self.add(s, c)
            }
            "v_lerp_u8" => self.lerp_u8(a, b, c),
            "v_add_nc_i32" | "v_sub_nc_i32" if m.clamp => {
                let (x, y) = (self.sext64(a), self.sext64(b));
                let r = if name == "v_add_nc_i32" { self.add(x, y) } else { self.sub(x, y) };
                self.sat_i32(r)
            }
            "v_add_nc_i32" => self.add(a, b),
            "v_sub_nc_i32" => self.sub(a, b),
            "v_mad_u32_u16" => {
                let x = self.lshrc(a, 16 * (m.opsel & 1) as u32);
                let x = self.andc(x, 0xFFFF);
                let y = self.lshrc(b, 8 * (m.opsel & 2) as u32);
                let y = self.andc(y, 0xFFFF);
                let p = self.mul(x, y);
                self.add(p, c)
            }
            "v_mad_i32_i16" => {
                let x = self.lshrc(a, 16 * (m.opsel & 1) as u32);
                let x = self.shlc(x, 16);
                let x = self.ashrc(x, 16);
                let y = self.lshrc(b, 8 * (m.opsel & 2) as u32);
                let y = self.shlc(y, 16);
                let y = self.ashrc(y, 16);
                let p = self.mul(x, y);
                self.add(p, c)
            }
            "v_cvt_pk_u16_u32" => {
                let k = self.c(0xFFFF);
                let x = self.umin(a, k);
                let y = self.umin(b, k);
                let y = self.shlc(y, 16);
                self.or(x, y)
            }
            "v_cvt_pk_i16_i32" => {
                let (lo, hi) = (self.c(-32768i32 as u32), self.c(32767));
                let x = self.imax(a, lo);
                let x = self.imin(x, hi);
                let x = self.andc(x, 0xFFFF);
                let y = self.imax(b, lo);
                let y = self.imin(y, hi);
                let y = self.andc(y, 0xFFFF);
                let y = self.shlc(y, 16);
                self.or(x, y)
            }
            "v_cvt_pknorm_i16_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                let l = self.cvt_norm_i16_f32(x);
                let h = self.cvt_norm_i16_f32(y);
                let h = self.shlc(h, 16);
                self.or(l, h)
            }
            "v_cvt_pknorm_u16_f32" => {
                let (x, y) = (fs!(0), fs!(1));
                let l = self.cvt_norm_u16_f32(x);
                let h = self.cvt_norm_u16_f32(y);
                let h = self.shlc(h, 16);
                self.or(l, h)
            }
            "v_cvt_pknorm_i16_f16" | "v_cvt_pknorm_u16_f16" => {
                let mut hb = [z, z];
                for (i, slot) in hb.iter_mut().enumerate() {
                    let s = u.get(i).map(|s| s.w()).unwrap_or(z);
                    let mut s = if m.opsel >> i & 1 == 1 { self.lshrc(s, 16) } else { self.andc(s, 0xFFFF) };
                    if m.abs[i] {
                        s = self.andc(s, 0x7FFF);
                    }
                    if m.neg[i] {
                        let k = self.c(0x8000);
                        s = self.xor(s, k);
                    }
                    *slot = s;
                }
                let (l, h) = if name == "v_cvt_pknorm_i16_f16" {
                    (self.cvt_norm_i16_f16(hb[0]), self.cvt_norm_i16_f16(hb[1]))
                } else {
                    (self.cvt_norm_u16_f16(hb[0]), self.cvt_norm_u16_f16(hb[1]))
                };
                let h = self.shlc(h, 16);
                self.or(l, h)
            }
            "v_sat_pk_u8_i16" => {
                let (lo, hi) = (self.c(0), self.c(255));
                let x = self.shlc(a, 16);
                let x = self.ashrc(x, 16);
                let x = self.imax(x, lo);
                let x = self.imin(x, hi);
                let y = self.ashrc(a, 16);
                let y = self.imax(y, lo);
                let y = self.imin(y, hi);
                let y = self.shlc(y, 8);
                self.or(x, y)
            }
            "v_cvt_pk_u8_f32" => {
                let x = fs!(0);
                let r = self.un(op::ROUND_NE, x);
                let (lo, hi) = (self.fc(0.0), self.fc(255.0));
                let r = self.fmax(r, lo);
                let r = self.fmin(r, hi);
                let v = self.ftoui(r);
                let sel = self.andc(b, 3);
                let sh = self.shlc(sel, 3);
                let k = self.c(0xFF);
                let msk = self.shl(k, sh);
                let nm = self.not(msk);
                let keep = self.and(c, nm);
                let v = self.shl(v, sh);
                self.or(keep, v)
            }
            "v_mbcnt_lo_u32_b32" => {
                let m = self.lanemask_lt(false);
                let x = self.and(a, m);
                let n = self.countbits(x);
                self.add(n, b)
            }
            "v_mbcnt_hi_u32_b32" => {
                let m = self.lanemask_lt(true);
                let x = self.and(a, m);
                let n = self.countbits(x);
                self.add(n, b)
            }
            "v_cndmask_b32" => {
                let Some(Src::Pair(l, h)) = u.get(2).copied() else { return err(d, "mask") };
                let bit = self.lane_of(l, h);
                self.sel(bit, b, a)
            }
            "v_permlane16_b32" | "v_permlanex16_b32" => {
                let (fi, bound) = (m.opsel & 1 == 1, m.opsel & 2 == 2);
                let lane = self.lane;
                let r = self.andc(lane, 15);
                let lo8 = self.cmpc(Pred::ULt, r, 8);
                let w = self.sel(lo8, b, c);
                let q = self.andc(lane, 7);
                let sh = self.shlc(q, 2);
                let nsel = self.bin(BinOp::LShr, w, sh);
                let nsel = self.andc(nsel, 15);
                let base = self.andc(lane, !31);
                let row = self.andc(lane, 16);
                let row = if name == "v_permlanex16_b32" {
                    let k = self.c(16);
                    let x = self.xor(row, k);
                    self.andc(x, 16)
                } else {
                    row
                };
                let l = self.or(base, row);
                let l = self.or(l, nsel);
                let (el, eh) = self.exec_mask();
                let on = self.lanebit_at(el, eh, l);
                let fib = self.b(fi);
                let ok = self.lor(fib, on);
                let v = self.read_lane_half(a, l);
                if bound {
                    self.sel(ok, v, z)
                } else {
                    self.sel(ok, v, cur)
                }
            }
            "v_readfirstlane_b32" => {
                let (el, eh) = self.exec_mask();
                let l = self.first_lane(el, eh);
                let t = self.read_lane_uniform(a, l);
                self.write32(&dst, t);
                return Ok(());
            }
            "v_readlane_b32" => {
                let l = self.andc(b, wmask);
                let t = self.read_lane_uniform(a, l);
                self.write32(&dst, t);
                return Ok(());
            }
            "v_writelane_b32" => {
                if let Dst::V(n) = dst {
                    let l = self.andc(b, wmask);
                    let lane = self.lane;
                    let me = self.cmp(Pred::Eq, lane, l);
                    let v = self.sel(me, a, cur);
                    self.wr(VAR_VGPR + n as u32, v);
                }
                return Ok(());
            }
            "v_div_scale_f32" => {
                let sdst = outs.get(1).map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
                let f = self.b(false);
                self.write_mask(&sdst, f);
                let x = fs!(0);
                let r = self.bits(x);
                self.vwrite(d, &dst, r);
                return Ok(());
            }
            "v_add_co_u32" | "v_sub_co_u32" | "v_subrev_co_u32" => {
                let (x, y) = if name == "v_subrev_co_u32" { (b, a) } else { (a, b) };
                let (t, carry) = if name == "v_add_co_u32" {
                    let t = self.add(x, y);
                    (t, self.cmp(Pred::ULt, t, x))
                } else {
                    let t = self.sub(x, y);
                    (t, self.cmp(Pred::UGt, y, x))
                };
                let sdst = outs.get(1).map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
                self.write_mask(&sdst, carry);
                self.vwrite(d, &dst, t);
                return Ok(());
            }
            "v_add_co_ci_u32" | "v_sub_co_ci_u32" | "v_subrev_co_ci_u32" => {
                let Some(Src::Pair(l, h)) = u.get(2).copied() else { return err(d, "mask") };
                let cb = self.lane_of(l, h);
                let cin = self.u(cb);
                let (x, y) = if name == "v_subrev_co_ci_u32" { (b, a) } else { (a, b) };
                let (x6, y6, c6) = (self.zext64(x), self.zext64(y), self.zext64(cin));
                let w = if name == "v_add_co_ci_u32" {
                    let s = self.add(x6, y6);
                    self.add(s, c6)
                } else {
                    let s = self.sub(x6, y6);
                    self.sub(s, c6)
                };
                let (t, hi) = self.unpack64(w);
                let carry = self.nz(hi);
                let sdst = outs.get(1).map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
                self.write_mask(&sdst, carry);
                self.vwrite(d, &dst, t);
                return Ok(());
            }
            _ => return err(d, "VALU"),
        };
        self.vwrite(d, &dst, res);
        Ok(())
    }

    /// Source bits with abs / neg applied on the bits (no float operation: denormals stay).
    fn fsrc_bits(&mut self, m: &Mods, i: usize, u: Value) -> Value {
        let mut s = u;
        if m.abs[i] {
            s = self.andc(s, 0x7FFF_FFFF);
        }
        if m.neg[i] {
            let k = self.c(0x8000_0000);
            s = self.xor(s, k);
        }
        s
    }

    /// The f32 instructions that round by MODE[1:0] in a program directing f32 rounding
    /// (`lower_round`): the result as a float before output modifiers, None for the rest.
    fn valu_f32_rm(&mut self, name: &str, m: &Mods, u: &[Src], cur: Value) -> LResult<Option<Value>> {
        let z = self.c(0);
        let raw = |i: usize| u.get(i).map(|s| s.w()).unwrap_or(z);
        let (a, b, c) = (raw(0), raw(1), raw(2));
        let ba = self.fsrc_bits(m, 0, a);
        let bb = self.fsrc_bits(m, 1, b);
        let bc = self.fsrc_bits(m, 2, c);
        let (fa, fb, fc) = (self.f(ba), self.f(bb), self.f(bc));
        let legacy_zero = |lw: &mut Lw, x: Value, y: Value| {
            let zz = lw.fc(0.0);
            let xz = lw.fcmp(Pred::FOEq, x, zz);
            let yz = lw.fcmp(Pred::FOEq, y, zz);
            lw.lor(xz, yz)
        };
        Ok(Some(match name {
            "v_add_f32" => {
                let n = self.fadd(fa, fb);
                self.rm_add(ba, bb, n)
            }
            "v_sub_f32" | "v_subrev_f32" => {
                let (x, y) = if name == "v_sub_f32" { (ba, bb) } else { (bb, ba) };
                let k = self.c(0x8000_0000);
                let ny = self.xor(y, k);
                let (fx, fy) = (self.f(x), self.f(y));
                let n = self.fsub(fx, fy);
                self.rm_add(x, ny, n)
            }
            "v_mul_f32" => {
                let n = self.fmul(fa, fb);
                self.rm_mul(ba, bb, n)
            }
            "v_mul_legacy_f32" => {
                let n = self.fmul(fa, fb);
                let r = self.rm_mul(ba, bb, n);
                let lz = legacy_zero(self, fa, fb);
                let zz = self.fc(0.0);
                self.sel(lz, zz, r)
            }
            "v_fma_f32" | "v_div_fmas_f32" => {
                let n = self.fmad(fa, fb, fc);
                self.rm_fma(ba, bb, bc, n)
            }
            "v_fma_legacy_f32" => {
                let n = self.fmad(fa, fb, fc);
                let r = self.rm_fma(ba, bb, bc, n);
                let lz = legacy_zero(self, fa, fb);
                self.sel(lz, fc, r)
            }
            "v_fmac_f32" | "v_fmac_legacy_f32" => {
                let fcur = self.f(cur);
                let n = self.fmad(fa, fb, fcur);
                let r = self.rm_fma(ba, bb, cur, n);
                if name == "v_fmac_legacy_f32" {
                    let lz = legacy_zero(self, fa, fb);
                    self.sel(lz, fcur, r)
                } else {
                    r
                }
            }
            "v_fmamk_f32" => {
                let fk = self.f(b);
                let n = self.fmad(fa, fk, fc);
                self.rm_fma(ba, b, bc, n)
            }
            "v_fmaak_f32" => {
                let fk = self.f(c);
                let n = self.fmad(fa, fb, fk);
                self.rm_fma(ba, bb, c, n)
            }
            "v_mad_f32" => {
                let n = self.mad_nf(fa, fb, fc);
                self.rm_mad(ba, bb, bc, n)
            }
            "v_mac_f32" => {
                let fcur = self.f(cur);
                let n = self.mad_nf(fa, fb, fcur);
                self.rm_mad(ba, bb, cur, n)
            }
            "v_madmk_f32" => {
                let fk = self.f(b);
                let n = self.mad_nf(fa, fk, fc);
                self.rm_mad(ba, b, bc, n)
            }
            "v_madak_f32" => {
                let fk = self.f(c);
                let n = self.mad_nf(fa, fb, fk);
                self.rm_mad(ba, bb, c, n)
            }
            "v_cvt_f32_i32" | "v_cvt_f32_u32" => {
                let signed = name == "v_cvt_f32_i32";
                let n = if signed { self.sitof(a) } else { self.uitof(a) };
                self.rm_cvt_int(a, signed, n)
            }
            "v_sqrt_f32" => {
                let n = self.un(op::SQRT, fa);
                self.rm_sqrt(ba, n)
            }
            "v_ldexp_f32" => {
                let n = self.ldexp_f(fa, b);
                self.rm_ldexp(ba, b, n)
            }
            _ => return Ok(None),
        }))
    }

    pub fn vopc(&mut self, d: &Decoded, rest: &str, cmpx: bool, ins: &[&Operand], outs: &[&Operand], u: &[Src], m: &Mods) -> LResult<()> {
        let (opn, ty) = rest.rsplit_once('_').ok_or_else(|| format!("{}: VOPC", rdna2::format(d)))?;
        if u.len() < 2 {
            return err(d, "VOPC");
        }
        let (mut ua, mut ub) = (u[0].w(), u[1].w());
        let half = |s: &mut Self, i: u64, v: Value| if m.opsel >> i & 1 == 1 { s.lshrc(v, 16) } else { s.andc(v, 0xFFFF) };
        let ty = match ty {
            "f16" => {
                if opn == "class" {
                    let mut h = half(self, 0, ua);
                    if m.abs[0] {
                        h = self.andc(h, 0x7FFF);
                    }
                    if m.neg[0] {
                        let k = self.c(0x8000);
                        h = self.xor(h, k);
                    }
                    let cls = self.fclass16(h);
                    let s = self.bin(BinOp::LShr, ub, cls);
                    let s = self.andc(s, 1);
                    let cond = self.nz(s);
                    return self.vopc_write(d, cmpx, outs, cond);
                }
                let ha = half(self, 0, ua);
                let fa = self.s.f16_to_f32(ha);
                ua = self.bits(fa);
                let hb = half(self, 1, ub);
                let fb = self.s.f16_to_f32(hb);
                ub = self.bits(fb);
                "f32"
            }
            "u16" => {
                ua = half(self, 0, ua);
                ub = half(self, 1, ub);
                "u32"
            }
            "i16" => {
                let x = half(self, 0, ua);
                let x = self.shlc(x, 16);
                ua = self.ashrc(x, 16);
                let y = half(self, 1, ub);
                let y = self.shlc(y, 16);
                ub = self.ashrc(y, 16);
                "i32"
            }
            t => t,
        };
        // Pixel shaders (#134): quad-read the compare's VGPR operands rather than its
        // predicate result (AMD RDNA2/3/4 amdxc64 crashes on quadOp of values derived
        // from WaveReadLaneAt, such as v_readfirstlane waterfall loops).
        if self.gfx.as_ref().is_some_and(|g| g.ps.is_some())
            && d.variant == Variant::Plain
            && self.guard_override.is_none()
            && opn != "f"
        {
            let dst = self.vopc_dst(d, cmpx, outs);
            if matches!(dst, Dst::Null) && matches!(ty, "f32" | "i32" | "u32") {
                return Ok(());
            }
            let is_vgpr = |o: &Operand| match o.field {
                Some("LITERAL") | None => false,
                Some(f) => {
                    (o.ty.starts_with("OPR_VGPR") && d.enc.field_width(f) <= 8)
                        || (!matches!(o.ty, "OPR_SIMM16" | "OPR_SIMM32") && Self::field(d, f) >= 256)
                }
            };
            let (a_vgpr, b_vgpr) = if matches!(opn, "t" | "tru") {
                (false, false)
            } else {
                (ins.first().is_some_and(|o| is_vgpr(o)), ins.get(1).is_some_and(|o| is_vgpr(o)))
            };
            let (el, eh) = self.exec_mask();
            let qbase = self.andc(self.lane, !3);
            let mut q = self.c(0);
            for p in 0..4u32 {
                let kp = self.c(p);
                let qa = if a_vgpr { self.s.quad_read_lane_at(ua, kp) } else { ua };
                let qb = if b_vgpr {
                    if ub == ua && a_vgpr {
                        qa
                    } else {
                        self.s.quad_read_lane_at(ub, kp)
                    }
                } else {
                    ub
                };
                let cp = self.vopc_cond(d, opn, ty, qa, qb, m)?;
                let lp = self.or(qbase, kp);
                let ap = self.lanebit_at(el, eh, lp);
                let bp = self.land(ap, cp);
                let up = self.u(bp);
                let sh = self.shlc(up, p);
                q = self.or(q, sh);
            }
            let (lo, hi) = self.ballot_ps_quad(q);
            if self.wave64 {
                self.write64(&dst, lo, hi);
            } else {
                self.write32(&dst, lo);
            }
            return Ok(());
        }
        let cond = self.vopc_cond(d, opn, ty, ua, ub, m)?;
        self.vopc_write(d, cmpx, outs, cond)
    }

    fn vopc_cond(&mut self, d: &Decoded, opn: &str, ty: &str, ua: Value, ub: Value, m: &Mods) -> LResult<Value> {
        Ok(match ty {
            "f32" => {
                let a = self.fsrc(m, 0, ua);
                if opn == "class" {
                    if let Some(k) = self.s.m.const_u64(ub) {
                        self.fclass_const(a, (k & 0x3FF) as u32)
                    } else {
                        let cls = self.fclass(a);
                        let s = self.bin(BinOp::LShr, ub, cls);
                        let s = self.andc(s, 1);
                        self.nz(s)
                    }
                } else {
                    let b = self.fsrc(m, 1, ub);
                    self.float_compare(opn, a, b).ok_or_else(|| format!("{}: VOPC f32", rdna2::format(d)))?
                }
            }
            "i32" | "u32" => {
                let s = ty == "i32";
                match opn {
                    "f" => self.b(false),
                    "t" | "tru" => self.b(true),
                    "lt" => self.cmp(if s { Pred::SLt } else { Pred::ULt }, ua, ub),
                    "eq" => self.cmp(Pred::Eq, ua, ub),
                    "le" => self.cmp(if s { Pred::SLe } else { Pred::ULe }, ua, ub),
                    "gt" => self.cmp(if s { Pred::SGt } else { Pred::UGt }, ua, ub),
                    "ne" | "lg" => self.cmp(Pred::Ne, ua, ub),
                    "ge" => self.cmp(if s { Pred::SGe } else { Pred::UGe }, ua, ub),
                    _ => return err(d, "VOPC int"),
                }
            }
            _ => return err(d, "VOPC type"),
        })
    }

    /// The recompiler's `float_compare`: NaN tested explicitly.
    pub fn float_compare(&mut self, opn: &str, a: Value, b: Value) -> Option<Value> {
        let rel = |s: &mut Self, r: &str| -> Option<Value> {
            Some(match r {
                "lt" => s.fcmp(Pred::FOLt, a, b),
                "eq" => s.fcmp(Pred::FOEq, a, b),
                "le" => s.fcmp(Pred::FOLe, a, b),
                "gt" => s.fcmp(Pred::FOGt, a, b),
                "ge" => s.fcmp(Pred::FOGe, a, b),
                "lg" => {
                    let x = s.fcmp(Pred::FOLt, a, b);
                    let y = s.fcmp(Pred::FOGt, a, b);
                    s.lor(x, y)
                }
                _ => return None,
            })
        };
        let nan = |s: &mut Self| {
            let x = s.isnan_any(a);
            let y = s.isnan_any(b);
            s.lor(x, y)
        };
        Some(match opn {
            "f" => self.b(false),
            "t" | "tru" => self.b(true),
            "o" => {
                let n = nan(self);
                self.lnot(n)
            }
            "u" => nan(self),
            _ => match opn.strip_prefix('n').filter(|r| matches!(*r, "lt" | "eq" | "le" | "gt" | "ge" | "lg")) {
                Some(r) => {
                    let n = nan(self);
                    let x = rel(self, r)?;
                    let nx = self.lnot(x);
                    self.lor(n, nx)
                }
                None => {
                    let n = nan(self);
                    let nn = self.lnot(n);
                    let x = rel(self, opn)?;
                    self.land(nn, x)
                }
            },
        })
    }

    /// IsNaN of a float or double (doubles from their bits: DXC narrows `isnan`).
    pub fn isnan_any(&mut self, x: Value) -> Value {
        let t = self.s.m.type_of(x);
        if t == self.s.m.f64() {
            self.isnan_d(x)
        } else {
            self.isnan(x)
        }
    }

    fn vopc_dst(&self, d: &Decoded, cmpx: bool, outs: &[&Operand]) -> Dst {
        let mut dst = outs.first().map(|o| Self::dst_of(d, o)).unwrap_or(Dst::S(106));
        if d.variant == Variant::Sdwa {
            if let Some(e) = ext_encoding(d) {
                if rdna2::field_value(e, &d.words, "SD") == Some(1) {
                    let sd = rdna2::field_value(e, &d.words, "SDST").unwrap_or(106);
                    dst = if sd == 125 { Dst::Null } else { Dst::S(sd) };
                }
            }
        }
        if cmpx {
            dst = Dst::S(126);
        }
        dst
    }

    pub fn vopc_write(&mut self, d: &Decoded, cmpx: bool, outs: &[&Operand], cond: Value) -> LResult<()> {
        let dst = self.vopc_dst(d, cmpx, outs);
        self.write_mask(&dst, cond);
        Ok(())
    }

    /// lanemask_lt_lo / _hi words for this lane.
    pub fn lanemask_lt(&mut self, hi: bool) -> Value {
        let l = self.lane;
        let all = self.c(u32::MAX);
        if !hi {
            // lane >= 32 ? ~0 : (lane == 0 ? 0 : ~0 >> (32 - lane))
            let k = self.c(32);
            let s = self.sub(k, l);
            let s = self.andc(s, 31);
            let m = self.bin(BinOp::LShr, all, s);
            let z = self.cmpc(Pred::Eq, l, 0);
            let m = self.selc(z, 0, m);
            let up = self.cmpc(Pred::UGe, l, 32);
            self.sel(up, all, m)
        } else {
            // lane <= 32 ? 0 : ~0 >> (64 - lane)
            let k = self.c(64);
            let s = self.sub(k, l);
            let s = self.andc(s, 31);
            let m = self.bin(BinOp::LShr, all, s);
            let le = self.cmpc(Pred::ULe, l, 32);
            self.selc(le, 0, m)
        }
    }
}

// LINUX-TODO Phase D: gated windows-only like the other dxil translation test
// modules — `validate`/`runner` are Windows-only until the SPIR-V backend lands.
#[cfg(all(test, windows))]
mod wave32_mask_src_tests {
    use std::collections::BTreeSet;

    use super::Lw;
    use crate::gpu::dxil::bitreader_tests;
    use crate::gpu::dxil::kernel::{self, Entry, KernelOptions};
    use crate::gpu::dxil::validate;
    use crate::gpu::rdna2;
    use crate::gpu::recomp::{self, runner};

    const BLOCK_CONSTANTS: u32 = 11;
    const BLOCK_FUNCTION: u32 = 12;
    const CST_CODE_INTEGER: u32 = 4;
    const FUNC_CODE_INST_CALL: u32 = 34;

    fn asm(list: &[(&str, &str, &[(&str, u64)])]) -> Vec<u8> {
        let mut code = Vec::new();
        for &(name, enc, fields) in list {
            code.extend(rdna2::encode(name, enc, fields, None).unwrap_or_else(|e| panic!("encode {name}/{enc}: {e}")));
        }
        code.extend(rdna2::encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    fn touched_for(code: &[u8], wave64: bool) -> BTreeSet<u32> {
        let insts = [rdna2::decode(code, 0).expect("decode mask consumer")];
        let leaders: BTreeSet<u64> = [0].into_iter().collect();
        let ko = KernelOptions { wave64, split: false, mem_windows: 1, sampler_base: 0 };
        let mut lw = Lw::new(&insts, &leaders, ko, &Entry::Kernel);
        lw.setup_lanes();
        lw.init.vgpr_base = Some(lw.c(0));
        lw.init.touched.clear();
        lw.inst(&insts[0]).expect("lower single mask consumer");
        lw.init.touched
    }

    fn dxil_bitcode(container: &[u8]) -> &[u8] {
        let n = u32::from_le_bytes(container[28..32].try_into().unwrap()) as usize;
        for i in 0..n {
            let off = u32::from_le_bytes(container[32 + 4 * i..36 + 4 * i].try_into().unwrap()) as usize;
            if &container[off..off + 4] == b"DXIL" {
                let len = u32::from_le_bytes(container[off + 4..off + 8].try_into().unwrap()) as usize;
                let bc = &container[off + 8 + 24..off + 8 + len];
                assert_eq!(&bc[..4], b"BC\xC0\xDE");
                return bc;
            }
        }
        panic!("missing DXIL part");
    }

    fn call_count(container: &[u8]) -> usize {
        let root = bitreader_tests::parse(dxil_bitcode(container));
        root.find(BLOCK_FUNCTION).expect("function block").records_with(FUNC_CODE_INST_CALL).len()
    }

    fn has_i32_const(container: &[u8], val: u32) -> bool {
        let root = bitreader_tests::parse(dxil_bitcode(container));
        let enc = (val as u64) << 1;
        root.find(BLOCK_CONSTANTS)
            .expect("constants block")
            .records_with(CST_CODE_INTEGER)
            .iter()
            .any(|r| **r == [enc])
    }

    /// Wave32 VALU lane-mask sources (`v_cndmask_b32`, `v_add_co_ci_u32`, `v_sub_co_ci_u32`,
    /// `v_subrev_co_ci_u32`) must accept 32-bit-only scalar codes `m0` (124) and `exec_hi` (127)
    /// across kernel and compute Direct DXIL entrypoints, while Wave64 still rejects both.
    #[test]
    fn direct_wave32_valu_mask_32bit_only_sources_translate() {
        let ko32 = KernelOptions { wave64: false, split: false, mem_windows: 1, sampler_base: 0 };
        let ko64 = KernelOptions { wave64: true, split: false, mem_windows: 1, sampler_base: 0 };
        let cs = recomp::CsInterface { threads: [32, 1, 1], user_sgprs: 4, tgid: [false; 3], tg_size: false, tid_components: 1, ..Default::default() };

        for src2 in [124u64, 127] {
            let ops: [(&str, &str, &[(&str, u64)]); 4] = [
                ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", src2)]),
                ("V_ADD_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 2), ("SDST", 0), ("SRC0", 256), ("SRC1", 257), ("SRC2", src2)]),
                ("V_SUB_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 2), ("SDST", 0), ("SRC0", 256), ("SRC1", 257), ("SRC2", src2)]),
                ("V_SUBREV_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 2), ("SDST", 0), ("SRC0", 256), ("SRC1", 257), ("SRC2", src2)]),
            ];
            for (name, enc, fields) in ops {
                let code = asm(&[(name, enc, fields)]);
                let dxil = kernel::translate(&code, ko32).unwrap_or_else(|e| panic!("wave32 {name} src2={src2}: {e}"));
                assert!(bitreader_tests::parse(dxil_bitcode(&dxil)).find(BLOCK_FUNCTION).is_some());
                if validate::validator_supported() {
                    validate::validate(&dxil).unwrap_or_else(|e| panic!("validate wave32 {name} src2={src2}: {e}"));
                }
                assert!(kernel::translate(&code, ko64).is_err(), "wave64 {name} src2={src2} must reject non-pair code");

                let cs_code = asm(&[
                    ("S_MOV_B32", "ENC_SOP1", &[("SDST", src2), ("SSRC0", 0)]),
                    (name, enc, fields),
                    ("BUFFER_STORE_DWORD", "ENC_MUBUF", &[("VDATA", 2), ("SRSRC", 0), ("SOFFSET", 128)]),
                ]);
                let comp = kernel::translate_compute(&cs_code, ko32, &cs)
                    .unwrap_or_else(|e| panic!("compute wave32 {name} src2={src2}: {e}"));
                if validate::validator_supported() {
                    validate::validate(&comp.dxil).unwrap_or_else(|e| panic!("validate compute {name} src2={src2}: {e}"));
                }
            }
        }
    }

    /// Wave32 `OPR_VCC` and `OPR_SREG` mask consumers read only the low 32-bit register (`vcc_lo`
    /// or `src2`), never touching or loading `vcc_hi` (107), `exec_hi` (127), `s[n+1]`, or bogus
    /// `s108`, whereas Wave64 still reads both halves of the 64-bit mask pair.
    #[test]
    fn direct_wave32_valu_mask_sources_skip_hi_sgpr_touches_and_loads() {
        let ko32 = KernelOptions { wave64: false, split: false, mem_windows: 1, sampler_base: 0 };
        let ko64 = KernelOptions { wave64: true, split: false, mem_windows: 1, sampler_base: 0 };

        let vcc_ops: &[(&str, &str, &[(&str, u64)])] = &[
            ("V_CNDMASK_B32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 256), ("VSRC1", 1)]),
            (
                "V_CNDMASK_B32",
                "VOP2_VOP_SDWA",
                &[("VDST", 2), ("SRC0", 0xF9), ("VSRC0", 0), ("VSRC1", 1), ("SRC0_SEL", 6), ("SRC1_SEL", 6), ("DST_SEL", 6)],
            ),
            (
                "V_CNDMASK_B32",
                "VOP2_VOP_DPP16",
                &[
                    ("VDST", 2),
                    ("SRC0", 0xFA),
                    ("VSRC0", 0),
                    ("VSRC1", 1),
                    ("DPP_CTRL", 0x141),
                    ("ROW_MASK", 0xF),
                    ("BANK_MASK", 0xF),
                    ("BOUND_CTRL", 1),
                ],
            ),
            ("V_ADD_CO_CI_U32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 256), ("VSRC1", 1)]),
            ("V_SUB_CO_CI_U32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 256), ("VSRC1", 1)]),
            ("V_SUBREV_CO_CI_U32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 256), ("VSRC1", 1)]),
            ("V_DIV_FMAS_F32", "ENC_VOP3", &[("VDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", 258)]),
        ];
        for &(name, enc, fields) in vcc_ops {
            let code = asm(&[(name, enc, fields)]);
            let touched = touched_for(&code, false);
            assert!(touched.contains(&106), "{name}/{enc} wave32 must read vcc_lo: {touched:?}");
            assert!(!touched.contains(&107), "{name}/{enc} wave32 must not touch vcc_hi (s107): {touched:?}");
            assert!(!touched.contains(&127), "{name}/{enc} wave32 must not touch exec_hi (s127): {touched:?}");
        }

        for src2 in [10u64, 105, 106, 107, 126] {
            for (name, enc, fields) in [
                ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 2u64), ("SRC0", 256), ("SRC1", 257), ("SRC2", src2)][..]),
                ("V_ADD_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 2), ("SDST", 0), ("SRC0", 256), ("SRC1", 257), ("SRC2", src2)][..]),
            ] {
                let code = asm(&[(name, enc, fields)]);
                let touched = touched_for(&code, false);
                let lo = src2 as u32;
                assert!(touched.contains(&lo), "{name} src2={src2} wave32 must touch s{lo}: {touched:?}");
                assert!(!touched.contains(&(lo + 1)), "{name} src2={src2} wave32 must not touch s{}: {touched:?}", lo + 1);
            }
        }

        // Control: Wave64 still reads both halves (`vcc_lo` + `vcc_hi`, and `s10` + `s11`).
        let vop2_cnd = asm(&[("V_CNDMASK_B32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 256), ("VSRC1", 1)])]);
        let t64_vcc = touched_for(&vop2_cnd, true);
        assert!(t64_vcc.contains(&106) && t64_vcc.contains(&107), "wave64 ENC_VOP2 must touch vcc_lo+vcc_hi: {t64_vcc:?}");
        let vop3_s10 = asm(&[("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", 10)])]);
        let t64_s10 = touched_for(&vop3_s10, true);
        assert!(t64_s10.contains(&10) && t64_s10.contains(&11), "wave64 ENC_VOP3 s10 must touch s10+s11: {t64_s10:?}");

        // Bitcode check on shipping `translate_compute`: with `user_sgprs = 12` and `SRC2 = s10`,
        // Wave32 emits a `rawBufferLoad` at `s10`'s `io` byte offset (`40`) and never `s11`'s
        // (`44`), whereas Wave64 loads both `s10` (`40`) and `s11` (`44`).
        let cs32 = recomp::CsInterface { threads: [32, 1, 1], user_sgprs: 12, tgid: [false; 3], tg_size: false, tid_components: 2, ..Default::default() };
        let cs64 = recomp::CsInterface { threads: [64, 1, 1], user_sgprs: 12, tgid: [false; 3], tg_size: false, tid_components: 2, ..Default::default() };
        let cs_s10 = asm(&[
            ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", 10)]),
            ("BUFFER_STORE_DWORD", "ENC_MUBUF", &[("VDATA", 2), ("SRSRC", 0), ("SOFFSET", 128)]),
        ]);
        let comp32 = kernel::translate_compute(&cs_s10, ko32, &cs32).expect("compute wave32 s10").dxil;
        let comp64 = kernel::translate_compute(&cs_s10, ko64, &cs64).expect("compute wave64 s10").dxil;
        assert!(has_i32_const(&comp32, 10 * 4), "compute wave32 must load s10 at io offset 40");
        assert!(!has_i32_const(&comp32, 11 * 4), "compute wave32 must not load s11 at io offset 44");
        assert!(has_i32_const(&comp64, 11 * 4), "compute wave64 must load s11 at io offset 44");
        assert_eq!(call_count(&comp32) + 1, call_count(&comp64), "compute wave32 skips bogus s11 rawBufferLoad");
    }

    /// Wave32 inline float constants (`0.5` = 240, `inv_2pi` = 248) on scalar mask operands emit
    /// their 32-bit IEEE-754 `f32` bit patterns rather than `code64`'s `f64` low dword.
    #[test]
    fn direct_wave32_valu_mask_inline_float_consts_use_f32_bits() {
        let ko32 = KernelOptions { wave64: false, split: false, mem_windows: 1, sampler_base: 0 };

        let f05 = asm(&[("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", 240)])]);
        let f05_dxil = kernel::translate(&f05, ko32).expect("wave32 inline 0.5 mask");
        assert!(has_i32_const(&f05_dxil, 0.5f32.to_bits()), "wave32 inline 0.5 mask must emit 0x3F000000");

        let inv2pi = asm(&[("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", 248)])]);
        let inv2pi_dxil = kernel::translate(&inv2pi, ko32).expect("wave32 inline inv_2pi mask");
        assert!(has_i32_const(&inv2pi_dxil, 0x3E22_F983), "wave32 inline inv_2pi mask must emit f32 0x3E22F983");
        assert!(!has_i32_const(&inv2pi_dxil, 0x6DC9_C882), "wave32 inline inv_2pi mask must not emit f64 low bits");
    }

    /// Wave32 Direct DXIL GPU execution of `v_cndmask_b32` (with `VCC`, `m0`, `exec_hi`, `s10`,
    /// and inline `0.5`) and `v_add_co_ci_u32` / `v_sub_co_ci_u32` / `v_subrev_co_ci_u32` (with
    /// `m0`), verifying lane results and confirming adjacent high SGPRs (`s11`, `vcc_hi`, `s3`,
    /// `s5`, `s7`) remain untouched.
    #[test]
    fn gpu_direct_wave32_valu_mask_sources() {
        match runner::wave32_supported() {
            Ok(true) => {}
            Ok(false) => return runner::skip_test("no wave32"),
            Err(e) => return runner::skip_test(&e),
        }
        let code = asm(&[
            ("V_CNDMASK_B32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 256), ("VSRC1", 1)]),
            ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 3), ("SRC0", 256), ("SRC1", 257), ("SRC2", 124)]),
            ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 4), ("SRC0", 256), ("SRC1", 257), ("SRC2", 127)]),
            ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 5), ("SRC0", 256), ("SRC1", 257), ("SRC2", 10)]),
            ("V_CNDMASK_B32", "ENC_VOP3", &[("VDST", 6), ("SRC0", 256), ("SRC1", 257), ("SRC2", 240)]),
            ("V_ADD_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 7), ("SDST", 2), ("SRC0", 256), ("SRC1", 257), ("SRC2", 124)]),
            ("V_SUB_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 8), ("SDST", 4), ("SRC0", 256), ("SRC1", 257), ("SRC2", 124)]),
            ("V_SUBREV_CO_CI_U32", "VOP3_SDST_ENC", &[("VDST", 9), ("SDST", 6), ("SRC0", 256), ("SRC1", 257), ("SRC2", 124)]),
        ]);
        let ko = KernelOptions { wave64: false, split: false, mem_windows: 1, sampler_base: 0 };
        let dxil = kernel::translate(&code, ko).expect("direct wave32 translate");
        let mut st = runner::KernelState::default();
        let (vcc_mask, m0_mask, exechi_mask, s10_mask, f05_mask) =
            (0xA5A5_5A5Au32, 0x0F0F_F0F0u32, 0x33CC_AA55u32, 0xFFFF_0000u32, 0.5f32.to_bits());
        st.sgpr[106] = vcc_mask;
        st.sgpr[107] = 0xCAFE_BABE;
        st.sgpr[124] = m0_mask;
        st.sgpr[126] = u32::MAX;
        st.sgpr[127] = exechi_mask;
        st.sgpr[10] = s10_mask;
        st.sgpr[11] = 0xDEAD_BEEF;
        st.sgpr[3] = 0x1111_2222;
        st.sgpr[5] = 0x3333_4444;
        st.sgpr[7] = 0x5555_6666;
        for l in 0..32usize {
            st.vgpr[l][0] = if l % 2 == 0 { u32::MAX } else { 100 + l as u32 };
            st.vgpr[l][1] = (l as u32) + 1;
        }
        let r = runner::run(&dxil, &st, &[]).expect("GPU run");
        let (mut want_add_c, mut want_sub_b, mut want_subrev_b) = (0u32, 0u32, 0u32);
        for l in 0..32usize {
            let (a, b) = (st.vgpr[l][0], st.vgpr[l][1]);
            let bit = |m: u32| (m >> l) & 1 == 1;
            assert_eq!(r.vgpr[l][2], if bit(vcc_mask) { b } else { a }, "lane {l}: vcc cndmask");
            assert_eq!(r.vgpr[l][3], if bit(m0_mask) { b } else { a }, "lane {l}: m0 cndmask");
            assert_eq!(r.vgpr[l][4], if bit(exechi_mask) { b } else { a }, "lane {l}: exec_hi cndmask");
            assert_eq!(r.vgpr[l][5], if bit(s10_mask) { b } else { a }, "lane {l}: s10 cndmask");
            assert_eq!(r.vgpr[l][6], if bit(f05_mask) { b } else { a }, "lane {l}: 0.5 cndmask");

            let c = bit(m0_mask) as u64;
            let add64 = a as u64 + b as u64 + c;
            assert_eq!(r.vgpr[l][7], add64 as u32, "lane {l}: add_co_ci");
            want_add_c |= (((add64 >> 32) != 0) as u32) << l;

            let sub64 = (a as u64).wrapping_sub(b as u64).wrapping_sub(c);
            assert_eq!(r.vgpr[l][8], sub64 as u32, "lane {l}: sub_co_ci");
            want_sub_b |= (((sub64 >> 32) != 0) as u32) << l;

            let subrev64 = (b as u64).wrapping_sub(a as u64).wrapping_sub(c);
            assert_eq!(r.vgpr[l][9], subrev64 as u32, "lane {l}: subrev_co_ci");
            want_subrev_b |= (((subrev64 >> 32) != 0) as u32) << l;
        }
        assert_eq!(r.sgpr[2], want_add_c, "add_co_ci carry-out");
        assert_eq!(r.sgpr[4], want_sub_b, "sub_co_ci borrow-out");
        assert_eq!(r.sgpr[6], want_subrev_b, "subrev_co_ci borrow-out");
        assert_eq!(r.sgpr[3], 0x1111_2222, "s3 must remain untouched");
        assert_eq!(r.sgpr[5], 0x3333_4444, "s5 must remain untouched");
        assert_eq!(r.sgpr[7], 0x5555_6666, "s7 must remain untouched");
        assert_eq!(r.sgpr[11], 0xDEAD_BEEF, "s11 must remain untouched");
        assert_eq!(r.sgpr[107], 0xCAFE_BABE, "vcc_hi must remain untouched");
    }
}
