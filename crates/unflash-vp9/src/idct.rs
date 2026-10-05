//! Inverse transforms (8.7) and reconstruction (8.6.2): the dequantised
//! coefficients of one transform block, rows then columns, added to the
//! prediction.
//!
//! The 1-D transforms are generic over `Lane`: 8-bit streams compute in
//! `i32` (the specification's ranges guarantee it suffices for conforming
//! streams; arithmetic wraps rather than panicking on corrupt ones), 10 and
//! 12-bit streams in `i64`.

use crate::frame::Pixel;

pub const DCT_DCT: u8 = 0;
/// ADST vertically (columns), DCT horizontally (rows).
pub const ADST_DCT: u8 = 1;
/// DCT vertically, ADST horizontally.
pub const DCT_ADST: u8 = 2;
pub const ADST_ADST: u8 = 3;

/// The arithmetic of the transforms, on one value or several side by side.
pub trait Lane: Copy {
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    fn neg(self) -> Self;
    fn mul(self, c: i32) -> Self;
    /// Round2(x, 14): after a multiplication by a 14-bit cosine.
    fn r14(self) -> Self;
}

/// One value: `i32` for 8-bit streams, `i64` for deeper ones.
pub trait Scalar: Lane {
    fn from_i32(v: i32) -> Self;
    /// Round2(x, shift) as a residual.
    fn round_to_i32(self, shift: u32) -> i32;
    /// A row transform's output, kept in 32 bits between the passes (as
    /// libvpx keeps it).
    fn to_i32(self) -> i32;
}

impl Lane for i32 {
    #[inline(always)]
    fn add(self, o: Self) -> Self {
        self.wrapping_add(o)
    }
    #[inline(always)]
    fn sub(self, o: Self) -> Self {
        self.wrapping_sub(o)
    }
    #[inline(always)]
    fn neg(self) -> Self {
        self.wrapping_neg()
    }
    #[inline(always)]
    fn mul(self, c: i32) -> Self {
        self.wrapping_mul(c)
    }
    #[inline(always)]
    fn r14(self) -> Self {
        self.wrapping_add(1 << 13) >> 14
    }
}

impl Scalar for i32 {
    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v
    }
    #[inline(always)]
    fn round_to_i32(self, shift: u32) -> i32 {
        self.wrapping_add(1 << (shift - 1)) >> shift
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self
    }
}

impl Lane for i64 {
    #[inline(always)]
    fn add(self, o: Self) -> Self {
        self.wrapping_add(o)
    }
    #[inline(always)]
    fn sub(self, o: Self) -> Self {
        self.wrapping_sub(o)
    }
    #[inline(always)]
    fn neg(self) -> Self {
        self.wrapping_neg()
    }
    #[inline(always)]
    fn mul(self, c: i32) -> Self {
        self.wrapping_mul(c as i64)
    }
    #[inline(always)]
    fn r14(self) -> Self {
        self.wrapping_add(1 << 13) >> 14
    }
}

impl Scalar for i64 {
    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v as i64
    }
    #[inline(always)]
    fn round_to_i32(self, shift: u32) -> i32 {
        (self.wrapping_add(1 << (shift - 1)) >> shift) as i32
    }
    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }
}

/// The 4-point inverse ADST (8.7.1.6).
#[inline(always)]
pub fn iadst4<L: Lane>(t: &mut [L; 4]) {
    let [a, b, c, d] = *t;
    let s0 = a.mul(5283);
    let s1 = a.mul(9929);
    let s2 = b.mul(13377);
    let s3 = c.mul(15212);
    let s4 = c.mul(5283);
    let s5 = d.mul(9929);
    let s6 = d.mul(15212);
    let s7 = a.sub(c).add(d).mul(13377);
    let x0 = s0.add(s3).add(s5);
    let x1 = s1.sub(s4).sub(s6);
    *t = [x0.add(s2).r14(), x1.add(s2).r14(), s7.r14(), x0.add(x1).sub(s2).r14()];
}

/// The inverse Walsh-Hadamard transform of lossless blocks (8.7.1.10).
#[inline(always)]
fn iwht4(t: &mut [i32; 4], shift: u32) {
    let mut a = t[0] >> shift;
    let mut c = t[1] >> shift;
    let mut d = t[2] >> shift;
    let mut b = t[3] >> shift;
    a = a.wrapping_add(c);
    d = d.wrapping_sub(b);
    let e = a.wrapping_sub(d) >> 1;
    b = e.wrapping_sub(b);
    c = e.wrapping_sub(c);
    a = a.wrapping_sub(b);
    d = d.wrapping_add(c);
    *t = [a, b, c, d];
}

// The 1-D inverse transforms of 8.7.1, unrolled from the specification's
// butterfly networks (generated; the permutations are resolved into the
// variable names). A DCT passes its even-numbered inputs through the DCT
// of half its size, as the network does, and numbers the rest as in the
// whole network. `L` is one value, or several processed side by side.

