//! Tests against the bundled fonts.

use alloc::string::String;
use alloc::vec::Vec;
use std::println;

use crate::*;

pub(crate) const INTER: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.otf");
pub(crate) const INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.otf");
pub(crate) const LATO: &[u8] = include_bytes!("../../../assets/fonts/Lato-Regular.ttf");
pub(crate) const LATO_BOLD: &[u8] = include_bytes!("../../../assets/fonts/Lato-Bold.ttf");
pub(crate) const JBM: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
pub(crate) const JBM_BOLD: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf");

pub(crate) const ALL: [(&str, &[u8]); 6] = [
    ("Inter-Regular", INTER),
    ("Inter-SemiBold", INTER_SEMIBOLD),
    ("Lato-Regular", LATO),
    ("Lato-Bold", LATO_BOLD),
    ("JetBrainsMono-Regular", JBM),
    ("JetBrainsMono-Bold", JBM_BOLD),
];

#[test]
fn explore() {
    for (name, data) in ALL {
        let f = Font::from_bytes(data).unwrap();
        let m = f.metrics();
        println!(
            "{name}: glyphs {} upem {} fmt {:?} family {:?} sub {:?} full {:?} kern {} mono {}",
            f.num_glyphs(),
            f.units_per_em(),
            f.outline_format(),
            f.family_name(),
            f.subfamily_name(),
            f.full_name(),
            f.has_kerning(),
            m.is_monospace
        );
        println!("   {m:?}");
        let gi = |c| f.glyph_index(c);
        let k = |a, b| f.kerning(gi(a).unwrap(), gi(b).unwrap());
        println!("   AV {} To {} Te {} VA {} Ta {}", k('A', 'V'), k('T', 'o'), k('T', 'e'), k('V', 'A'), k('T', 'a'));
        println!("   euro {:?} emdash {:?}", gi('€'), gi('—'));
        let _ = String::new();
        let _: Vec<u8> = Vec::new();
    }
}
