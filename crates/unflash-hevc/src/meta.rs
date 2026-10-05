//! What decoding a picture records per 4x4 luma block, per coding tree
//! block and per slice segment: what later blocks predict from, and what
//! the in-loop filters and the temporal prediction of later pictures need.

/// Per 4x4 block flags.
pub const INTRA: u8 = 1;
/// cu_skip_flag.
pub const SKIP: u8 = 2;
/// cu_transquant_bypass_flag.
pub const BYPASS: u8 = 4;
/// pcm_flag.
pub const PCM: u8 = 8;
/// In a luma transform block with non-zero coefficients.
pub const CODED: u8 = 16;

/// Per 4x4 block edge flags: the block's left / top edge is a transform
/// block or prediction block boundary.
pub const TU_LEFT: u8 = 1;
pub const TU_TOP: u8 = 2;
pub const PU_LEFT: u8 = 4;
pub const PU_TOP: u8 = 8;

/// The motion of a prediction block: per list the vector and the
/// reference index (−1 when the list is not used; the vector is then 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Motion {
    pub mv: [[i16; 2]; 2],
    pub ref_idx: [i8; 2],
}

impl Motion {
    /// No motion: an intra (or not yet decoded) block.
    pub const NONE: Motion = Motion { mv: [[0; 2]; 2], ref_idx: [-1, -1] };

    #[inline]
    pub fn uses(&self, list: usize) -> bool {
        self.ref_idx[list] >= 0
    }
}

/// A reference picture as a slice's list sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefKey {
    pub poc: i32,
    pub long_term: bool,
    /// The picture's id (which picture, for the deblocking filter).
    pub id: u32,
}

/// What the loop filters and temporal prediction need of a slice segment.
#[derive(Clone, Debug)]
pub struct SliceInfo {
    /// SliceAddrRs.
    pub addr: u32,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    pub refs: [Vec<RefKey>; 2],
}

/// Sample adaptive offset parameters of one coding tree block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaoParams {
    /// SaoTypeIdx per component: 0 off, 1 band offset, 2 edge offset.
    pub kind: [u8; 3],
    /// sao_band_position or SaoEoClass per component.
    pub class: [u8; 3],
    /// SaoOffsetVal[1..=4] per component, scaled.
    pub offsets: [[i16; 4]; 3],
}

/// Marks a coding tree block no slice has covered.
pub const NO_SLICE: u16 = u16::MAX;

/// The per-picture records: per 4x4 block, per coding tree block and per
/// slice segment.
pub struct Meta {
    /// 4x4 blocks per row and column.
    pub w4: usize,
    pub h4: usize,
    pub flags: Vec<u8>,
    pub edges: Vec<u8>,
    /// QpY.
    pub qp: Vec<i8>,
    /// CtDepth.
    pub depth: Vec<u8>,
    /// IntraPredModeY.
    pub ipm: Vec<u8>,
    pub motion: Vec<Motion>,
    /// Per coding tree block (raster order): the index of its slice
    /// segment in `slices`.
    pub ctb_slice: Vec<u16>,
    pub sao: Vec<SaoParams>,
    pub slices: Vec<SliceInfo>,
}

impl Meta {
    pub fn new() -> Meta {
        Meta { w4: 0, h4: 0, flags: Vec::new(), edges: Vec::new(), qp: Vec::new(), depth: Vec::new(), ipm: Vec::new(), motion: Vec::new(), ctb_slice: Vec::new(), sao: Vec::new(), slices: Vec::new() }
    }

    /// Prepare for a picture of the given size (per-block data is written
    /// by every coding unit before it is read, so only the per-CTB state is
    /// cleared).
    pub fn reset(&mut self, width: usize, height: usize, ctbs: usize) {
        let (w4, h4) = (width.div_ceil(4), height.div_ceil(4));
        let n = w4 * h4;
        if self.w4 != w4 || self.h4 != h4 {
            self.w4 = w4;
            self.h4 = h4;
            self.flags = vec![0; n];
            self.edges = vec![0; n];
            self.qp = vec![0; n];
            self.depth = vec![0; n];
            self.ipm = vec![1; n];
            self.motion = vec![Motion::NONE; n];
        }
        self.ctb_slice.clear();
        self.ctb_slice.resize(ctbs, NO_SLICE);
        self.sao.clear();
        self.sao.resize(ctbs, SaoParams::default());
        self.slices.clear();
    }

    /// Set a byte map over the 4x4 blocks of a rectangle (in luma samples).
    #[inline]
    pub fn fill<T: Copy>(map: &mut [T], w4: usize, x: usize, y: usize, w: usize, h: usize, v: T) {
        let (x4, y4, w4b, h4b) = (x >> 2, y >> 2, w.div_ceil(4), h.div_ceil(4));
        for r in y4..y4 + h4b {
            map[r * w4 + x4..r * w4 + x4 + w4b].fill(v);
        }
    }

    #[inline]
    pub fn at(&self, x: usize, y: usize) -> usize {
        (y >> 2) * self.w4 + (x >> 2)
    }
}

impl Default for Meta {
    fn default() -> Self {
        Self::new()
    }
}