/// The 4-point inverse DCT.
#[inline(always)]
pub fn idct4<L: Lane>(t: &mut [L; 4]) {
    let [i0, i1, i2, i3] = *t;
    let x1 = i0.sub(i2).mul(11585).r14();
    let x2 = i0.add(i2).mul(11585).r14();
    let x3 = i1.mul(6270).sub(i3.mul(15137)).r14();
    let x4 = i1.mul(15137).add(i3.mul(6270)).r14();
    let x5 = x2.add(x4);
    let x6 = x2.sub(x4);
    let x7 = x1.add(x3);
    let x8 = x1.sub(x3);
    *t = [x5, x7, x8, x6];
}

/// The 8-point inverse DCT.
#[inline(always)]
pub fn idct8<L: Lane>(t: &mut [L; 8]) {
    let [i0, i1, i2, i3, i4, i5, i6, i7] = *t;
    let mut even = [i0, i2, i4, i6];
    idct4(&mut even);
    let [x5, x7, x8, x6] = even;
    let x9 = i1.mul(3196).sub(i7.mul(16069)).r14();
    let x10 = i1.mul(16069).add(i7.mul(3196)).r14();
    let x11 = i5.mul(13623).sub(i3.mul(9102)).r14();
    let x12 = i5.mul(9102).add(i3.mul(13623)).r14();
    let x13 = x9.add(x11);
    let x14 = x9.sub(x11);
    let x15 = x10.add(x12);
    let x16 = x10.sub(x12);
    let x17 = x16.sub(x14).mul(11585).r14();
    let x18 = x16.add(x14).mul(11585).r14();
    let x19 = x5.add(x15);
    let x20 = x5.sub(x15);
    let x21 = x7.add(x18);
    let x22 = x7.sub(x18);
    let x23 = x8.add(x17);
    let x24 = x8.sub(x17);
    let x25 = x6.add(x13);
    let x26 = x6.sub(x13);
    *t = [x19, x21, x23, x25, x26, x24, x22, x20];
}

/// The 16-point inverse DCT.
#[inline(always)]
pub fn idct16<L: Lane>(t: &mut [L; 16]) {
    let [i0, i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11, i12, i13, i14, i15] = *t;
    let mut even = [i0, i2, i4, i6, i8, i10, i12, i14];
    idct8(&mut even);
    let [x19, x21, x23, x25, x26, x24, x22, x20] = even;
    let x27 = i1.mul(1606).sub(i15.mul(16305)).r14();
    let x28 = i1.mul(16305).add(i15.mul(1606)).r14();
    let x29 = i9.mul(12665).sub(i7.mul(10394)).r14();
    let x30 = i9.mul(10394).add(i7.mul(12665)).r14();
    let x31 = i5.mul(7723).sub(i11.mul(14449)).r14();
    let x32 = i5.mul(14449).add(i11.mul(7723)).r14();
    let x33 = i13.mul(15679).sub(i3.mul(4756)).r14();
    let x34 = i13.mul(4756).add(i3.mul(15679)).r14();
    let x35 = x27.add(x29);
    let x36 = x27.sub(x29);
    let x37 = x33.add(x31);
    let x38 = x33.sub(x31);
    let x39 = x34.add(x32);
    let x40 = x34.sub(x32);
    let x41 = x28.add(x30);
    let x42 = x28.sub(x30);
    let x43 = x42.mul(6270).sub(x36.mul(15137)).r14();
    let x44 = x42.mul(15137).add(x36.mul(6270)).r14();
    let x45 = x38.mul(-15137).add(x40.mul(6270)).r14();
    let x46 = x38.mul(-6270).sub(x40.mul(15137)).r14();
    let x47 = x35.add(x37);
    let x48 = x35.sub(x37);
    let x49 = x41.add(x39);
    let x50 = x41.sub(x39);
    let x51 = x43.add(x46);
    let x52 = x43.sub(x46);
    let x53 = x44.add(x45);
    let x54 = x44.sub(x45);
    let x55 = x54.sub(x52).mul(11585).r14();
    let x56 = x54.add(x52).mul(11585).r14();
    let x57 = x50.sub(x48).mul(11585).r14();
    let x58 = x50.add(x48).mul(11585).r14();
    let x59 = x19.add(x49);
    let x60 = x19.sub(x49);
    let x61 = x21.add(x53);
    let x62 = x21.sub(x53);
    let x63 = x23.add(x56);
    let x64 = x23.sub(x56);
    let x65 = x25.add(x58);
    let x66 = x25.sub(x58);
    let x67 = x26.add(x57);
    let x68 = x26.sub(x57);
    let x69 = x24.add(x55);
    let x70 = x24.sub(x55);
    let x71 = x22.add(x51);
    let x72 = x22.sub(x51);
    let x73 = x20.add(x47);
    let x74 = x20.sub(x47);
    *t = [x59, x61, x63, x65, x67, x69, x71, x73, x74, x72, x70, x68, x66, x64, x62, x60];
}

