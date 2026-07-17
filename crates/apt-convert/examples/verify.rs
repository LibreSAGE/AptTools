// Parse each SWF and report bitmaps/fills/imports, so a conversion run can be
// checked without opening a window.
use swf::{FillStyle, Tag};
fn main() {
    let (mut files, mut bad, mut with_bmp) = (0, 0, 0);
    for p in std::env::args().skip(1) {
        files += 1;
        let Ok(data) = std::fs::read(&p) else {
            bad += 1;
            continue;
        };
        let Ok(buf) = swf::decompress_swf(&data[..]) else {
            println!("BAD {p}");
            bad += 1;
            continue;
        };
        let Ok(s) = swf::parse_swf(&buf) else {
            println!("BAD {p}");
            bad += 1;
            continue;
        };
        let (mut bmps, mut bfills, mut imports, mut exports) = (0, 0, 0, 0);
        let (mut clip_acts, mut buttons, mut etexts) = (0, 0, 0);
        fn count_place(tags: &[Tag], clip_acts: &mut i32) {
            for t in tags {
                match t {
                    Tag::PlaceObject(p) => {
                        if let Some(ca) = &p.clip_actions {
                            *clip_acts += ca.len() as i32
                        }
                    }
                    Tag::DefineSprite(sp) => count_place(&sp.tags, clip_acts),
                    _ => {}
                }
            }
        }
        count_place(&s.tags, &mut clip_acts);
        for t in &s.tags {
            match t {
                Tag::DefineButton2(_) => buttons += 1,
                Tag::DefineEditText(_) => etexts += 1,
                _ => {}
            }
        }
        for t in &s.tags {
            match t {
                Tag::DefineBitsLossless(_) => bmps += 1,
                Tag::ImportAssets { imports: i, .. } => imports += i.len(),
                Tag::ExportAssets(e) => exports += e.len(),
                Tag::DefineShape(sh) => {
                    bfills += sh
                        .styles
                        .fill_styles
                        .iter()
                        .filter(|f| matches!(f, FillStyle::Bitmap { .. }))
                        .count();
                }
                _ => {}
            }
        }
        if bmps > 0 {
            with_bmp += 1
        }
        let name = p.rsplit('/').next().unwrap_or(&p);
        if bmps > 0 || imports > 0 {
            println!("{name:34} bitmaps={bmps:<3} fills={bfills:<4} imports={imports:<3} exports={exports:<5} buttons={buttons:<3} etexts={etexts:<3} clip_actions={clip_acts}");
        }
    }
    println!("\n{files} SWFs, {bad} invalid, {with_bmp} with embedded bitmaps");
}
