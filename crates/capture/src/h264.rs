//! What an H.264 stream says of itself in its parameter sets, for the
//! readers of files (`mp4read`, and `media`'s probe).

/// The colour matrix from an H.264 SPS (with its NAL header), if its VUI
/// has a colour description.
pub fn sps_matrix(nal: &[u8]) -> Option<u8> {
    // Without the emulation-prevention bytes (00 00 03 → 00 00).
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal.get(1..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { data: &rbsp, at: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bits(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.bits(1)?;
        if r.bits(1)? == 1 {
            // Scaling matrices: rarely there; not worth walking.
            return None;
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bits(1)?;
    r.ue()?;
    r.ue()?;
    if r.bits(1)? == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?;
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.bits(1)? == 0 {
        return None; // no VUI
    }
    if r.bits(1)? == 1 {
        // aspect_ratio_info: 255 is extended (two 16-bit numbers).
        if r.bits(8)? == 255 {
            r.bits(32)?;
        }
    }
    if r.bits(1)? == 1 {
        r.bits(1)?; // overscan
    }
    if r.bits(1)? == 0 {
        return None; // no video signal type
    }
    r.bits(3)?;
    r.bits(1)?;
    if r.bits(1)? == 0 {
        return None; // no colour description
    }
    r.bits(8)?;
    r.bits(8)?;
    Some(r.bits(8)? as u8)
}

/// Reads bits, most significant first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// x264's SPSs (High, emulation-prevention bytes and all) as ffprobe
    /// reads their colour: BT.709, BT.601 (SMPTE 170M), and not said.
    #[test]
    fn colour_matrix_from_the_sps() {
        assert_eq!(sps_matrix(&hex("67640033acb4014005ad80b502020140000003004000001e03c60ca8")), Some(1));
        assert_eq!(sps_matrix(&hex("6764002aacb403c0113f2e02d4080819000003000100000300780f1832a0")), Some(6));
        assert_eq!(sps_matrix(&hex("67640033acb4014005ad08000003000800000303c078c195")), None);
        assert_eq!(sps_matrix(&[0x67]), None, "cut short");
    }
}