/// The 32-point inverse DCT.
#[inline(always)]
pub fn idct32<L: Lane>(t: &mut [L; 32]) {
    let [i0, i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11, i12, i13, i14, i15, i16, i17, i18, i19, i20, i21, i22, i23, i24, i25, i26, i27, i28, i29, i30, i31] = *t;
    let mut even = [i0, i2, i4, i6, i8, i10, i12, i14, i16, i18, i20, i22, i24, i26, i28, i30];
    idct16(&mut even);
    let [x59, x61, x63, x65, x67, x69, x71, x73, x74, x72, x70, x68, x66, x64, x62, x60] = even;
    let x75 = i1.mul(804).sub(i31.mul(16364)).r14();
    let x76 = i1.mul(16364).add(i31.mul(804)).r14();
    let x77 = i17.mul(12140).sub(i15.mul(11003)).r14();
    let x78 = i17.mul(11003).add(i15.mul(12140)).r14();
    let x79 = i9.mul(7005).sub(i23.mul(14811)).r14();
    let x80 = i9.mul(14811).add(i23.mul(7005)).r14();
    let x81 = i25.mul(15426).sub(i7.mul(5520)).r14();
    let x82 = i25.mul(5520).add(i7.mul(15426)).r14();
    let x83 = i5.mul(3981).sub(i27.mul(15893)).r14();
    let x84 = i5.mul(15893).add(i27.mul(3981)).r14();
    let x85 = i21.mul(14053).sub(i11.mul(8423)).r14();
    let x86 = i21.mul(8423).add(i11.mul(14053)).r14();
    let x87 = i13.mul(9760).sub(i19.mul(13160)).r14();
    let x88 = i13.mul(13160).add(i19.mul(9760)).r14();
    let x89 = i29.mul(16207).sub(i3.mul(2404)).r14();
    let x90 = i29.mul(2404).add(i3.mul(16207)).r14();
    let x91 = x75.add(x77);
    let x92 = x75.sub(x77);
    let x93 = x81.add(x79);
    let x94 = x81.sub(x79);
    let x95 = x83.add(x85);
    let x96 = x83.sub(x85);
    let x97 = x89.add(x87);
    let x98 = x89.sub(x87);
    let x99 = x90.add(x88);
    let x100 = x90.sub(x88);
    let x101 = x84.add(x86);
    let x102 = x84.sub(x86);
    let x103 = x82.add(x80);
    let x104 = x82.sub(x80);
    let x105 = x76.add(x78);
    let x106 = x76.sub(x78);
    let x107 = x106.mul(3196).sub(x92.mul(16069)).r14();
    let x108 = x106.mul(16069).add(x92.mul(3196)).r14();
    let x109 = x98.mul(-9102).add(x100.mul(13623)).r14();
    let x110 = x98.mul(-13623).sub(x100.mul(9102)).r14();
    let x111 = x102.mul(13623).sub(x96.mul(9102)).r14();
    let x112 = x102.mul(9102).add(x96.mul(13623)).r14();
    let x113 = x94.mul(-16069).add(x104.mul(3196)).r14();
    let x114 = x94.mul(-3196).sub(x104.mul(16069)).r14();
    let x115 = x91.add(x93);
    let x116 = x91.sub(x93);
    let x117 = x97.add(x95);
    let x118 = x97.sub(x95);
    let x119 = x99.add(x101);
    let x120 = x99.sub(x101);
    let x121 = x105.add(x103);
    let x122 = x105.sub(x103);
    let x123 = x107.add(x114);
    let x124 = x107.sub(x114);
    let x125 = x110.add(x111);
    let x126 = x110.sub(x111);
    let x127 = x109.add(x112);
    let x128 = x109.sub(x112);
    let x129 = x108.add(x113);
    let x130 = x108.sub(x113);
    let x131 = x130.mul(6270).sub(x124.mul(15137)).r14();
    let x132 = x130.mul(15137).add(x124.mul(6270)).r14();
    let x133 = x126.mul(-15137).add(x128.mul(6270)).r14();
    let x134 = x126.mul(-6270).sub(x128.mul(15137)).r14();
    let x135 = x122.mul(6270).sub(x116.mul(15137)).r14();
    let x136 = x122.mul(15137).add(x116.mul(6270)).r14();
    let x137 = x118.mul(-15137).add(x120.mul(6270)).r14();
    let x138 = x118.mul(-6270).sub(x120.mul(15137)).r14();
    let x139 = x115.add(x117);
    let x140 = x115.sub(x117);
    let x141 = x121.add(x119);
    let x142 = x121.sub(x119);
    let x143 = x123.add(x125);
    let x144 = x123.sub(x125);
    let x145 = x129.add(x127);
    let x146 = x129.sub(x127);
    let x147 = x131.add(x134);
    let x148 = x131.sub(x134);
    let x149 = x132.add(x133);
    let x150 = x132.sub(x133);
    let x151 = x135.add(x138);
    let x152 = x135.sub(x138);
    let x153 = x136.add(x137);
    let x154 = x136.sub(x137);
    let x155 = x154.sub(x152).mul(11585).r14();
    let x156 = x154.add(x152).mul(11585).r14();
    let x157 = x150.sub(x148).mul(11585).r14();
    let x158 = x150.add(x148).mul(11585).r14();
    let x159 = x146.sub(x144).mul(11585).r14();
    let x160 = x146.add(x144).mul(11585).r14();
    let x161 = x142.sub(x140).mul(11585).r14();
    let x162 = x142.add(x140).mul(11585).r14();
    let x163 = x59.add(x141);
    let x164 = x59.sub(x141);
    let x165 = x61.add(x145);
    let x166 = x61.sub(x145);
    let x167 = x63.add(x149);
    let x168 = x63.sub(x149);
    let x169 = x65.add(x153);
    let x170 = x65.sub(x153);
    let x171 = x67.add(x156);
    let x172 = x67.sub(x156);
    let x173 = x69.add(x158);
    let x174 = x69.sub(x158);
    let x175 = x71.add(x160);
    let x176 = x71.sub(x160);
    let x177 = x73.add(x162);
    let x178 = x73.sub(x162);
    let x179 = x74.add(x161);
    let x180 = x74.sub(x161);
    let x181 = x72.add(x159);
    let x182 = x72.sub(x159);
    let x183 = x70.add(x157);
    let x184 = x70.sub(x157);
    let x185 = x68.add(x155);
    let x186 = x68.sub(x155);
    let x187 = x66.add(x151);
    let x188 = x66.sub(x151);
    let x189 = x64.add(x147);
    let x190 = x64.sub(x147);
    let x191 = x62.add(x143);
    let x192 = x62.sub(x143);
    let x193 = x60.add(x139);
    let x194 = x60.sub(x139);
    *t = [x163, x165, x167, x169, x171, x173, x175, x177, x179, x181, x183, x185, x187, x189, x191, x193, x194, x192, x190, x188, x186, x184, x182, x180, x178, x176, x174, x172, x170, x168, x166, x164];
}

