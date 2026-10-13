//! Persisted delta checksums must remain IEEE CRC-32 across dependency/CPU changes.

fn reference(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[test]
fn persisted_checksums_match_independent_reference() {
    assert_eq!(reference(b"123456789"), 0xcbf4_3926);
    assert_eq!(reference(b""), 0);
    let mut lengths: Vec<usize> = (0..=260).collect();
    for boundary in [512, 1024, 1536, 2048, 2176, 2304, 4096, 8192, 65536] {
        lengths.extend([boundary - 1, boundary, boundary + 1]);
    }
    let mut state = 0x1234_5678_u32;
    let bytes: Vec<u8> = (0..65600)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    for offset in [0, 1, 3, 7, 15, 31] {
        for &len in &lengths {
            let input = &bytes[offset..offset + len];
            assert_eq!(
                crc32fast::hash(input),
                reference(input),
                "len={len} offset={offset}"
            );
        }
    }
}
