//! Test fixture shared by the engine's unit tests and the interface's integration tests (included
//! with `#[path]`): no binary files in the repository.

/// A one-sheet Excel 97-2003 workbook ([MS-CFB] container, [MS-XLS] BIFF8 stream) built in code:
/// A1 = 42 and A2 = `A1*2` with its cached result.
pub fn xls_fixture() -> Vec<u8> {
    fn rec(id: u16, payload: &[u8]) -> Vec<u8> {
        [&id.to_le_bytes()[..], &(payload.len() as u16).to_le_bytes(), payload].concat()
    }
    let bof = |dt: u16| rec(0x0809, &[&0x0600u16.to_le_bytes()[..], &dt.to_le_bytes(), &[0; 12]].concat());
    let sheet = [
        bof(0x0010),
        rec(0x0203, &[&[0u8, 0, 0, 0, 0, 0][..], &42f64.to_le_bytes()].concat()),
        rec(0x0006, &[&[1u8, 0, 0, 0, 0, 0][..], &84f64.to_le_bytes(), &[0; 6], &[9, 0, 0x44, 0, 0, 0, 0xC0, 0x1E, 2, 0, 0x05]].concat()),
        rec(0x000A, &[]),
    ]
    .concat();
    let globals = |pos: u32| [bof(0x0005), rec(0x0085, &[&pos.to_le_bytes()[..], &[0, 0, 3, 0], b"Old"].concat()), rec(0x000A, &[])].concat();
    let stream = [globals(globals(0).len() as u32), sheet].concat();
    // Sectors: 0 FAT, 1 directory, 2 mini FAT, 3 mini stream container.
    let mut b = vec![0u8; 512 * 5];
    let mut put = |at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
    put(0, &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    put(26, &3u16.to_le_bytes());
    put(30, &9u16.to_le_bytes());
    put(32, &6u16.to_le_bytes());
    put(44, &1u32.to_le_bytes());
    put(48, &1u32.to_le_bytes());
    put(56, &4096u32.to_le_bytes());
    put(60, &2u32.to_le_bytes());
    put(64, &1u32.to_le_bytes());
    put(68, &0xFFFF_FFFEu32.to_le_bytes());
    for i in 1..109 {
        put(76 + i * 4, &0xFFFF_FFFFu32.to_le_bytes());
    }
    for (i, v) in [0xFFFF_FFFDu32, 0xFFFF_FFFE, 0xFFFF_FFFE, 0xFFFF_FFFE].iter().enumerate() {
        put(512 + i * 4, &v.to_le_bytes());
    }
    for i in 4..128 {
        put(512 + i * 4, &0xFFFF_FFFFu32.to_le_bytes());
    }
    let minis = stream.len().div_ceil(64);
    for i in 0..128 {
        let next = if i + 1 < minis {
            i as u32 + 1
        } else if i + 1 == minis {
            0xFFFF_FFFE
        } else {
            0xFFFF_FFFF
        };
        put(512 * 3 + i * 4, &next.to_le_bytes());
    }
    let entry = |k: usize, name: &str, kind: u8, child: u32, start: u32, size: u32, put: &mut dyn FnMut(usize, &[u8])| {
        let at = 512 * 2 + k * 128;
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        put(at, &units);
        put(at + 64, &(units.len() as u16 + 2).to_le_bytes());
        put(at + 66, &[kind, 1]);
        put(at + 68, &[0xFF; 8]);
        put(at + 76, &child.to_le_bytes());
        put(at + 116, &start.to_le_bytes());
        put(at + 120, &size.to_le_bytes());
    };
    entry(0, "Root Entry", 5, 1, 3, 512, &mut put);
    entry(1, "Workbook", 2, 0xFFFF_FFFF, 0, stream.len() as u32, &mut put);
    put(512 * 4, &stream);
    b
}