/// The 8-point inverse ADST.
#[inline(always)]
pub fn iadst8<L: Lane>(t: &mut [L; 8]) {
    let [i0, i1, i2, i3, i4, i5, i6, i7] = *t;
    let s1 = i7.mul(1606).sub(i0.mul(16305));
    let s2 = i7.mul(16305).add(i0.mul(1606));
    let s3 = i5.mul(7723).sub(i2.mul(14449));
    let s4 = i5.mul(14449).add(i2.mul(7723));
    let s5 = i3.mul(12665).sub(i4.mul(10394));
    let s6 = i3.mul(10394).add(i4.mul(12665));
    let s7 = i1.mul(15679).sub(i6.mul(4756));
    let s8 = i1.mul(4756).add(i6.mul(15679));
    let x9 = s2.add(s6).r14();
    let x10 = s2.sub(s6).r14();
    let x11 = s1.add(s5).r14();
    let x12 = s1.sub(s5).r14();
    let x13 = s4.add(s8).r14();
    let x14 = s4.sub(s8).r14();
    let x15 = s3.add(s7).r14();
    let x16 = s3.sub(s7).r14();
    let s17 = x10.mul(6270).sub(x12.mul(15137));
    let s18 = x10.mul(15137).add(x12.mul(6270));
    let s19 = x16.mul(15137).sub(x14.mul(6270));
    let s20 = x16.mul(6270).add(x14.mul(15137));
    let x21 = s18.add(s19).r14();
    let x22 = s18.sub(s19).r14();
    let x23 = s17.add(s20).r14();
    let x24 = s17.sub(s20).r14();
    let x25 = x9.add(x13);
    let x26 = x9.sub(x13);
    let x27 = x11.add(x15);
    let x28 = x11.sub(x15);
    let x29 = x26.sub(x28).mul(11585).r14();
    let x30 = x26.add(x28).mul(11585).r14();
    let x31 = x22.sub(x24).mul(11585).r14();
    let x32 = x22.add(x24).mul(11585).r14();
    let x33 = x21.neg();
    let x34 = x30.neg();
    let x35 = x31.neg();
    let x36 = x27.neg();
    *t = [x25, x33, x32, x34, x29, x35, x23, x36];
}

