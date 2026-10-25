// SPDX-License-Identifier: Apache-2.0
//! The tuning strategies' draws against a stream printed by Abseil itself (`data/uniform-stream.txt`):
//! per seed, a mix of `Uniform<size_t>`, `Uniform<int>`, `Uniform<float>(0, 1)` and `Bernoulli`, then
//! the engine's next raw word, so a draw that consumes the wrong number of words fails too.
use vyges_rmp::rng::Mt19937;

#[test]
fn every_draw_and_the_word_after_them_match_the_printed_stream() {
    let text = include_str!("data/uniform-stream.txt");
    let mut rng: Option<(i64, Mt19937)> = None;
    let mut checked = 0;
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let seed: i64 = w[1].parse().unwrap();
        if rng.as_ref().map(|r| r.0) != Some(seed) {
            rng = Some((seed, Mt19937::new(seed as u32)));
        }
        let g = &mut rng.as_mut().unwrap().1;
        match w[0] {
            "size_t" => assert_eq!(g.uniform_usize(w[2].parse().unwrap()).to_string(), w[3], "{line}"),
            "int" => assert_eq!(g.uniform_i32(w[2].parse().unwrap()).to_string(), w[3], "{line}"),
            "float" => assert_eq!(format!("{:08x}", g.uniform_f32().to_bits()), w[3], "{line}"),
            "bernoulli" => assert_eq!(u8::from(g.bernoulli(w[2].parse().unwrap())).to_string(), w[3], "{line}"),
            "next" => assert_eq!(g.word().to_string(), w[2], "{line}"),
            other => panic!("unknown draw {other}"),
        }
        checked += 1;
    }
    assert_eq!(checked, 245);
}