/// The 16-point inverse ADST.
#[inline(always)]
pub fn iadst16<L: Lane>(t: &mut [L; 16]) {
    let [i0, i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11, i12, i13, i14, i15] = *t;
    let s1 = i15.mul(804).sub(i0.mul(16364));
    let s2 = i15.mul(16364).add(i0.mul(804));
    let s3 = i13.mul(3981).sub(i2.mul(15893));
    let s4 = i13.mul(15893).add(i2.mul(3981));
    let s5 = i11.mul(7005).sub(i4.mul(14811));
    let s6 = i11.mul(14811).add(i4.mul(7005));
    let s7 = i9.mul(9760).sub(i6.mul(13160));
    let s8 = i9.mul(13160).add(i6.mul(9760));
    let s9 = i7.mul(12140).sub(i8.mul(11003));
    let s10 = i7.mul(11003).add(i8.mul(12140));
    let s11 = i5.mul(14053).sub(i10.mul(8423));
    let s12 = i5.mul(8423).add(i10.mul(14053));
    let s13 = i3.mul(15426).sub(i12.mul(5520));
    let s14 = i3.mul(5520).add(i12.mul(15426));
    let s15 = i1.mul(16207).sub(i14.mul(2404));
    let s16 = i1.mul(2404).add(i14.mul(16207));
    let x17 = s2.add(s10).r14();
    let x18 = s2.sub(s10).r14();
    let x19 = s1.add(s9).r14();
    let x20 = s1.sub(s9).r14();
    let x21 = s4.add(s12).r14();
    let x22 = s4.sub(s12).r14();
    let x23 = s3.add(s11).r14();
    let x24 = s3.sub(s11).r14();
    let x25 = s6.add(s14).r14();
    let x26 = s6.sub(s14).r14();
    let x27 = s5.add(s13).r14();
    let x28 = s5.sub(s13).r14();
    let x29 = s8.add(s16).r14();
    let x30 = s8.sub(s16).r14();
    let x31 = s7.add(s15).r14();
    let x32 = s7.sub(s15).r14();
    let s33 = x18.mul(3196).sub(x20.mul(16069));
    let s34 = x18.mul(16069).add(x20.mul(3196));
    let s35 = x22.mul(13623).sub(x24.mul(9102));
    let s36 = x22.mul(9102).add(x24.mul(13623));
    let s37 = x26.mul(16069).add(x28.mul(3196));
    let s38 = x26.mul(-3196).add(x28.mul(16069));
    let s39 = x30.mul(9102).add(x32.mul(13623));
    let s40 = x30.mul(-13623).add(x32.mul(9102));
    let x41 = s34.add(s38).r14();
    let x42 = s34.sub(s38).r14();
    let x43 = s33.add(s37).r14();
    let x44 = s33.sub(s37).r14();
    let x45 = s36.add(s40).r14();
    let x46 = s36.sub(s40).r14();
    let x47 = s35.add(s39).r14();
    let x48 = s35.sub(s39).r14();
    let x49 = x17.add(x25);
    let x50 = x17.sub(x25);
    let x51 = x19.add(x27);
    let x52 = x19.sub(x27);
    let x53 = x21.add(x29);
    let x54 = x21.sub(x29);
    let x55 = x23.add(x31);
    let x56 = x23.sub(x31);
    let s57 = x50.mul(6270).sub(x52.mul(15137));
    let s58 = x50.mul(15137).add(x52.mul(6270));
    let s59 = x56.mul(15137).sub(x54.mul(6270));
    let s60 = x56.mul(6270).add(x54.mul(15137));
    let s61 = x42.mul(6270).sub(x44.mul(15137));
    let s62 = x42.mul(15137).add(x44.mul(6270));
    let s63 = x48.mul(15137).sub(x46.mul(6270));
    let s64 = x48.mul(6270).add(x46.mul(15137));
    let x65 = s58.add(s59).r14();
    let x66 = s58.sub(s59).r14();
    let x67 = s62.add(s63).r14();
    let x68 = s62.sub(s63).r14();
    let x69 = s57.add(s60).r14();
    let x70 = s57.sub(s60).r14();
    let x71 = s61.add(s64).r14();
    let x72 = s61.sub(s64).r14();
    let x73 = x49.add(x53);
    let x74 = x49.sub(x53);
    let x75 = x41.add(x45);
    let x76 = x41.sub(x45);
    let x77 = x51.add(x55);
    let x78 = x51.sub(x55);
    let x79 = x43.add(x47);
    let x80 = x43.sub(x47);
    let x81 = x74.add(x78).mul(-11585).r14();
    let x82 = x74.sub(x78).mul(11585).r14();
    let x83 = x66.add(x70).mul(11585).r14();
    let x84 = x66.sub(x70).mul(-11585).r14();
    let x85 = x76.add(x80).mul(11585).r14();
    let x86 = x76.sub(x80).mul(-11585).r14();
    let x87 = x68.add(x72).mul(-11585).r14();
    let x88 = x68.sub(x72).mul(11585).r14();
    let x89 = x75.neg();
    let x90 = x71.neg();
    let x91 = x65.neg();
    let x92 = x77.neg();
    *t = [x73, x89, x67, x91, x83, x87, x85, x81, x82, x86, x88, x84, x69, x90, x79, x92];
}

/// Add the inverse transform of one transform block to the prediction at
/// `dst` (8.6.2, 8.7.2), and leave `coef` zeroed for the next block.
/// `coef` holds the dequantised coefficients in raster order; `eob` is the
/// number read in scan order and `rows` how many leading rows hold any.
#[allow(clippy::too_many_arguments)]
pub fn reconstruct<P: Pixel>(coef: &mut [i32], tx_size: usize, tx_type: u8, lossless: bool, eob: usize, rows: usize, dst: &mut [P], stride: usize, bd: u32) {
    if lossless {
        wht_add(coef, dst, stride, bd);
    } else if eob == 1 && tx_type == DCT_DCT {
        if bd == 8 {
            dc_add::<i32, P>(coef, tx_size, dst, stride, bd);
        } else {
            dc_add::<i64, P>(coef, tx_size, dst, stride, bd);
        }
    } else {
        P::transform_add(coef, tx_size, tx_type, rows, dst, stride, bd);
    }
}

/// The shift of the residual's final rounding.
const SHIFT: [u32; 4] = [4, 5, 6, 6];

/// Only the DC coefficient: every row, then every column, is flat.
fn dc_add<C: Scalar, P: Pixel>(coef: &mut [i32], tx_size: usize, dst: &mut [P], stride: usize, bd: u32) {
    let n = 4 << tx_size;
    let d = C::from_i32(coef[0]).mul(11585).r14().mul(11585).r14().round_to_i32(SHIFT[tx_size]);
    coef[0] = 0;
    for row in dst.chunks_mut(stride).take(n) {
        for s in &mut row[..n] {
            *s = P::clip(s.get() + d, bd);
        }
    }
}

/// A transform block (not DC-only) in scalar arithmetic.
#[allow(clippy::too_many_arguments)]
pub fn scalar_transform_add<C: Scalar, P: Pixel>(coef: &mut [i32], tx_size: usize, tx_type: u8, rows: usize, dst: &mut [P], stride: usize, bd: u32) {
    let shift = SHIFT[tx_size];
    // rows (horizontal) take the ADST in DCT_ADST and ADST_ADST blocks,
    // columns (vertical) in ADST_DCT and ADST_ADST ones
    let row_adst = tx_type == DCT_ADST || tx_type == ADST_ADST;
    let col_adst = tx_type == ADST_DCT || tx_type == ADST_ADST;
    match tx_size {
        0 => two_d::<C, P, 4>(coef, rows, dst, stride, bd, shift, |t| if row_adst { iadst4(t) } else { idct4(t) }, |t| if col_adst { iadst4(t) } else { idct4(t) }),
        1 => two_d::<C, P, 8>(coef, rows, dst, stride, bd, shift, |t| if row_adst { iadst8(t) } else { idct8(t) }, |t| if col_adst { iadst8(t) } else { idct8(t) }),
        2 => two_d::<C, P, 16>(coef, rows, dst, stride, bd, shift, |t| if row_adst { iadst16(t) } else { idct16(t) }, |t| if col_adst { iadst16(t) } else { idct16(t) }),
        _ => two_d::<C, P, 32>(coef, rows, dst, stride, bd, shift, idct32, idct32),
    }
}

/// The row transforms, then the column transforms, in place (rows past
/// `rows` hold only zeros, and so transform to zeros), then the residual
/// added to the prediction row by row.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn two_d<C: Scalar, P: Pixel, const N: usize>(coef: &mut [i32], rows: usize, dst: &mut [P], stride: usize, bd: u32, shift: u32, row: impl Fn(&mut [C; N]), col: impl Fn(&mut [C; N])) {
    let rows = rows.min(N);
    let coef = &mut coef[..N * N];
    for r in coef.chunks_exact_mut(N).take(rows) {
        let mut t = [C::from_i32(0); N];
        for (t, &c) in t.iter_mut().zip(r.iter()) {
            *t = C::from_i32(c);
        }
        row(&mut t);
        for (o, v) in r.iter_mut().zip(t) {
            *o = v.to_i32();
        }
    }
    for j in 0..N {
        let mut t = [C::from_i32(0); N];
        for (i, v) in t.iter_mut().enumerate().take(rows) {
            *v = C::from_i32(coef[i * N + j]);
        }
        col(&mut t);
        for (i, v) in t.iter().enumerate() {
            coef[i * N + j] = v.round_to_i32(shift);
        }
    }
    for (drow, r) in dst.chunks_mut(stride).zip(coef.chunks_exact_mut(N)) {
        for (s, c) in drow[..N].iter_mut().zip(r) {
            *s = P::clip(s.get() + *c, bd);
            *c = 0;
        }
    }
}

/// The 8-bit transforms of four rows or columns at once, in 32-bit SIMD
/// lanes. They only pay where the lanes multiply natively: without SSE4.1,
/// `wide` multiplies them one at a time, so plain x86-64 keeps the scalar
/// transforms.
#[cfg(feature = "simd")]
pub mod simd {
    use super::*;
    use wide::i32x4;

    /// Whether the target multiplies 32-bit lanes natively.
    pub const NATIVE_MUL: bool = cfg!(any(target_feature = "sse4.1", target_feature = "simd128", all(target_arch = "aarch64", target_feature = "neon")));

    impl Lane for i32x4 {
        #[inline(always)]
        fn add(self, o: Self) -> Self {
            self + o
        }
        #[inline(always)]
        fn sub(self, o: Self) -> Self {
            self - o
        }
        #[inline(always)]
        fn neg(self) -> Self {
            i32x4::ZERO - self
        }
        #[inline(always)]
        fn mul(self, c: i32) -> Self {
            self * i32x4::splat(c)
        }
        #[inline(always)]
        fn r14(self) -> Self {
            (self + i32x4::splat(1 << 13)) >> 14_i32
        }
    }

    /// A transform block of an 8-bit frame (not DC-only).
    pub fn transform_add(coef: &mut [i32], tx_size: usize, tx_type: u8, rows: usize, dst: &mut [u8], stride: usize) {
        let shift = SHIFT[tx_size];
        let row_adst = tx_type == DCT_ADST || tx_type == ADST_ADST;
        let col_adst = tx_type == ADST_DCT || tx_type == ADST_ADST;
        match tx_size {
            0 => two_d::<4>(coef, rows, dst, stride, shift, |t| if row_adst { iadst4(t) } else { idct4(t) }, |t| if col_adst { iadst4(t) } else { idct4(t) }),
            1 => two_d::<8>(coef, rows, dst, stride, shift, |t| if row_adst { iadst8(t) } else { idct8(t) }, |t| if col_adst { iadst8(t) } else { idct8(t) }),
            2 => two_d::<16>(coef, rows, dst, stride, shift, |t| if row_adst { iadst16(t) } else { idct16(t) }, |t| if col_adst { iadst16(t) } else { idct16(t) }),
            _ => two_d::<32>(coef, rows, dst, stride, shift, idct32, idct32),
        }
    }

    #[inline(always)]
    fn load(s: &[i32]) -> i32x4 {
        i32x4::from([s[0], s[1], s[2], s[3]])
    }

    /// Rows four at a time (transposed into lanes and back, in place), then
    /// columns four at a time, added to the prediction.
    #[inline(always)]
    fn two_d<const N: usize>(coef: &mut [i32], rows: usize, dst: &mut [u8], stride: usize, shift: u32, row: impl Fn(&mut [i32x4; N]), col: impl Fn(&mut [i32x4; N])) {
        // whole groups of four rows: those past `rows` are zeros, and
        // transform to zeros
        let rows = rows.min(N).div_ceil(4) * 4;
        let coef = &mut coef[..N * N];
        for g in (0..rows).step_by(4) {
            let mut t = [i32x4::ZERO; N];
            for j in (0..N).step_by(4) {
                let v = i32x4::transpose([0, 1, 2, 3].map(|k| load(&coef[(g + k) * N + j..])));
                t[j..j + 4].copy_from_slice(&v);
            }
            row(&mut t);
            for j in (0..N).step_by(4) {
                let v = i32x4::transpose([t[j], t[j + 1], t[j + 2], t[j + 3]]);
                for (k, v) in v.iter().enumerate() {
                    coef[(g + k) * N + j..][..4].copy_from_slice(v.as_array_ref());
                }
            }
        }
        let round = i32x4::splat(1 << (shift - 1));
        for j in (0..N).step_by(4) {
            let mut t = [i32x4::ZERO; N];
            for (i, v) in t.iter_mut().enumerate().take(rows) {
                *v = load(&coef[i * N + j..]);
            }
            col(&mut t);
            for (i, v) in t.iter().enumerate() {
                let d = &mut dst[i * stride + j..][..4];
                let p = i32x4::from([d[0] as i32, d[1] as i32, d[2] as i32, d[3] as i32]);
                let s = (p + ((*v + round) >> shift)).max(i32x4::ZERO).min(i32x4::splat(255));
                for (d, s) in d.iter_mut().zip(s.as_array_ref()) {
                    *d = *s as u8;
                }
            }
        }
        coef[..rows * N].fill(0);
    }
}

/// The lossless 4x4 block: Walsh-Hadamard rows (with the 2-bit pre-scaling)
/// then columns, added without rounding.
fn wht_add<P: Pixel>(coef: &mut [i32], dst: &mut [P], stride: usize, bd: u32) {
    let mut tmp = [[0i32; 4]; 4];
    for (i, t) in tmp.iter_mut().enumerate() {
        *t = [coef[4 * i], coef[4 * i + 1], coef[4 * i + 2], coef[4 * i + 3]];
        iwht4(t, 2);
    }
    coef[..16].fill(0);
    for j in 0..4 {
        let mut t = [tmp[0][j], tmp[1][j], tmp[2][j], tmp[3][j]];
        iwht4(&mut t, 0);
        for (i, v) in t.iter().enumerate() {
            let s = &mut dst[i * stride + j];
            *s = P::clip(s.get().wrapping_add(*v), bd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outputs of the specification's transforms (computed with its
    /// butterfly description, which agrees with libvpx's).
    #[test]
    fn one_dimensional() {
        let mut t = [63, -292, 208, -502];
        idct4(&mut t);
        assert_eq!(t, [-270, 249, -455, 654]);
        let mut t = [-452, 497, -408, 148, 593, -482, 439, -161];
        idct8(&mut t);
        assert_eq!(t, [202, -354, -274, 81, 537, -80, -2248, -420]);
        let mut t = [-524, -424, 288, 256, -457, -108, -415, 528, 269, -479, 558, -347, -143, 593, -474, 581];
        idct16(&mut t);
        assert_eq!(t, [-605, -930, -223, -249, 251, -1435, -1614, -1389, -235, 334, -165, -2089, 3275, -249, -206, -399]);
        let mut t = [599, 212, -499, -148, -505, 540, -328, -7, 258, -305, 507, -359, 569, 31, 547, -230, -389, 591, 569, -216, 162, -401, 521, -472, 555, -478, -179, 416, 488, 275, 43, 353];
        idct32(&mut t);
        assert_eq!(t, [1091, -1372, -369, -675, 1807, -659, 1825, 106, 636, 851, 1898, 2130, -1250, 1697, 2796, 296, -10, -572, 2487, -1242, -750, 384, 429, 826, 176, 1879, 4433, -1367, -1077, -967, -3556, 1671]);
        let mut t = [599, 328, 140, 13];
        iadst4(&mut t);
        assert_eq!(t, [599, 574, 385, 369]);
        let mut t = [-92, -232, -101, -433, 576, 14, 475, 413];
        iadst8(&mut t);
        assert_eq!(t, [925, -1115, -365, 59, 371, -293, -896, 832]);
        let mut t: [i64; 16] = [103, 319, -11, -451, -359, 448, 256, -263, 100, -289, 401, 263, -520, -442, 542, 573];
        iadst16(&mut t);
        assert_eq!(t, [564, -658, 378, -1096, 1086, -704, 2949, 577, -453, 386, -1759, 508, 633, -47, -768, 210]);
        let mut t = [42, 96, 117, 417];
        iwht4(&mut t, 2);
        assert_eq!(t, [84, -50, 30, -45]);
        let mut t = [42, 96, 117, 417];
        iwht4(&mut t, 0);
        assert_eq!(t, [336, -198, 123, -177]);
    }

    /// The SIMD transforms agree with the scalar ones, overflowing or not.
    #[cfg(feature = "simd")]
    #[test]
    fn simd_matches_scalar() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for trial in 0..4000 {
            let tx_size = trial % 4;
            let n = 4 << tx_size;
            let tx_type = if tx_size == 3 { DCT_DCT } else { (rnd() % 4) as u8 };
            let rows = 1 + rnd() as usize % n;
            let range = [64, 2048, 1 << 20][trial % 3];
            let mut coef = vec![0i32; 32 * 32];
            for c in coef[..rows * n].iter_mut() {
                if rnd() % 3 == 0 {
                    *c = (rnd() % (2 * range)) as i32 - range as i32;
                }
            }
            let mut a: Vec<u8> = (0..n * n).map(|_| rnd() as u8).collect();
            let mut b = a.clone();
            let mut coef2 = coef.clone();
            scalar_transform_add::<i32, u8>(&mut coef, tx_size, tx_type, rows, &mut a, n, 8);
            simd::transform_add(&mut coef2, tx_size, tx_type, rows, &mut b, n);
            assert_eq!(a, b, "trial {trial}");
            assert!(coef.iter().chain(&coef2).all(|&c| c == 0));
        }
    }

    /// The DC-only shortcut agrees with the full transform.
    #[test]
    fn dc_only() {
        for tx_size in 0..4 {
            let n = 4 << tx_size;
            for dc in [-900, -37, 1, 55, 1200] {
                let mut a = vec![128u8; n * n];
                let mut b = a.clone();
                let mut coef = vec![0i32; n * n];
                coef[0] = dc;
                reconstruct(&mut coef, tx_size, DCT_DCT, false, 1, 1, &mut a, n, 8);
                assert!(coef.iter().all(|&c| c == 0));
                coef[0] = dc;
                reconstruct(&mut coef, tx_size, DCT_DCT, false, 2, 1, &mut b, n, 8);
                assert_eq!(a, b, "size {n} dc {dc}");
            }
        }
    }
}
